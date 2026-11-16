//! One bounded checked-texture worker, independent of the terminal renderer.

// Rust guideline compliant 2026-10-07
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{io, num::NonZeroU64};

use vivid_protocol::identity::SurfaceIdentity;
use vivid_sdk::vector_renderer::compositor::texture::{
    Completed, SharedDevice, TexturePool, TextureRenderer,
};
use vivid_sdk::vector_renderer::compositor::{Drawing, RenderMetrics, WorkBudget};

// A pending GPU future is polled cooperatively; idle workers wake only for
// retirement/cancellation checks. Neither interval controls terminal frame rate.
const POLL: Duration = Duration::from_millis(1);
const IDLE: Duration = Duration::from_millis(50);
// Pool retirement uses the SDK's existing ten-second per-job bound too.
const WAIT: Duration = Duration::from_secs(10);
// Match one checked output's attempt allowance before starting another probe.
// A final in-flight probe retains its own 64-attempt SDK limit (127 total).
const PROBE_PASSES: u64 = 64;

#[derive(Debug, Default)]
pub(super) struct Control {
    cancelled: AtomicBool,
    diagnosed: AtomicBool,
    advising: AtomicBool,
    admitted: AtomicBool,
    advice: Mutex<Option<Advice>>,
}
#[derive(Clone, Debug)]
pub(super) struct Advice {
    pub retained: Vec<SurfaceIdentity>,
    budget: WorkBudget,
    started: Instant,
}
impl Control {
    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub(super) fn diagnosed(&self) -> bool {
        self.diagnosed.load(Ordering::Acquire)
    }
    pub(super) fn admit(&self) {
        self.admitted.store(true, Ordering::Release);
    }
    pub(super) fn advice(&self) -> Option<Advice> {
        if !self.advising.load(Ordering::Acquire) {
            return None;
        }
        self.advice
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .filter(|a| {
                a.budget.spent() > 0
                    && a.budget.spent() < PROBE_PASSES
                    && a.started.elapsed() < WAIT
            })
            .cloned()
    }
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[cfg(any(test, feature = "test-util"))]
#[derive(Default)]
pub(super) struct Gate {
    #[cfg(test)]
    pub held: AtomicBool,
    #[cfg(test)]
    pub entered: AtomicBool,
    #[cfg(test)]
    pub expire_completed: AtomicBool,
    pub diagnosing: AtomicBool,
    pub presentations: std::sync::atomic::AtomicU64,
    pub spent: std::sync::atomic::AtomicU64,
}

struct Gpu {
    shared: SharedDevice,
    pool: TexturePool,
    device: wgpu::Device,
    queue: wgpu::Queue,
    #[cfg(any(test, feature = "test-util"))]
    gate: Arc<Gate>,
}

struct Request {
    control: Arc<Control>,
    drawings: Vec<Drawing>,
    committed: Vec<Drawing>,
    budget: Option<WorkBudget>,
    expires: Option<Instant>,
    width: u32,
    height: u32,
}

pub(super) struct Response {
    pub control: Arc<Control>,
    pub result: io::Result<Output>,
    pub metrics: RenderMetrics,
}

pub(super) enum Output {
    Image(Completed),
    Refused { window: SurfaceIdentity, survivors: Option<Completed>, witness: Option<Arc<Control>> },
}

fn work_refusal(error: &io::Error) -> bool {
    use vivid_sdk::vector_renderer::compositor::Error;
    matches!(
        error.get_ref().and_then(|e| e.downcast_ref::<Error>()),
        Some(Error::Arena(_) | Error::WorkLimit)
    )
}

pub(super) struct Worker {
    commands: Option<mpsc::SyncSender<Request>>,
    results: mpsc::Receiver<Response>,
    stopping: Arc<AtomicBool>,
    pending: Option<Arc<Control>>,
    peer: Option<Arc<Control>>,
    _thread: JoinHandle<()>,
}
impl Worker {
    pub fn new(
        shared: SharedDevice,
        pool: TexturePool,
        device: wgpu::Device,
        queue: wgpu::Queue,
        #[cfg(any(test, feature = "test-util"))] gate: Arc<Gate>,
    ) -> io::Result<Self> {
        let (commands, requests) = mpsc::sync_channel(1);
        let (completed, results) = mpsc::sync_channel(1);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = thread::Builder::new().name("Vivido.overlay".into()).spawn(move || {
            run(
                Gpu {
                    shared,
                    pool,
                    device,
                    queue,
                    #[cfg(any(test, feature = "test-util"))]
                    gate,
                },
                &requests,
                &completed,
                &stop,
            );
        })?;
        Ok(Self {
            commands: Some(commands),
            results,
            stopping,
            pending: None,
            peer: None,
            _thread: thread,
        })
    }

    pub fn start(
        &mut self,
        drawings: Vec<Drawing>,
        width: u32,
        height: u32,
        committed: Vec<Drawing>,
    ) -> io::Result<Arc<Control>> {
        if self.pending.is_some() || self.peer.is_some() {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let control = Arc::new(Control::default());
        self.commands
            .as_ref()
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?
            .try_send(Request {
                control: control.clone(),
                drawings,
                committed,
                budget: None,
                expires: None,
                width,
                height,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => io::Error::from(io::ErrorKind::WouldBlock),
                mpsc::TrySendError::Disconnected(_) => io::Error::from(io::ErrorKind::BrokenPipe),
            })?;
        self.pending = Some(control.clone());
        Ok(control)
    }

    pub fn start_peer(
        &mut self,
        drawings: Vec<Drawing>,
        width: u32,
        height: u32,
    ) -> io::Result<Arc<Control>> {
        if self.peer.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let advice = self
            .pending
            .as_ref()
            .and_then(|c| c.advice())
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
        let control = Arc::new(Control::default());
        self.commands
            .as_ref()
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?
            .try_send(Request {
                control: control.clone(),
                drawings,
                committed: Vec::new(),
                budget: Some(advice.budget),
                expires: Some(advice.started + WAIT),
                width,
                height,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => io::Error::from(io::ErrorKind::WouldBlock),
                mpsc::TrySendError::Disconnected(_) => io::Error::from(io::ErrorKind::BrokenPipe),
            })?;
        self.peer = Some(control.clone());
        Ok(control)
    }
    pub fn advice(&self) -> Option<Advice> {
        self.pending.as_ref()?.advice()
    }
    #[cfg(all(feature = "test-util", target_os = "macos"))]
    pub(super) fn probe_passes_for_check(&self) -> Option<u64> {
        self.advice().map(|advice| advice.budget.spent())
    }
    pub fn peer_busy(&self) -> bool {
        self.peer.is_some()
    }
    pub fn poll(&mut self) -> io::Result<Option<Response>> {
        match self.results.try_recv() {
            Ok(response) => {
                if self.pending.as_ref().is_some_and(|c| Arc::ptr_eq(c, &response.control)) {
                    self.pending = None;
                } else if self.peer.as_ref().is_some_and(|c| Arc::ptr_eq(c, &response.control)) {
                    self.peer = None;
                } else {
                    return Err(io::Error::other("overlay worker response identity mismatch"));
                }
                Ok(Some(response))
            },
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    pub fn busy(&self) -> bool {
        self.pending.is_some() || self.peer.is_some()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(pending) = &self.pending {
            pending.cancel();
        }
        if let Some(peer) = &self.peer {
            peer.cancel();
        }
        self.commands.take();
        // Detach instead of joining on the UI thread. The worker cancels its
        // own !Send future; the SDK retains GPU charges behind queue fences.
    }
}

fn run(
    gpu: Gpu,
    requests: &mpsc::Receiver<Request>,
    completed: &mpsc::SyncSender<Response>,
    stopping: &AtomicBool,
) {
    let mut renderer: Option<TextureRenderer> = None;
    let private_pool = TexturePool::default();
    let mut private: Option<TextureRenderer> = None;
    let mut retired = RenderMetrics::default();
    while !stopping.load(Ordering::Acquire) {
        let request = if renderer.as_ref().is_some_and(TextureRenderer::is_busy)
            || private.as_ref().is_some_and(TextureRenderer::is_busy)
        {
            match requests.recv_timeout(IDLE) {
                Ok(request) => request,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(renderer) = &mut renderer {
                        let _ = renderer.poll();
                    }
                    if let Some(renderer) = &mut private {
                        let _ = renderer.poll();
                    }
                    continue;
                },
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match requests.recv() {
                Ok(request) => request,
                Err(_) => break,
            }
        };
        #[cfg(any(test, feature = "test-util"))]
        gpu.gate.diagnosing.store(false, Ordering::Release);
        let result = if let Some(budget) = &request.budget {
            render_inner(
                &request,
                &request.drawings,
                &gpu,
                &mut renderer,
                stopping,
                Some(budget),
                &mut || Ok(false),
            )
            .map(Output::Image)
        } else {
            match render(&request, &request.drawings, &gpu, &mut renderer, stopping) {
                Ok(image) => Ok(Output::Image(image)),
                Err(error)
                    if work_refusal(&error)
                        && renderer.as_ref().is_some_and(|r| !r.is_failed()) =>
                {
                    diagnose(
                        &request,
                        &gpu,
                        &mut renderer,
                        &mut private,
                        &private_pool,
                        Dispatch { requests, completed, stopping, retired },
                        error,
                    )
                },
                Err(error) => Err(error),
            }
        };
        request.control.advising.store(false, Ordering::Release);
        #[cfg(any(test, feature = "test-util"))]
        gpu.gate.diagnosing.store(false, Ordering::Release);
        let metrics = super::add(retired, metrics(&renderer, &private));
        if renderer.as_ref().is_some_and(TextureRenderer::is_failed) {
            retired = super::add(retired, renderer.take().expect("failed renderer").metrics());
        }
        if private.as_ref().is_some_and(TextureRenderer::is_failed) {
            retired =
                super::add(retired, private.take().expect("failed private renderer").metrics());
        }
        if stopping.load(Ordering::Acquire) {
            break;
        }
        if completed.try_send(Response { control: request.control, result, metrics }).is_err() {
            break;
        }
    }
    drop(renderer);
    drop(private);
    // Poll the old device on this retiring worker, including after a host
    // rebuild moves onto another device. Never wait on the terminal thread.
    let (finished, done) = mpsc::channel();
    gpu.queue.on_submitted_work_done(move || {
        let _ = finished.send(());
    });
    let until = Instant::now() + WAIT;
    while matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)) && Instant::now() < until {
        if gpu.device.poll(wgpu::PollType::Poll).is_err() {
            break;
        }
        thread::sleep(POLL);
    }
}

fn render(
    request: &Request,
    drawings: &[Drawing],
    gpu: &Gpu,
    renderer: &mut Option<TextureRenderer>,
    stopping: &AtomicBool,
) -> io::Result<Completed> {
    render_inner(request, drawings, gpu, renderer, stopping, None, &mut || Ok(false))
}

fn render_inner(
    request: &Request,
    drawings: &[Drawing],
    gpu: &Gpu,
    renderer: &mut Option<TextureRenderer>,
    stopping: &AtomicBool,
    budget: Option<&WorkBudget>,
    service: &mut dyn FnMut() -> io::Result<bool>,
) -> io::Result<Completed> {
    let start = Instant::now();
    let cancelled = || {
        request.control.cancelled()
            || stopping.load(Ordering::Acquire)
            || request.expires.is_some_and(|deadline| Instant::now() >= deadline)
    };
    let job = loop {
        if cancelled() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        if start.elapsed() >= WAIT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "overlay texture retirement timed out",
            ));
        }
        if renderer.is_none() {
            match TextureRenderer::new(&gpu.shared, &gpu.pool) {
                Ok(value) => *renderer = Some(value),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    gpu.device.poll(wgpu::PollType::Poll).map_err(io::Error::other)?;
                    thread::sleep(POLL);
                    continue;
                },
                Err(error) => return Err(error),
            }
        }
        let renderer = renderer.as_mut().expect("initialized overlay worker");
        renderer.poll()?;
        let started = if let Some(budget) = budget {
            renderer.start_budgeted(drawings, &[], request.width, request.height, budget)
        } else {
            renderer.start(drawings, &[], request.width, request.height)
        };
        match started {
            Ok(job) => break job,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(error) => return Err(error),
        }
    };
    // This exact SDK JobId belongs to this opaque request and its host-held
    // reservation. Diagnosis may service a peer using the separate public
    // renderer; an ordinary command cannot replace this job's reservation.
    let renderer = renderer.as_mut().expect("started overlay worker");
    loop {
        if cancelled() {
            renderer.cancel();
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        if start.elapsed() >= WAIT {
            renderer.cancel();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "overlay texture job timed out"));
        }
        let diagnostic_poll = match service() {
            Ok(diagnostic) => diagnostic,
            Err(error) => {
                renderer.cancel();
                return Err(error);
            },
        };
        let completed = renderer.poll()?;
        let expired = start.elapsed() >= WAIT;
        #[cfg(test)]
        let expired =
            expired || (completed.is_some() && gpu.gate.expire_completed.load(Ordering::Acquire));
        if cancelled() || expired {
            renderer.cancel();
            return Err(if expired {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "overlay texture completed after its deadline",
                )
            } else {
                io::Error::from(io::ErrorKind::Interrupted)
            });
        }
        if let Some(completed) = completed {
            if completed.job != job {
                return Err(io::Error::other("overlay texture job identity mismatch"));
            }
            return Ok(completed);
        }
        #[cfg(test)]
        if gpu.gate.held.load(Ordering::Acquire) {
            gpu.gate.entered.store(true, Ordering::Release);
            while gpu.gate.held.load(Ordering::Acquire) {
                if cancelled() || start.elapsed() >= WAIT {
                    renderer.cancel();
                    return Err(if cancelled() {
                        io::Error::from(io::ErrorKind::Interrupted)
                    } else {
                        io::Error::new(io::ErrorKind::TimedOut, "paused overlay texture timed out")
                    });
                }
                thread::sleep(POLL);
            }
        }
        // A diagnostic yields between native attempts so the UI can admit a
        // checked peer and coalesce its next candidate. Ordinary jobs retain
        // the existing one-millisecond polling/cancellation interval.
        if diagnostic_poll {
            thread::sleep(POLL * 5);
        } else {
            // Wake on native completion instead of adding a full timer interval
            // after it. This worker-only wait remains bounded by one poll slice;
            // timeout means the job is still pending, not a source/device fault.
            match gpu
                .device
                .poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(POLL) })
            {
                Ok(_) | Err(wgpu::PollError::Timeout) => (),
                Err(error) => {
                    renderer.cancel();
                    return Err(io::Error::other(error));
                },
            }
        }
    }
}

fn same(a: &Drawing, b: &Drawing) -> bool {
    a.window == b.window
        && a.window_revision == b.window_revision
        && a.submission == b.submission
        && a.bounds == b.bounds
        && a.scale.to_bits() == b.scale.to_bits()
        && Arc::ptr_eq(&a.compiled, &b.compiled)
}

fn metrics(public: &Option<TextureRenderer>, private: &Option<TextureRenderer>) -> RenderMetrics {
    let a = public.as_ref().map_or_else(RenderMetrics::default, TextureRenderer::metrics);
    let b = private.as_ref().map_or_else(RenderMetrics::default, TextureRenderer::metrics);
    RenderMetrics {
        render_passes: a.render_passes.saturating_add(b.render_passes),
        skipped_passes: a.skipped_passes.saturating_add(b.skipped_passes),
        target_allocations: a.target_allocations.saturating_add(b.target_allocations),
        allocation_retries: a.allocation_retries.saturating_add(b.allocation_retries),
    }
}

struct Dispatch<'a> {
    requests: &'a mpsc::Receiver<Request>,
    completed: &'a mpsc::SyncSender<Response>,
    stopping: &'a AtomicBool,
    retired: RenderMetrics,
}

fn diagnose(
    request: &Request,
    gpu: &Gpu,
    public: &mut Option<TextureRenderer>,
    private: &mut Option<TextureRenderer>,
    private_pool: &TexturePool,
    dispatch: Dispatch<'_>,
    original: io::Error,
) -> io::Result<Output> {
    let Dispatch { requests, completed, stopping, retired } = dispatch;
    #[cfg(any(test, feature = "test-util"))]
    {
        gpu.gate.diagnosing.store(true, Ordering::Release);
        gpu.gate.presentations.store(0, Ordering::Release);
        gpu.gate.spent.store(0, Ordering::Release);
    }
    request.control.diagnosed.store(true, Ordering::Release);
    let started = Instant::now();
    let budget = WorkBudget::new(NonZeroU64::new(127).expect("nonzero diagnostic bound"));
    let mut unknown: Vec<_> = request
        .drawings
        .iter()
        .filter(|d| !request.committed.iter().any(|old| same(d, old)))
        .map(|d| d.window)
        .collect();
    let private_before =
        private.as_ref().map_or_else(RenderMetrics::default, TextureRenderer::metrics);
    let before = super::add(retired, metrics(public, private));
    let mut latest: Option<Request> = None;
    let mut refused = None;
    if request.drawings.len() == 1 {
        refused = Some(request.drawings[0].window);
    } else {
        if private.is_none() {
            *private = Some(TextureRenderer::new(&gpu.shared, private_pool)?);
        }
        // Separate targets/caches, same device/queue/health. No private image is
        // imported, sampled by the terminal or allowed to settle a receipt.
        let private_gpu = Gpu {
            shared: gpu.shared.clone(),
            pool: private_pool.clone(),
            device: gpu.device.clone(),
            queue: gpu.queue.clone(),
            #[cfg(any(test, feature = "test-util"))]
            gate: gpu.gate.clone(),
        };
        for window in unknown.clone() {
            if budget.spent() >= PROBE_PASSES || started.elapsed() >= WAIT {
                break;
            }
            *request.control.advice.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(Advice { retained: unknown.clone(), budget: budget.clone(), started });
            request.control.advising.store(true, Ordering::Release);
            let drawing =
                request.drawings.iter().find(|d| d.window == window).expect("diagnostic drawing");
            let probe = render_inner(
                request,
                std::slice::from_ref(drawing),
                &private_gpu,
                private,
                stopping,
                Some(&budget),
                &mut || {
                    let peer = match requests.try_recv() {
                        Ok(peer) => peer,
                        Err(mpsc::TryRecvError::Empty) => return Ok(true),
                        Err(mpsc::TryRecvError::Disconnected) => {
                            return Err(io::ErrorKind::Interrupted.into());
                        },
                    };
                    let result = if let Some(peer_budget) = &peer.budget {
                        render_inner(
                            &peer,
                            &peer.drawings,
                            gpu,
                            public,
                            stopping,
                            Some(peer_budget),
                            &mut || Ok(false),
                        )
                        .map(Output::Image)
                    } else {
                        return Err(io::Error::other(
                            "ordinary request entered diagnostic handoff",
                        ));
                    };
                    let succeeded = result.is_ok();
                    let mut current_metrics =
                        super::add(retired, super::add(metrics(public, &None), private_before));
                    current_metrics.render_passes =
                        before.render_passes.saturating_add(budget.spent());
                    completed
                        .try_send(Response {
                            control: peer.control.clone(),
                            result,
                            metrics: current_metrics,
                        })
                        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
                    // Wait only on this worker. The UI and terminal keep polling;
                    // no probe can retire a source ahead of the ordered peer image.
                    let until = Instant::now() + WAIT;
                    while succeeded
                        && !peer.control.admitted.load(Ordering::Acquire)
                        && !peer.control.cancelled()
                    {
                        if request.control.cancelled()
                            || stopping.load(Ordering::Acquire)
                            || started.elapsed() >= WAIT
                            || Instant::now() >= until
                        {
                            return Err(io::ErrorKind::Interrupted.into());
                        }
                        gpu.device.poll(wgpu::PollType::Poll).map_err(io::Error::other)?;
                        thread::sleep(POLL);
                    }
                    if succeeded && peer.control.admitted.load(Ordering::Acquire) {
                        #[cfg(any(test, feature = "test-util"))]
                        gpu.gate.presentations.fetch_add(1, Ordering::AcqRel);
                        latest = Some(peer);
                    }
                    Ok(true)
                },
            );
            #[cfg(any(test, feature = "test-util"))]
            gpu.gate.spent.store(budget.spent(), Ordering::Release);
            match probe {
                Ok(_) => unknown.retain(|id| *id != window),
                Err(error)
                    if work_refusal(&error) && private.as_ref().is_some_and(|r| !r.is_failed()) =>
                {
                    refused = Some(window);
                    break;
                },
                Err(error) => return Err(error),
            }
        }
    }
    request.control.advising.store(false, Ordering::Release);
    let Some(window) = refused else { return Err(original) };
    let latest_drawings =
        latest.as_ref().map_or(request.drawings.as_slice(), |p| p.drawings.as_slice());
    let survivors: Vec<_> =
        latest_drawings.iter().filter(|d| d.window != window).cloned().collect();
    let survivors = match render(request, &survivors, gpu, public, stopping) {
        Ok(image) => Some(image),
        Err(error) if work_refusal(&error) && public.as_ref().is_some_and(|r| !r.is_failed()) => {
            None
        },
        Err(error) => return Err(error),
    };
    Ok(Output::Refused { window, survivors, witness: latest.map(|p| p.control) })
}
