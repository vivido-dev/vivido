//! Checked overlay output, independently retained display images and exact frame admission.

// Rust guideline compliant 2026-10-07
mod worker;

use std::{io, sync::Arc};

use vello::{Renderer, Scene, peniko::ImageData};
use vivid_sdk::vector_renderer::compositor::texture::{SharedDevice, TextureImport, TexturePool};
use vivid_sdk::vector_renderer::compositor::{Drawing, RenderMetrics};

use crate::vivid::{overlay::Snapshot, scene::SharedScene};
pub(crate) use vivid_sdk::vector_renderer::compositor::RenderMetrics as OverlayRenderMetrics;
use worker::{Control, Output, Worker};

struct Pending {
    control: Arc<Control>,
    snapshot: Arc<Snapshot>,
    size: (u32, u32),
}
struct Image {
    control: Arc<Control>,
    snapshot: Arc<Snapshot>,
    import: TextureImport,
    size: (u32, u32),
}
struct Failed {
    snapshot: Arc<Snapshot>,
    size: (u32, u32),
}

pub(super) struct OverlayRenderer {
    pub scene: Option<SharedScene>,
    shared: SharedDevice,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pool: TexturePool,
    worker: Option<Worker>,
    pending: Option<Pending>,
    peer_pending: Option<Pending>,
    ready: Option<Image>,
    displayed: Option<Image>,
    failed: Option<Failed>,
    metrics: RenderMetrics,
    base: RenderMetrics,
    skipped: u64,
    observed: bool,
    rescan: bool,
    #[cfg(any(test, feature = "test-util"))]
    gate: Arc<worker::Gate>,
    pub generation: u64,
}
impl OverlayRenderer {
    pub fn new(shared: SharedDevice, device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        Self {
            scene: None,
            shared,
            device: device.clone(),
            queue: queue.clone(),
            pool: TexturePool::default(),
            worker: None,
            pending: None,
            peer_pending: None,
            ready: None,
            displayed: None,
            failed: None,
            metrics: RenderMetrics::default(),
            base: RenderMetrics::default(),
            skipped: 0,
            observed: false,
            rescan: false,
            #[cfg(any(test, feature = "test-util"))]
            gate: Arc::default(),
            generation: 0,
        }
    }
    #[cfg(all(feature = "test-util", target_os = "macos"))]
    pub(super) fn stream_state_for_check(&self) -> (bool, u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.gate.diagnosing.load(Ordering::Acquire),
            self.gate.presentations.load(Ordering::Acquire),
            self.gate.spent.load(Ordering::Acquire),
        )
    }
    #[cfg(all(feature = "test-util", target_os = "macos"))]
    pub(super) fn probe_passes_for_check(&self) -> Option<u64> {
        self.worker.as_ref()?.probe_passes_for_check()
    }
    #[cfg(all(feature = "test-util", target_os = "macos"))]
    pub(super) fn stream_frame_for_check(&self) -> Option<(u64, u64, [[u32; 2]; 2])> {
        let shown = self.displayed.as_ref()?;
        let peer = shown.snapshot.drawings().iter().find(|d| {
            d.bounds.width.get() == 56.
                && d.bounds.height.get() == 56.
                && d.bounds.origin.x.get() > 0.
        })?;
        let coordinates = [
            [80. * peer.scale, 120. * peer.scale],
            [
                (peer.bounds.origin.x.get() + 28.) * peer.scale,
                (peer.bounds.origin.y.get() + 28.) * peer.scale,
            ],
        ];
        let mut points = [[0; 2]; 2];
        for (out, point) in points.iter_mut().zip(coordinates) {
            for (axis, bound) in [shown.size.0, shown.size.1].into_iter().enumerate() {
                if !point[axis].is_finite()
                    || point[axis] < 0.
                    || point[axis].round() >= f64::from(bound)
                {
                    return None;
                }
                out[axis] = point[axis].round() as u32;
            }
        }
        Some((peer.submission.revision, self.generation, points))
    }
    #[cfg(test)]
    pub fn hold_for_test(&self, hold: bool) {
        use std::sync::atomic::Ordering;
        if hold {
            self.gate.entered.store(false, Ordering::Release);
        }
        self.gate.held.store(hold, Ordering::Release);
    }
    #[cfg(test)]
    pub fn held_for_test(&self) -> bool {
        self.gate.entered.load(std::sync::atomic::Ordering::Acquire)
    }
    #[cfg(test)]
    pub fn diagnosing_for_test(&self) -> bool {
        self.worker.as_ref().is_some_and(Worker::busy)
            && self.gate.diagnosing.load(std::sync::atomic::Ordering::Acquire)
    }
    #[cfg(test)]
    pub fn diagnostic_stream_for_test(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        (self.gate.presentations.load(Ordering::Acquire), self.gate.spent.load(Ordering::Acquire))
    }
    #[cfg(test)]
    pub fn expire_completed_for_test(&self, expire: bool) {
        self.gate.expire_completed.store(expire, std::sync::atomic::Ordering::Release);
    }
    #[cfg(test)]
    pub fn allocations_for_test(&self) -> usize {
        self.pool.reserved()
    }
    pub fn pool(&self) -> TexturePool {
        self.pool.clone()
    }
    pub fn set_pool(&mut self, pool: TexturePool) {
        self.pool = pool;
    }
    pub fn metrics(&self) -> OverlayRenderMetrics {
        let mut metrics = self.metrics;
        metrics.skipped_passes = metrics.skipped_passes.saturating_add(self.skipped);
        metrics
    }
    pub fn needs_frame(&self) -> bool {
        self.rescan || self.worker.as_ref().is_some_and(Worker::busy) || self.ready.is_some()
    }
    fn abandon(&self, snapshot: &Arc<Snapshot>) {
        if let Some(scene) = &self.scene {
            scene
                .overlays()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .finish(scene, snapshot, false);
        }
    }
    fn valid(&self, snapshot: &Arc<Snapshot>, reserved: bool) -> bool {
        let Some(scene) = &self.scene else {
            return false;
        };
        let host = scene.overlays().lock().unwrap_or_else(|p| p.into_inner());
        if reserved { host.admits(scene, snapshot) } else { host.current(scene, snapshot) }
    }
    fn valid_pending(&self, pending: &Pending) -> bool {
        if !pending.control.diagnosed() {
            return self.valid(&pending.snapshot, true);
        }
        self.scene.as_ref().is_some_and(|scene| {
            scene
                .overlays()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .diagnosing(scene, &pending.snapshot)
        })
    }
    fn remove_peer(&mut self) {
        if let Some(peer) = self.peer_pending.take() {
            peer.control.cancel();
            self.abandon(&peer.snapshot);
        }
    }
    fn changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
    fn remove_ready(&mut self, renderer: &mut Renderer) {
        if let Some(image) = self.ready.take() {
            image.control.cancel();
            self.abandon(&image.snapshot);
            image.import.unregister(renderer);
            self.changed();
        }
    }
    fn remove_displayed(&mut self, renderer: &mut Renderer) {
        if let Some(image) = self.displayed.take() {
            image.import.unregister(renderer);
            self.changed();
        }
    }
    pub fn clear(&mut self, renderer: &mut Renderer) {
        self.rescan = false;
        if let Some(pending) = self.pending.take() {
            pending.control.cancel();
            self.abandon(&pending.snapshot);
        }
        if let Some(peer) = self.peer_pending.take() {
            peer.control.cancel();
            self.abandon(&peer.snapshot);
        }
        self.worker = None;
        self.remove_ready(renderer);
        self.remove_displayed(renderer);
        self.failed = None;
        self.changed();
    }

    /// Publishes only the imported image actually referenced by the composed scene.
    pub fn finish_frame(&mut self, frame: &Scene, renderer: &mut Renderer) {
        let Some(ready) = &self.ready else {
            return;
        };
        let Some(image) = ready.import.image() else {
            return;
        };
        let id = image.data.id();
        let sampled = frame.encoding().resources.patches.iter().any(|patch| {
            matches!(patch, vello::low_level::vello_encoding::Patch::Image { image, .. } if image.data.id() == id)
        });
        if !sampled {
            return;
        }
        let image = self.ready.take().expect("sampled overlay image");
        let admitted = self.scene.as_ref().is_some_and(|scene| {
            scene.overlays().lock().unwrap_or_else(|p| p.into_inner()).finish(
                scene,
                &image.snapshot,
                true,
            )
        });
        if admitted {
            image.control.admit();
            if let Some(previous) = self.displayed.take() {
                previous.import.unregister(renderer);
            }
            self.displayed = Some(image);
        } else {
            image.control.cancel();
            image.import.unregister(renderer);
            self.changed();
        }
        // Source wakes may have been coalesced while this image was pending.
        // Recheck once after admission; unchanged content returns to idle.
        self.rescan = true;
    }

    pub fn draw(
        &mut self,
        renderer: &mut Renderer,
        width: u32,
        height: u32,
    ) -> io::Result<(Option<ImageData>, bool)> {
        let previous = self.generation;
        self.rescan = false;
        let size = (width, height);
        if self.ready.as_ref().is_some_and(|i| i.size != size || !self.valid(&i.snapshot, true)) {
            self.remove_ready(renderer);
        }
        if self
            .displayed
            .as_ref()
            .is_some_and(|i| i.size != size || !self.valid(&i.snapshot, false))
        {
            self.remove_displayed(renderer);
        }
        if self.pending.as_ref().is_some_and(|p| p.size != size || !self.valid_pending(p)) {
            let pending = self.pending.take().expect("stale overlay request");
            pending.control.cancel();
            self.abandon(&pending.snapshot);
        }
        if self
            .peer_pending
            .as_ref()
            .is_some_and(|p| p.size != size || !self.valid(&p.snapshot, true))
        {
            self.remove_peer();
        }
        if let Some(worker) = &mut self.worker {
            let response = worker.poll();
            match response {
                Ok(Some(response)) => {
                    self.metrics = add(self.base, response.metrics);
                    let is_peer = self
                        .peer_pending
                        .as_ref()
                        .is_some_and(|p| Arc::ptr_eq(&p.control, &response.control));
                    let pending = if is_peer {
                        self.peer_pending.take()
                    } else if self
                        .pending
                        .as_ref()
                        .is_some_and(|p| Arc::ptr_eq(&p.control, &response.control))
                    {
                        self.pending.take()
                    } else {
                        None
                    };
                    if let Some(pending) = pending {
                        if !Arc::ptr_eq(&pending.control, &response.control) {
                            self.abandon(&pending.snapshot);
                            return Err(io::Error::other("overlay reservation response mismatch"));
                        }
                        match response.result {
                            Ok(Output::Image(completed))
                                if pending.size == size
                                    && completed.texture.dimensions() == size
                                    && self.valid(&pending.snapshot, true) =>
                            {
                                self.ready = Some(Image {
                                    control: pending.control,
                                    snapshot: pending.snapshot,
                                    import: completed.texture.register(renderer),
                                    size,
                                });
                                self.changed();
                            },
                            Ok(Output::Image(_)) => {
                                pending.control.cancel();
                                self.abandon(&pending.snapshot);
                            },
                            Ok(Output::Refused { window, survivors, witness }) => {
                                self.remove_peer();
                                let survivor_frame = self
                                    .displayed
                                    .as_ref()
                                    .filter(|image| {
                                        witness.as_ref().is_some_and(|control| {
                                            Arc::ptr_eq(control, &image.control)
                                        })
                                    })
                                    .map_or_else(
                                        || pending.snapshot.clone(),
                                        |image| image.snapshot.clone(),
                                    );
                                let snapshot = self.scene.as_ref().and_then(|scene| {
                                    let mut host =
                                        scene.overlays().lock().unwrap_or_else(|p| p.into_inner());
                                    let refused =
                                        host.reject_drawing(scene, &pending.snapshot, window);
                                    // Preserve the original peer receipt before releasing
                                    // its failed mixed reservation. Later accepted bytes
                                    // must not relabel already-checked survivor pixels.
                                    let snapshot = refused
                                        .then(|| {
                                            host.reserve_survivors(scene, &survivor_frame, window)
                                        })
                                        .flatten();
                                    host.finish(scene, &pending.snapshot, false);
                                    snapshot
                                });
                                if let Some(snapshot) = snapshot {
                                    if let Some(completed) = survivors
                                        && pending.size == size
                                        && completed.texture.dimensions() == size
                                        && snapshot.survivors(&survivor_frame, window)
                                        && self.valid(&snapshot, true)
                                    {
                                        self.ready = Some(Image {
                                            control: pending.control,
                                            snapshot,
                                            import: completed.texture.register(renderer),
                                            size,
                                        });
                                        self.changed();
                                    } else {
                                        self.abandon(&snapshot);
                                    }
                                }
                                // A closed owner's pixels cannot remain sampled. When the
                                // checked survivor image still matches, it replaces them in
                                // this same draw without blanking the healthy peer first.
                                if self
                                    .displayed
                                    .as_ref()
                                    .is_some_and(|i| !self.valid(&i.snapshot, false))
                                {
                                    self.remove_displayed(renderer);
                                }
                            },
                            Err(error) => {
                                pending.control.cancel();
                                self.abandon(&pending.snapshot);
                                if error.kind() != io::ErrorKind::Interrupted {
                                    log::warn!("checked overlay rendering failed: {error}");
                                    self.failed = Some(Failed {
                                        snapshot: pending.snapshot,
                                        size: pending.size,
                                    });
                                }
                            },
                        }
                    }
                },
                Ok(None) => (),
                Err(error) => {
                    if let Some(pending) = self.pending.take() {
                        self.abandon(&pending.snapshot);
                    }
                    self.remove_peer();
                    self.worker = None;
                    return Err(error);
                },
            }
        }
        if !self.worker.as_ref().is_some_and(Worker::busy) && self.ready.is_none() {
            self.start(size)?;
        } else if self.ready.is_none() && self.peer_pending.is_none() {
            self.start_peer(size)?;
        }
        let image =
            self.ready.as_ref().or(self.displayed.as_ref()).and_then(|i| i.import.image()).cloned();
        Ok((image, previous != self.generation))
    }

    fn start_peer(&mut self, size: (u32, u32)) -> io::Result<()> {
        let Some(worker) = self.worker.as_ref().filter(|w| !w.peer_busy()) else {
            return Ok(());
        };
        let Some(advice) = worker.advice() else {
            return Ok(());
        };
        let Some(displayed) = self.displayed.as_ref().filter(|i| i.size == size) else {
            return Ok(());
        };
        let Some(scene) = &self.scene else {
            return Ok(());
        };
        let Some(snapshot) = scene
            .overlays()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .prepare_retaining(scene, &displayed.snapshot, &advice.retained)
        else {
            return Ok(());
        };
        if snapshot.same_content(&displayed.snapshot)
            || !self.valid(&snapshot, true)
            || self
                .failed
                .as_ref()
                .is_some_and(|f| f.size == size && f.snapshot.same_content(&snapshot))
        {
            self.abandon(&snapshot);
            return Ok(());
        }
        match self.worker.as_mut().expect("diagnostic worker").start_peer(
            drawings(&snapshot),
            size.0,
            size.1,
        ) {
            Ok(control) => self.peer_pending = Some(Pending { control, snapshot, size }),
            Err(error) => {
                self.abandon(&snapshot);
                if error.kind() != io::ErrorKind::WouldBlock {
                    return Err(error);
                }
            },
        }
        Ok(())
    }

    fn start(&mut self, size: (u32, u32)) -> io::Result<()> {
        let Some(scene) = &self.scene else {
            return Ok(());
        };
        let Some(snapshot) =
            scene.overlays().lock().unwrap_or_else(|p| p.into_inner()).prepare(scene)
        else {
            return Ok(());
        };
        let unchanged = self
            .displayed
            .as_ref()
            .is_some_and(|i| i.size == size && i.snapshot.same_content(&snapshot));
        let failed = self
            .failed
            .as_ref()
            .is_some_and(|i| i.size == size && i.snapshot.same_content(&snapshot));
        let empty = snapshot.drawings().is_empty() && self.displayed.is_none() && !self.observed;
        if unchanged || failed || empty || !self.valid(&snapshot, true) {
            self.abandon(&snapshot);
            self.skipped = self.skipped.saturating_add(1);
            return Ok(());
        }
        if self.worker.is_none() {
            self.base = self.metrics;
            self.worker = match Worker::new(
                self.shared.clone(),
                self.pool.clone(),
                self.device.clone(),
                self.queue.clone(),
                #[cfg(any(test, feature = "test-util"))]
                self.gate.clone(),
            ) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    self.abandon(&snapshot);
                    return Err(error);
                },
            };
        }
        let candidate = snapshot
            .drawings()
            .iter()
            .map(|d| Drawing {
                window: d.window,
                window_revision: d.window_revision,
                bounds: d.bounds,
                scale: d.scale,
                compiled: d.compiled.clone(),
                submission: d.submission,
            })
            .collect();
        match self.worker.as_mut().expect("initialized overlay worker").start(
            candidate,
            size.0,
            size.1,
            self.displayed.as_ref().map_or_else(Vec::new, |image| drawings(&image.snapshot)),
        ) {
            Ok(control) => {
                self.observed = true;
                self.failed = None;
                self.pending = Some(Pending { control, snapshot, size });
            },
            Err(error) => {
                self.abandon(&snapshot);
                return Err(error);
            },
        }
        Ok(())
    }
}
impl Drop for OverlayRenderer {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.control.cancel();
            self.abandon(&pending.snapshot);
        }
        self.remove_peer();
        if let Some(ready) = self.ready.take() {
            ready.control.cancel();
            self.abandon(&ready.snapshot);
        }
        // SceneRenderer unregisters imports with its original renderer before
        // dropping this state; no worker join or shared-device destruction here.
    }
}

fn add(a: RenderMetrics, b: RenderMetrics) -> RenderMetrics {
    RenderMetrics {
        render_passes: a.render_passes.saturating_add(b.render_passes),
        skipped_passes: a.skipped_passes.saturating_add(b.skipped_passes),
        target_allocations: a.target_allocations.saturating_add(b.target_allocations),
        allocation_retries: a.allocation_retries.saturating_add(b.allocation_retries),
    }
}

fn drawings(snapshot: &Snapshot) -> Vec<Drawing> {
    snapshot
        .drawings()
        .iter()
        .map(|d| Drawing {
            window: d.window,
            window_revision: d.window_revision,
            bounds: d.bounds,
            scale: d.scale,
            compiled: d.compiled.clone(),
            submission: d.submission,
        })
        .collect()
}
