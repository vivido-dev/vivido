//! Per-window Vivid Protocol endpoint, session manager, and media dispatch.

mod audio;
mod decoder;
pub mod scene;
mod transport;

use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(unix)]
use std::fs;
use std::io::{self, Cursor, ErrorKind};
#[cfg(windows)]
use std::net::{Ipv4Addr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use image::GenericImageView;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use vivid_protocol::anchor::{self, AnchorKey};
use vivid_protocol::media::{self, VIDEO_PACKET_KEY};
use vivid_protocol::messages::{self, DisplayChanged};
use vivid_protocol::revision::{ObservationSequence, SceneRevision, SourceRevision};
use vivid_protocol::trace::{TraceComponent, TraceGuard, TraceHop};
use vivid_protocol::wire::{ConnectionKind, RECORD_OPTIONAL, Record};
use vivid_protocol::{VIVID_MAJOR, VIVID_MINOR};

use crate::event::{EventProxy, EventType};
use crate::terminal::event::EventListener;
use crate::terminal::grid::Dimensions;
use crate::terminal::index::{Column, Line, Point};
use crate::terminal::term::{ResizePoint, Term};
use crate::vivid::audio::AudioOutput;
use crate::vivid::decoder::{DecodedFrame, Decoder};
use crate::vivid::scene::{
    Frame, MediaBarrierWait, SceneMutation, SceneNode, SessionId, SessionObservationSnapshot,
    SharedScene, SourceConfig, SourceKey, SourceObservation, SourceWaitEvaluation,
};
use crate::vivid::transport::{Reader, TraceChannel, Writer};

#[cfg(windows)]
type LocalListener = TcpListener;
#[cfg(windows)]
type LocalStream = TcpStream;
#[cfg(unix)]
type LocalListener = UnixListener;
#[cfg(unix)]
type LocalStream = UnixStream;

const INITIAL_BYTE_CREDITS: u64 = 4 * 1024 * 1024;
const INITIAL_PACKET_CREDITS: u64 = 32;
const MAX_SESSIONS: usize = 16;
const MAX_CONNECTIONS: usize = 64;
const MAX_PENDING_OPERATIONS: usize = 64;
const MAX_TRANSACTIONS: usize = 64;
const MAX_REGISTERED_WAITS: usize = 64;
const MAX_IDEMPOTENCY_ENTRIES: usize = 256;
const MAX_CONTEXT_CAPABILITIES: usize = 256;
const MAX_PENDING_REQUESTS: usize = MAX_PENDING_OPERATIONS + MAX_REGISTERED_WAITS;
const MAX_OBSERVATION_QUEUE: usize = 64;
const MAX_OBSERVATIONS_PER_TICK: usize = 8;
const PENDING_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const MEDIA_ORDER_BARRIER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(10);

fn root_context_quotas() -> messages::ContextQuotas {
    messages::ContextQuotas {
        maximum_sources: 64,
        maximum_nodes: messages::MAX_SCENE_NODES as u64,
        maximum_retained_pixels: 8192 * 8192 * 2,
        maximum_media_bytes: 256 * 1024 * 1024,
        maximum_media_connections: MAX_CONNECTIONS as u64,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DisplayMetrics {
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub columns: u32,
    pub rows: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingDisplayChange {
    metrics: DisplayMetrics,
    last_unsettled_generation: Option<u64>,
}

impl PendingDisplayChange {
    fn event(&mut self, settled: bool) -> Option<DisplayChanged> {
        if !settled && self.last_unsettled_generation == Some(self.metrics.generation) {
            return None;
        }
        if !settled {
            self.last_unsettled_generation = Some(self.metrics.generation);
        }
        Some(DisplayChanged {
            display_generation: self.metrics.generation,
            viewport_width: self.metrics.viewport_width,
            viewport_height: self.metrics.viewport_height,
            grid_columns: self.metrics.columns,
            grid_rows: self.metrics.rows,
            cell_width: self.metrics.cell_width,
            cell_height: self.metrics.cell_height,
            settled,
        })
    }
}

#[derive(Clone)]
struct Ticket {
    session_id: SessionId,
    source_key: SourceKey,
    kind: ConnectionKind,
}

struct SessionRuntime {
    writer: Weak<Writer>,
    tag: [u8; 16],
    anchor_key: AnchorKey,
    seen_anchors: HashSet<u64>,
    last_visibility: HashMap<u64, bool>,
    accepted_features: HashSet<u64>,
    authority_root_session: SessionId,
    bound_context_id: u64,
    context_class_mask: u64,
    context_quotas: messages::ContextQuotas,
    active_media_connections: u64,
    revoked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ContextKey {
    authority_root_session: SessionId,
    context_id: u64,
}

struct ContextEntry {
    parent: Option<ContextKey>,
    class_mask: u64,
    quotas: messages::ContextQuotas,
    _label: String,
    expires_at: Option<Instant>,
    revoked: bool,
}

struct CapabilityBinding {
    verifier: [u8; 32],
    context: ContextKey,
    class_mask: u64,
    quotas: messages::ContextQuotas,
    expires_at: Option<Instant>,
}

#[derive(Default)]
struct Registry {
    next_session_id: u64,
    sessions: HashMap<SessionId, SessionRuntime>,
    tickets: HashMap<Vec<u8>, Ticket>,
    contexts: HashMap<ContextKey, ContextEntry>,
    capabilities: Vec<CapabilityBinding>,
}

struct ServiceShared {
    token: [u8; 32],
    scene: SharedScene,
    registry: Mutex<Registry>,
    metrics: Mutex<DisplayMetrics>,
    pending_display_change: Mutex<Option<PendingDisplayChange>>,
    capability_generation: AtomicU64,
    audio_device_available: AtomicBool,
    active_connections: AtomicUsize,
    audio_outputs: Mutex<HashMap<SourceKey, Arc<AudioOutput>>>,
    /// Last `(renderable, display_offset)` reported by the UI thread. Cached so scene changes
    /// applied on the control-dispatcher thread (e.g. a newly committed node) can recompute
    /// source visibility without the UI-thread inputs directly at hand.
    render_state: Mutex<(bool, usize)>,
    wake: Arc<dyn Fn() + Send + Sync>,
    trace: Option<vivid_protocol::trace::TraceEmitter>,
    _trace_guard: Option<TraceGuard>,
}

struct PendingOperation {
    object_id: u64,
    deadline: Instant,
}

#[derive(Debug, Clone, Copy)]
struct RegisteredWait {
    source_id: u64,
    condition: u64,
    value: Option<u64>,
    deadline: Instant,
}

#[derive(Debug, Clone, Copy)]
enum IdempotencyOutcome {
    Ok { object_id: u64 },
    Presented { scene_revision: SceneRevision },
    SourceCreated { source_id: u64 },
}

#[derive(Debug, Clone, Copy)]
struct IdempotencyEntry {
    request_hash: [u8; 32],
    outcome: IdempotencyOutcome,
}

#[derive(Debug)]
enum PreconditionError {
    Malformed(&'static str),
    Failed { kind: u64, detail: messages::ErrorDetail },
}

#[derive(Debug, Clone, Copy)]
enum QueuedObservation {
    Source {
        source_id: u64,
        source_revision: SourceRevision,
        changed_fields: u64,
        sequence: ObservationSequence,
        causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
    },
    Scene {
        scene_revision: SceneRevision,
        reason_mask: u64,
        sequence: ObservationSequence,
        causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
    },
    Playback {
        source_id: u64,
        snapshot: messages::PlaybackSnapshot,
        source_revision: SourceRevision,
        sequence: ObservationSequence,
        causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
    },
}

impl QueuedObservation {
    fn class(self) -> u64 {
        match self {
            Self::Source { .. } => messages::OBSERVE_SOURCE_TRANSITIONS,
            Self::Scene { .. } => messages::OBSERVE_SCENE_CHANGES,
            Self::Playback { .. } => messages::OBSERVE_PLAYBACK_TRANSITIONS,
        }
    }

    fn sequence(self) -> ObservationSequence {
        match self {
            Self::Source { sequence, .. }
            | Self::Scene { sequence, .. }
            | Self::Playback { sequence, .. } => sequence,
        }
    }

    fn coalesces(self, other: Self) -> bool {
        match (self, other) {
            (Self::Source { source_id: left, .. }, Self::Source { source_id: right, .. })
            | (Self::Playback { source_id: left, .. }, Self::Playback { source_id: right, .. }) => {
                left == right
            },
            (Self::Scene { .. }, Self::Scene { .. }) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
struct ObservationTracker {
    mask: u64,
    sequence: ObservationSequence,
    scene_revision: SceneRevision,
    sources: HashMap<u64, (SourceObservation, Option<messages::PlaybackSnapshot>)>,
    queue: VecDeque<QueuedObservation>,
    source_gap: Option<ObservationSequence>,
    scene_gap: Option<ObservationSequence>,
    source_causation: HashMap<u64, [u8; messages::CAUSATION_ID_BYTES]>,
    scene_causation: Option<[u8; messages::CAUSATION_ID_BYTES]>,
}

#[derive(Default)]
struct PendingOperations {
    entries: Mutex<HashMap<u64, PendingOperation>>,
}

#[derive(Debug)]
enum PendingRegisterError {
    Full,
    Duplicate,
}

impl PendingOperations {
    fn register(
        &self,
        request_id: u64,
        object_id: u64,
        timeout: Duration,
    ) -> Result<(), PendingRegisterError> {
        let mut entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if entries.contains_key(&request_id) {
            return Err(PendingRegisterError::Duplicate);
        }
        if entries.len() >= MAX_PENDING_OPERATIONS {
            return Err(PendingRegisterError::Full);
        }
        entries
            .insert(request_id, PendingOperation { object_id, deadline: Instant::now() + timeout });
        Ok(())
    }

    fn complete(&self, request_id: u64) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&request_id)
            .is_some()
    }

    fn contains(&self, request_id: u64) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&request_id)
    }

    fn expire(&self, now: Instant) -> Vec<(u64, u64)> {
        let mut entries = self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let expired = entries
            .iter()
            .filter_map(|(&request_id, operation)| {
                (operation.deadline <= now).then_some((request_id, operation.object_id))
            })
            .collect::<Vec<_>>();
        for (request_id, _) in &expired {
            entries.remove(request_id);
        }
        expired
    }

    fn cancel_all(&self) {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }
}

impl ObservationTracker {
    fn configure(&mut self, mask: u64, snapshot: SessionObservationSnapshot) {
        self.mask = mask;
        self.scene_revision = snapshot.scene_revision;
        self.sources = snapshot
            .sources
            .into_iter()
            .map(|(source_id, source, playback)| (source_id, (source, playback)))
            .collect();
        self.queue.clear();
        self.source_gap = None;
        self.scene_gap = None;
        self.source_causation.clear();
        self.scene_causation = None;
    }

    fn note_causation(
        &mut self,
        record_type: u16,
        object_id: u64,
        causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
    ) {
        let Some(causation_id) = causation_id else { return };
        if record_type == messages::COMMIT_TXN {
            self.scene_causation = Some(causation_id);
        }
        if matches!(
            record_type,
            messages::CREATE_IMAGE
                | messages::CREATE_VIDEO
                | messages::CREATE_RASTER
                | messages::DESTROY_SOURCE
                | messages::CREATE_AUDIO
                | messages::SET_SOURCE_POLICY
                | messages::UPDATE_SOURCE_DESCRIPTOR
                | messages::PLAY
                | messages::PAUSE
                | messages::FLUSH
                | messages::EOS
        ) && object_id != 0
        {
            self.source_causation.insert(object_id, causation_id);
        }
    }

    fn collect(&mut self, snapshot: SessionObservationSnapshot) -> io::Result<()> {
        if self.mask & messages::OBSERVE_SCENE_CHANGES != 0
            && snapshot.scene_revision != self.scene_revision
            && snapshot.scene_change_reasons != 0
        {
            let sequence = self.next_sequence()?;
            let causation_id = self.scene_causation.take();
            self.push(QueuedObservation::Scene {
                scene_revision: snapshot.scene_revision,
                reason_mask: snapshot.scene_change_reasons,
                sequence,
                causation_id,
            });
        }
        self.scene_revision = snapshot.scene_revision;

        let mut current = HashMap::with_capacity(snapshot.sources.len());
        for (source_id, source, playback) in snapshot.sources {
            let previous = self.sources.get(&source_id).cloned();
            if self.mask & messages::OBSERVE_SOURCE_TRANSITIONS != 0 {
                let changed_fields =
                    previous.as_ref().map_or(messages::SOURCE_CHANGED_LIFECYCLE, |old| {
                        source_changed_fields_after(&source, old.0.revision)
                    });
                if source.revision
                    != previous.as_ref().map_or(SourceRevision::ZERO, |old| old.0.revision)
                    && changed_fields != 0
                {
                    let sequence = self.next_sequence()?;
                    let causation_id = self.source_causation.remove(&source_id);
                    self.push(QueuedObservation::Source {
                        source_id,
                        source_revision: source.revision,
                        changed_fields,
                        sequence,
                        causation_id,
                    });
                }
            }
            if self.mask & messages::OBSERVE_PLAYBACK_TRANSITIONS != 0
                && let (Some((_, Some(previous))), Some(current_playback)) = (previous, playback)
                && (previous.state != current_playback.state
                    || previous.eos_state != current_playback.eos_state)
            {
                let sequence = self.next_sequence()?;
                let causation_id = self.source_causation.remove(&source_id);
                self.push(QueuedObservation::Playback {
                    source_id,
                    snapshot: current_playback,
                    source_revision: source.revision,
                    sequence,
                    causation_id,
                });
            }
            current.insert(source_id, (source, playback));
        }
        self.sources = current;
        Ok(())
    }

    fn next_sequence(&mut self) -> io::Result<ObservationSequence> {
        self.sequence =
            self.sequence.advance().map_err(|_| invalid("observation sequence exhausted"))?;
        Ok(self.sequence)
    }

    fn push(&mut self, mut event: QueuedObservation) {
        if let Some(index) = self.queue.iter().position(|queued| queued.coalesces(event)) {
            let previous = self.queue.remove(index).unwrap();
            self.note_lost(previous);
            event = match (previous, event) {
                (
                    QueuedObservation::Source { changed_fields: old, .. },
                    QueuedObservation::Source {
                        source_id,
                        source_revision,
                        changed_fields,
                        sequence,
                        causation_id,
                    },
                ) => QueuedObservation::Source {
                    source_id,
                    source_revision,
                    changed_fields: old | changed_fields,
                    sequence,
                    causation_id,
                },
                (
                    QueuedObservation::Scene { reason_mask: old, .. },
                    QueuedObservation::Scene {
                        scene_revision,
                        reason_mask,
                        sequence,
                        causation_id,
                    },
                ) => QueuedObservation::Scene {
                    scene_revision,
                    reason_mask: old | reason_mask,
                    sequence,
                    causation_id,
                },
                (_, event) => event,
            };
        }
        if self.queue.len() >= MAX_OBSERVATION_QUEUE
            && let Some(discarded) = self.queue.pop_front()
        {
            self.note_lost(discarded);
        }
        self.queue.push_back(event);
    }

    fn note_lost(&mut self, event: QueuedObservation) {
        let gap = match event.class() {
            messages::OBSERVE_SOURCE_TRANSITIONS => &mut self.source_gap,
            messages::OBSERVE_SCENE_CHANGES => &mut self.scene_gap,
            _ => return,
        };
        if gap.is_none() {
            *gap = Some(event.sequence());
        }
    }

    fn flush(&mut self, writer: &Writer) -> io::Result<()> {
        for _ in 0..MAX_OBSERVATIONS_PER_TICK {
            let Some(event) = self.queue.pop_front() else {
                break;
            };
            match event {
                QueuedObservation::Source {
                    source_id,
                    source_revision,
                    changed_fields,
                    sequence,
                    causation_id,
                } => {
                    let body = messages::source_changed(messages::SourceChanged {
                        source_id,
                        source_revision,
                        changed_fields,
                        observation_sequence: sequence,
                        first_lost_sequence: self.source_gap.take(),
                    })?;
                    writer.write_record(
                        messages::SOURCE_CHANGED,
                        source_id,
                        &event_with_causation(&body, causation_id)?,
                    )?
                },
                QueuedObservation::Scene {
                    scene_revision,
                    reason_mask,
                    sequence,
                    causation_id,
                } => {
                    let body = messages::scene_changed(messages::SceneChanged {
                        scene_revision,
                        reason_mask,
                        observation_sequence: sequence,
                        first_lost_sequence: self.scene_gap.take(),
                    })?;
                    writer.write_record(
                        messages::SCENE_CHANGED,
                        0,
                        &event_with_causation(&body, causation_id)?,
                    )?
                },
                QueuedObservation::Playback {
                    source_id,
                    snapshot,
                    source_revision,
                    sequence,
                    causation_id,
                } => {
                    let body = messages::playback_state(messages::PlaybackState {
                        source_id,
                        snapshot,
                        source_revision,
                        observation_sequence: sequence,
                    })?;
                    writer.write_record(
                        messages::PLAYBACK_STATE,
                        source_id,
                        &event_with_causation(&body, causation_id)?,
                    )?
                },
            }
        }
        Ok(())
    }
}

fn source_changed_fields_after(source: &SourceObservation, revision: SourceRevision) -> u64 {
    source.field_revisions.iter().enumerate().fold(0, |fields, (bit, changed_at)| {
        fields | (u64::from(*changed_at > revision.get()) * (1 << bit))
    })
}

pub struct VividService {
    endpoint: String,
    token: String,
    scene: SharedScene,
    shared: Arc<ServiceShared>,
    shutdown: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
    _directory: Option<TempDir>,
}

impl VividService {
    pub fn start(metrics: DisplayMetrics, event_proxy: EventProxy) -> io::Result<Self> {
        Self::start_with_wake(
            metrics,
            Arc::new(move || event_proxy.send_event(EventType::VividFrame)),
        )
    }

    fn start_with_wake(
        metrics: DisplayMetrics,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> io::Result<Self> {
        let (listener, endpoint, directory) = bind_local_listener()?;

        let mut token = [0_u8; 32];
        getrandom::fill(&mut token).map_err(|error| {
            io::Error::other(format!("could not generate Vivid token: {error}"))
        })?;
        let token_text = hex(&token);
        let scene = SharedScene::default();
        let trace_guard = diagnostic_trace_guard(TraceComponent::Vivido)?;
        let trace = trace_guard.as_ref().map(TraceGuard::emitter);
        let shared = Arc::new(ServiceShared {
            token,
            scene: scene.clone(),
            registry: Mutex::new(Registry::default()),
            metrics: Mutex::new(metrics),
            pending_display_change: Mutex::new(None),
            capability_generation: AtomicU64::new(1),
            audio_device_available: AtomicBool::new(true),
            active_connections: AtomicUsize::new(0),
            audio_outputs: Mutex::new(HashMap::new()),
            render_state: Mutex::new((true, 0)),
            wake,
            trace,
            _trace_guard: trace_guard,
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let listener_shutdown = shutdown.clone();
        let listener_thread = thread::Builder::new().name("vivid-listener".into()).spawn({
            let shared = shared.clone();
            move || listener_loop(listener, shared, listener_shutdown)
        })?;

        Ok(Self {
            endpoint,
            token: token_text,
            scene,
            shared,
            shutdown,
            listener_thread: Some(listener_thread),
            _directory: directory,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn scene(&self) -> SharedScene {
        self.scene.clone()
    }

    #[allow(dead_code)] // Platform device watchers call this hook on supported desktop backends.
    pub fn capability_generation(&self) -> u64 {
        self.shared.capability_generation.load(Ordering::Acquire)
    }

    /// Notify producers that future source-creation capabilities changed.
    ///
    /// The accepted feature set remains immutable. Callers separately lose any affected live
    /// source through its source-scoped path.
    #[allow(dead_code)] // Platform device watchers call this hook on supported desktop backends.
    pub fn notify_capabilities_changed(&self, reason_mask: u64) -> io::Result<u64> {
        advance_capability_generation(&self.shared, reason_mask)
    }

    #[cfg(unix)]
    pub(crate) fn automation_source_status(
        &self,
        session_id: SessionId,
        source_id: u64,
    ) -> Option<messages::SourceStatus> {
        self.scene.source_status(
            (session_id, source_id),
            INITIAL_BYTE_CREDITS,
            INITIAL_PACKET_CREDITS,
        )
    }

    #[cfg(unix)]
    pub(crate) fn automation_source_keys(&self) -> Vec<SourceKey> {
        self.scene.source_keys()
    }

    #[cfg(unix)]
    pub(crate) fn automation_scene_status(
        &self,
        session_id: SessionId,
        maximum_nodes: u64,
    ) -> messages::SceneStatus {
        self.scene
            .scene_status(
                session_id,
                &messages::SceneQuery {
                    expected_revision: None,
                    cursor: None,
                    maximum_nodes: Some(maximum_nodes),
                },
            )
            .expect("unconditional scene query cannot fail")
    }

    #[cfg(unix)]
    pub(crate) fn automation_evaluate_wait(
        &self,
        session_id: SessionId,
        source_id: u64,
        condition: u64,
        value: Option<u64>,
    ) -> SourceWaitEvaluation {
        self.scene.evaluate_wait((session_id, source_id), condition, value)
    }

    pub fn update_metrics(&self, mut metrics: DisplayMetrics) -> Option<u64> {
        {
            let mut current = lock_metrics(&self.shared);
            if current.viewport_width == metrics.viewport_width
                && current.viewport_height == metrics.viewport_height
                && current.columns == metrics.columns
                && current.rows == metrics.rows
                && current.cell_width == metrics.cell_width
                && current.cell_height == metrics.cell_height
            {
                return None;
            }
            metrics.generation = current.generation.saturating_add(1);
            *current = metrics;
        }

        *lock_pending_display_change(&self.shared) =
            Some(PendingDisplayChange { metrics, last_unsettled_generation: None });
        emit_visibility(&self.shared);
        wake(&self.shared);
        Some(metrics.generation)
    }

    /// Publish at most one coalesced display update from a compositor frame.
    pub fn flush_display_change(&self, settled_generation: Option<u64>) {
        let display = {
            let mut pending = lock_pending_display_change(&self.shared);
            let settled = pending
                .as_ref()
                .is_some_and(|change| settled_generation == Some(change.metrics.generation));
            let display = pending.as_mut().and_then(|change| change.event(settled));
            if settled {
                *pending = None;
            }
            display
        };
        let Some(display) = display else {
            return;
        };
        let writers = {
            let registry = lock_registry(&self.shared);
            registry
                .sessions
                .values()
                .filter_map(|session| session.writer.upgrade())
                .collect::<Vec<_>>()
        };
        let body = messages::display_changed(0, display);
        for writer in writers {
            if let Err(error) = writer.write_record(messages::DISPLAY_CHANGED, 0, &body) {
                log::debug!("Could not notify Vivid session of display change: {error}");
            }
        }
    }

    /// Resize the terminal while preserving authenticated anchors through grid reflow.
    pub fn resize_terminal<T, S>(&self, terminal: &mut Term<T>, size: S)
    where
        T: EventListener,
        S: Dimensions,
    {
        let anchors = self.scene.anchor_positions();
        let mut positions = anchors
            .iter()
            .map(|(_, column, line, alternate)| {
                Some(ResizePoint {
                    point: Point::new(Line(*line), Column(*column)),
                    alternate: *alternate,
                })
            })
            .collect::<Vec<_>>();
        terminal.resize_with_tracking(size, &mut positions);
        let updates = anchors.into_iter().zip(positions).map(|((key, _, _, _), position)| {
            (
                key,
                position.map(|position| {
                    (position.point.column.0, position.point.line.0, position.alternate)
                }),
            )
        });
        match self.scene.apply_anchor_resize(updates) {
            Ok(removed) => self.notify_anchor_events(messages::ANCHOR_GONE, removed),
            Err(error) => log::error!("Could not advance scene revision during resize: {error}"),
        }
    }

    pub fn handle_terminal_marker(&self, marker: &str, line: i32, column: usize, alternate: bool) {
        let Ok(marker) = anchor::parse_marker(marker) else {
            return;
        };
        let session = {
            let mut registry = lock_registry(&self.shared);
            registry
                .sessions
                .iter_mut()
                .find(|(_, session)| session.tag == marker.session_tag)
                .and_then(|(&session_id, session)| {
                    if !session.accepted_features.contains(&messages::FEATURE_TEXT_ANCHORS_V2)
                        || session.seen_anchors.len() >= 4096
                        || session.seen_anchors.contains(&marker.anchor_id)
                        || !anchor::verify_marker(&session.anchor_key, &marker)
                    {
                        return None;
                    }
                    session.seen_anchors.insert(marker.anchor_id);
                    session.writer.upgrade().map(|writer| (session_id, writer))
                })
        };
        let Some((session_id, writer)) = session else {
            return;
        };
        let anchor_id = marker.anchor_id;
        if let Err(error) =
            self.scene.add_anchor_for_screen(session_id, anchor_id, column, line, alternate)
        {
            log::debug!("Rejected Vivid text anchor {anchor_id}: {error}");
            return;
        }
        if let Err(error) = writer.write_record(
            messages::ANCHOR_READY,
            anchor_id,
            &messages::anchor_event(anchor_id),
        ) {
            log::debug!("Could not acknowledge Vivid text anchor {anchor_id}: {error}");
        }
        // With the ConPTY transport, a node commit can overtake its terminal marker. The commit
        // therefore evaluates the anchored source as hidden; re-evaluate now that the marker has
        // supplied the node's terminal position so timed producers are released from that state.
        emit_visibility(&self.shared);
        wake(&self.shared);
    }

    pub fn handle_grid_scroll(&self, origin: i32, end: i32, lines: i32, history_size: usize) {
        match self.scene.scroll_anchors(origin, end, lines, history_size) {
            Ok(removed) => self.notify_anchor_events(messages::ANCHOR_GONE, removed),
            Err(error) => log::error!("Could not advance scene revision during scroll: {error}"),
        }
        wake(&self.shared);
    }

    pub fn handle_terminal_clear(&self) {
        match self.scene.clear_terminal() {
            Ok(removed) => self.notify_anchor_events(messages::ANCHOR_GONE, removed),
            Err(error) => log::error!("Could not advance scene revision during clear: {error}"),
        }
        wake(&self.shared);
    }

    /// The terminal switched between the primary and alternate screens. Anchored media on the
    /// inactive screen is hidden; anchors created on the alternate screen are gone once it exits.
    pub fn handle_screen_swap(&self, alternate: bool) {
        match self.scene.set_alternate_screen(alternate) {
            Ok(removed) => self.notify_anchor_events(messages::ANCHOR_GONE, removed),
            Err(error) => {
                log::error!("Could not advance scene revision during screen switch: {error}");
            },
        }
        wake(&self.shared);
    }

    pub fn update_visibility(&self, renderable: bool, display_offset: usize) {
        *lock_render_state(&self.shared) = (renderable, display_offset);
        emit_visibility(&self.shared);
    }

    fn notify_anchor_events(&self, record_type: u16, anchors: Vec<scene::AnchorKey>) {
        if anchors.is_empty() {
            return;
        }
        let registry = lock_registry(&self.shared);
        for (session_id, anchor_id) in anchors {
            let Some(writer) =
                registry.sessions.get(&session_id).and_then(|session| session.writer.upgrade())
            else {
                continue;
            };
            if let Err(error) =
                writer.write_record(record_type, anchor_id, &messages::anchor_event(anchor_id))
            {
                log::debug!("Could not send Vivid anchor event for {anchor_id}: {error}");
            }
        }
    }
}

impl Drop for VividService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }
    }
}

fn listener_loop(listener: LocalListener, shared: Arc<ServiceShared>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = stream.set_nonblocking(false) {
                    log::warn!("Could not configure Vivid peer stream: {error}");
                    continue;
                }
                if let Err(error) = verify_peer(&stream) {
                    log::warn!("Rejected Vivid peer: {error}");
                    continue;
                }
                if shared.active_connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                    shared.active_connections.fetch_sub(1, Ordering::AcqRel);
                    log::warn!("Rejected Vivid peer: connection quota exceeded");
                    continue;
                }
                let shared = shared.clone();
                let spawn_result = thread::Builder::new().name("vivid-connection".into()).spawn({
                    let connection_shared = shared.clone();
                    let worker_shared = shared.clone();
                    move || {
                        let _connection = ActiveConnection(&connection_shared.active_connections);
                        if let Err(error) = handle_connection(stream, worker_shared)
                            && !matches!(
                                error.kind(),
                                ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe
                            )
                        {
                            log::warn!("Vivid connection failed: {error}");
                        }
                    }
                });
                if let Err(error) = spawn_result {
                    shared.active_connections.fetch_sub(1, Ordering::AcqRel);
                    log::warn!("Could not start Vivid connection worker: {error}");
                }
            },
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            },
            Err(error) => {
                log::error!("Vivid listener failed: {error}");
                return;
            },
        }
    }
}

fn handle_connection(stream: LocalStream, shared: Arc<ServiceShared>) -> io::Result<()> {
    let (mut reader, preface) = Reader::new(stream)?;
    if let Some(trace) = &shared.trace {
        reader.set_trace(TraceChannel::new(trace.clone()));
    }
    match preface.kind {
        ConnectionKind::Control => handle_control(&mut reader, shared),
        ConnectionKind::Raster
        | ConnectionKind::Video
        | ConnectionKind::Blob
        | ConnectionKind::Audio => handle_media(&mut reader, preface.kind, shared),
        _ => Err(io::Error::new(
            ErrorKind::Unsupported,
            "this Vivid channel kind is not implemented",
        )),
    }
}

fn handle_control(reader: &mut Reader, shared: Arc<ServiceShared>) -> io::Result<()> {
    let hello_record = reader.read_record()?;
    if hello_record.record_type != messages::HELLO || hello_record.object_id != 0 {
        return Err(invalid("control connection must start with a session-level HELLO"));
    }
    let (request_id, hello) = messages::parse_hello(&hello_record.body)?;
    let writer = Arc::new(reader.writer()?);
    writer.set_maximum(hello.maximum_record_body)?;
    let negotiated = messages::negotiate_features(
        &hello.required_features,
        &hello.optional_features,
        is_supported_feature,
    );
    let unsupported_feature = negotiated.is_err();
    let supports_current_version = offers_vivid_version(
        hello.minimum_major,
        hello.minimum_minor,
        hello.maximum_major,
        hello.maximum_minor,
    );
    if !supports_current_version
        || hello.maximum_record_body == 0
        || unsupported_feature
        || hello.validate_authentication_kind(true).is_err()
    {
        let (code, diagnostic) = if !supports_current_version {
            (messages::ERROR_UNSUPPORTED_VERSION, "Vivid 1.1 is required")
        } else if hello.maximum_record_body == 0 {
            (messages::ERROR_BAD_MESSAGE, "maximum record body is zero")
        } else {
            (messages::ERROR_UNSUPPORTED_FEATURE, "required Vivid feature is unsupported")
        };
        let detail = if code == messages::ERROR_UNSUPPORTED_VERSION {
            messages::ErrorDetail::supported_version(u64::from(VIVID_MAJOR), u64::from(VIVID_MINOR))
        } else {
            messages::ErrorDetail::new()
        };
        writer.write_record(
            messages::ERROR,
            0,
            &messages::error_with_detail(request_id, code, false, &detail, diagnostic)?,
        )?;
        return Ok(());
    }

    let accepted_features = negotiated.unwrap_or_default();
    let credential = match anchor::decode_token(&hello.token) {
        Ok(credential) => credential,
        Err(_) => {
            writer.write_record(
                messages::ERROR,
                0,
                &messages::error(
                    request_id,
                    messages::ERROR_AUTH_FAILED,
                    "Vivid authentication failed",
                ),
            )?;
            return Ok(());
        },
    };
    let delegated_binding = match hello.authentication_kind {
        messages::AUTHENTICATION_WINDOW_ROOT => {
            if !constant_time_eq(&shared.token, &credential) {
                writer.write_record(
                    messages::ERROR,
                    0,
                    &messages::error(
                        request_id,
                        messages::ERROR_AUTH_FAILED,
                        "Vivid authentication failed",
                    ),
                )?;
                return Ok(());
            }
            None
        },
        messages::AUTHENTICATION_DELEGATED_CONTEXT => {
            let candidate: [u8; 32] = Sha256::digest(credential.as_slice()).into();
            let now = Instant::now();
            let registry = lock_registry(&shared);
            let mut matched = None;
            for binding in &registry.capabilities {
                let equal = constant_time_eq(&binding.verifier, &candidate);
                if equal
                    && binding.expires_at.is_none_or(|expires_at| expires_at > now)
                    && registry.contexts.get(&binding.context).is_some_and(|context| {
                        !context.revoked
                            && context.expires_at.is_none_or(|expires_at| expires_at > now)
                    })
                {
                    matched = Some((
                        binding.context,
                        binding.class_mask,
                        binding.quotas,
                        binding.expires_at,
                    ));
                }
            }
            drop(registry);
            let Some(binding) = matched else {
                writer.write_record(
                    messages::ERROR,
                    0,
                    &messages::error(
                        request_id,
                        messages::ERROR_AUTH_FAILED,
                        "Vivid authentication failed",
                    ),
                )?;
                return Ok(());
            };
            Some(binding)
        },
        _ => unreachable!("authentication kind was validated"),
    };

    let (session_id, session_tag, root_context_id) = {
        let mut registry = lock_registry(&shared);
        if registry.sessions.len() >= MAX_SESSIONS {
            let detail = messages::ErrorDetail::limit(
                messages::LIMIT_CONCURRENT_SESSIONS,
                registry.sessions.len() as u64,
                MAX_SESSIONS as u64,
            );
            writer.write_record(
                messages::ERROR,
                0,
                &messages::error_with_detail(
                    request_id,
                    messages::ERROR_LIMIT_EXCEEDED,
                    false,
                    &detail,
                    "session quota exceeded",
                )?,
            )?;
            return Ok(());
        }
        registry.next_session_id = registry
            .next_session_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Vivid session ID space exhausted"))?;
        let session_id = registry.next_session_id;
        let mut tag = [0_u8; 16];
        getrandom::fill(&mut tag).map_err(|error| {
            io::Error::other(format!("could not generate session tag: {error}"))
        })?;
        let anchor_key = anchor::derive_key(&credential, &tag);
        let (authority_root_session, root_context_id, context_class_mask) =
            if let Some((context, class_mask, _, _)) = delegated_binding {
                (context.authority_root_session, context.context_id, class_mask)
            } else {
                let root_context_id = session_id
                    .checked_shl(32)
                    .and_then(|value| value.checked_add(1))
                    .ok_or_else(|| io::Error::other("root context ID space exhausted"))?;
                registry.contexts.insert(
                    ContextKey { authority_root_session: session_id, context_id: root_context_id },
                    ContextEntry {
                        parent: None,
                        class_mask: messages::CONTEXT_CLASS_MASK,
                        quotas: root_context_quotas(),
                        _label: "root".into(),
                        expires_at: None,
                        revoked: false,
                    },
                );
                (session_id, root_context_id, messages::CONTEXT_CLASS_MASK)
            };
        registry.sessions.insert(
            session_id,
            SessionRuntime {
                writer: Arc::downgrade(&writer),
                tag,
                anchor_key,
                seen_anchors: HashSet::new(),
                last_visibility: HashMap::new(),
                accepted_features: accepted_features.iter().copied().collect(),
                authority_root_session,
                bound_context_id: root_context_id,
                context_class_mask,
                context_quotas: delegated_binding
                    .map(|(_, _, quotas, _)| quotas)
                    .unwrap_or_else(root_context_quotas),
                active_media_connections: 0,
                revoked: false,
            },
        );
        (session_id, tag, root_context_id)
    };

    let metrics = *lock_metrics(&shared);
    writer.write_record(
        messages::WELCOME,
        0,
        &messages::welcome_preserving_at_generations(
            request_id,
            session_id,
            &session_tag,
            root_context_id,
            DisplayChanged {
                display_generation: metrics.generation,
                viewport_width: metrics.viewport_width,
                viewport_height: metrics.viewport_height,
                grid_columns: metrics.columns,
                grid_rows: metrics.rows,
                cell_width: metrics.cell_width,
                cell_height: metrics.cell_height,
                settled: true,
            },
            &accepted_features,
            shared.capability_generation.load(Ordering::Acquire),
            shared.scene.scene_revision(session_id),
            &[],
        ),
    )?;
    log::info!("Authenticated Vivid producer {:?} as session {session_id}", hello.producer);

    let mut transactions: HashMap<u64, Vec<SceneMutation>> = HashMap::new();
    let pending = Arc::new(PendingOperations::default());
    let mut waits = HashMap::new();
    let mut idempotency = HashMap::<[u8; messages::IDEMPOTENCY_KEY_BYTES], IdempotencyEntry>::new();
    let mut observations = ObservationTracker::default();
    observations.configure(0, shared.scene.take_observation_snapshot(session_id));
    let result = 'control: loop {
        let unavailable = {
            let mut registry = lock_registry(&shared);
            let Some(session) = registry.sessions.get(&session_id) else {
                break 'control Ok(());
            };
            let context_key = ContextKey {
                authority_root_session: session.authority_root_session,
                context_id: session.bound_context_id,
            };
            let expired = registry.contexts.get(&context_key).is_some_and(|context| {
                context.expires_at.is_some_and(|expires_at| expires_at <= Instant::now())
            });
            if expired {
                if let Some(context) = registry.contexts.get_mut(&context_key) {
                    context.revoked = true;
                }
                registry.capabilities.retain(|binding| binding.context != context_key);
                if let Some(session) = registry.sessions.get_mut(&session_id) {
                    session.revoked = true;
                }
            }
            registry.sessions.get(&session_id).is_none_or(|session| session.revoked)
        };
        if unavailable {
            let _ = writer.write_record(messages::INPUT_RESET, 0, &messages::input_reset());
            let body = messages::error_with_detail(
                0,
                messages::ERROR_CONTEXT_REVOKED,
                true,
                &messages::ErrorDetail::new(),
                "delegated context expired or was revoked",
            )?;
            let _ = writer.write_record(messages::ERROR, root_context_id, &body);
            cleanup_revoked_session(&shared, session_id);
            break Ok(());
        }
        for (request_id, object_id) in pending.expire(Instant::now()) {
            if let Err(error) = writer.write_record(
                messages::ERROR,
                object_id,
                &messages::error(
                    request_id,
                    messages::ERROR_TIMEOUT,
                    "pending operation timed out",
                ),
            ) {
                break 'control Err(error);
            }
        }
        if let Err(error) =
            service_source_waits(&shared.scene, session_id, &writer, &mut waits, Instant::now())
        {
            break Err(error);
        }
        if let Err(error) = observations.collect(shared.scene.take_observation_snapshot(session_id))
        {
            break Err(error);
        }
        if let Err(error) = observations.flush(&writer) {
            break Err(error);
        }
        match reader.wait_readable(CONTROL_POLL_INTERVAL) {
            Ok(true) => {},
            Ok(false) => continue,
            Err(error) => break Err(error),
        }
        let record = match reader.read_record() {
            Ok(record) => record,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break Ok(()),
            Err(error) => break Err(error),
        };
        let envelope = match messages::decode_control(&record.body) {
            Ok(envelope) => envelope,
            Err(_) => {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(0, messages::ERROR_BAD_MESSAGE, "invalid control envelope"),
                )?;
                continue;
            },
        };
        if messages::validate_request_metadata(
            record.record_type,
            &envelope,
            accepted_features.contains(&messages::FEATURE_ATOMIC_CONTROL_V1),
        )
        .is_err()
        {
            writer.write_record(
                messages::ERROR,
                record.object_id,
                &messages::error(
                    envelope.request_id,
                    messages::ERROR_BAD_MESSAGE,
                    "invalid atomic request metadata",
                ),
            )?;
            continue;
        }
        match evaluate_preconditions(&record, &envelope, session_id, &shared, &transactions) {
            Ok(()) => {},
            Err(PreconditionError::Malformed(message)) => {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(envelope.request_id, messages::ERROR_BAD_MESSAGE, message),
                )?;
                continue;
            },
            Err(PreconditionError::Failed { kind, mut detail }) => {
                detail.insert_u64(messages::ERROR_DETAIL_PRECONDITION_KIND, kind);
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error_with_detail(
                        envelope.request_id,
                        messages::ERROR_PRECONDITION_FAILED,
                        false,
                        &detail,
                        "request precondition failed",
                    )?,
                )?;
                continue;
            },
        }
        let request_hash =
            envelope.idempotency_key.map(|_| idempotency_request_hash(&record)).transpose()?;
        if let (Some(key), Some(request_hash)) = (envelope.idempotency_key, request_hash)
            && let Some(entry) = idempotency.get(&key)
        {
            if constant_time_eq(&entry.request_hash, &request_hash) {
                replay_idempotent_outcome(&writer, envelope.request_id, entry.outcome)?;
            } else {
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(
                        envelope.request_id,
                        messages::ERROR_BAD_MESSAGE,
                        "idempotency key was reused for a different request",
                    ),
                )?;
            }
            continue;
        }
        if envelope.idempotency_key.is_some() && idempotency.len() >= MAX_IDEMPOTENCY_ENTRIES {
            let mut detail = messages::ErrorDetail::new();
            detail.insert_u64(
                messages::ERROR_DETAIL_LIMIT_ID,
                messages::LIMIT_IDEMPOTENCY_MAP_ENTRIES,
            );
            detail.insert_u64(messages::ERROR_DETAIL_CURRENT, MAX_IDEMPOTENCY_ENTRIES as u64);
            detail.insert_u64(messages::ERROR_DETAIL_MAXIMUM, MAX_IDEMPOTENCY_ENTRIES as u64);
            writer.write_record(
                messages::ERROR,
                record.object_id,
                &messages::error_with_detail(
                    envelope.request_id,
                    messages::ERROR_LIMIT_EXCEEDED,
                    false,
                    &detail,
                    "idempotency map is full",
                )?,
            )?;
            continue;
        }
        let result = dispatch_control(
            &record,
            session_id,
            root_context_id,
            &shared,
            &writer,
            &pending,
            &mut transactions,
            &mut waits,
            &mut observations,
        );
        match result {
            Ok(ControlAction::Continue) => {
                observations.note_causation(
                    record.record_type,
                    record.object_id,
                    envelope.causation_id,
                );
                if let (Some(key), Some(request_hash), Some(outcome)) = (
                    envelope.idempotency_key,
                    request_hash,
                    idempotency_outcome(&record, session_id, &shared),
                ) {
                    idempotency.insert(key, IdempotencyEntry { request_hash, outcome });
                }
            },
            Ok(ControlAction::Goodbye) => break Ok(()),
            Err(error) => {
                let request_id = messages::decode_control(&record.body)
                    .map(|envelope| envelope.request_id)
                    .unwrap_or(0);
                writer.write_record(
                    messages::ERROR,
                    record.object_id,
                    &messages::error(request_id, error.code, error.message),
                )?;
                if error.fatal {
                    break Err(invalid(error.message));
                }
            },
        }
    };

    pending.cancel_all();
    cleanup_session(&shared, session_id);
    wake(&shared);
    result
}

fn is_supported_feature(feature: u64) -> bool {
    matches!(
        feature,
        messages::FEATURE_RASTER_RGBA8
            | messages::FEATURE_SCENE_TRANSACTIONS
            | messages::FEATURE_GRID_CELL_NODES
            | messages::FEATURE_CREDIT_FLOW_CONTROL
            | messages::FEATURE_ENCODED_IMAGE_V1
            | messages::FEATURE_RASTER_ZSTD_V1
            | messages::FEATURE_RASTER_PREMULTIPLIED_ALPHA
            | messages::FEATURE_VISIBILITY_EVENTS_V1
            | messages::FEATURE_VIDEO_ACCESS_UNIT_V1
            | messages::FEATURE_VIDEO_CONTROL_V1
            | messages::FEATURE_TEXT_ANCHORS_V2
            | messages::FEATURE_AUDIO_ACCESS_UNIT_V1
            | messages::FEATURE_NODE_CLIP_RECT_V1
            | messages::FEATURE_DECODER_DESCRIPTION_V1
            | messages::FEATURE_OBSERVABILITY_CORE_V1
            | messages::FEATURE_ATOMIC_CONTROL_V1
            | messages::FEATURE_DELEGATED_CONTEXT_V1
            | messages::FEATURE_SOURCE_CAPTURE_POLICY_V1
            | messages::FEATURE_SOURCE_DESCRIPTOR_V1
            | messages::FEATURE_MEDIA_ORDER_BARRIER_V1
    )
}

fn offers_vivid_version(
    minimum_major: u64,
    minimum_minor: u64,
    maximum_major: u64,
    maximum_minor: u64,
) -> bool {
    let current = (u64::from(VIVID_MAJOR), u64::from(VIVID_MINOR));
    (minimum_major, minimum_minor) <= current && (maximum_major, maximum_minor) >= current
}

fn negotiated(shared: &Arc<ServiceShared>, session_id: SessionId, feature: u64) -> bool {
    lock_registry(shared)
        .sessions
        .get(&session_id)
        .is_some_and(|session| session.accepted_features.contains(&feature))
}

fn session_quotas(
    shared: &Arc<ServiceShared>,
    session_id: SessionId,
) -> Option<messages::ContextQuotas> {
    lock_registry(shared).sessions.get(&session_id).map(|session| session.context_quotas)
}

fn required_context_class(record_type: u16) -> Option<u64> {
    match record_type {
        messages::SET_OBSERVATION
        | messages::QUERY_SOURCE
        | messages::QUERY_SCENE
        | messages::WAIT_SOURCE
        | messages::CANCEL_WAIT => Some(messages::CONTEXT_CLASS_OBSERVE),
        messages::CREATE_RASTER
        | messages::CREATE_VIDEO
        | messages::CREATE_AUDIO
        | messages::CREATE_IMAGE
        | messages::DESTROY_SOURCE
        | messages::SET_SOURCE_POLICY
        | messages::UPDATE_SOURCE_DESCRIPTOR
        | messages::PLAY
        | messages::PAUSE
        | messages::FLUSH
        | messages::DRAIN
        | messages::EOS => Some(messages::CONTEXT_CLASS_CREATE_SOURCE),
        messages::BEGIN_TXN
        | messages::CREATE_NODE
        | messages::UPDATE_NODE
        | messages::DELETE_NODE
        | messages::COMMIT_TXN
        | messages::ABORT_TXN => Some(messages::CONTEXT_CLASS_MUTATE_SCENE),
        messages::CREATE_CONTEXT | messages::DELEGATE_CONTEXT | messages::REVOKE_CONTEXT => {
            Some(messages::CONTEXT_CLASS_ADMINISTER)
        },
        _ => None,
    }
}

fn context_is_descendant(registry: &Registry, candidate: ContextKey, ancestor: ContextKey) -> bool {
    let mut current = Some(candidate);
    for _ in 0..=messages::MAX_CONTEXTS_PER_SESSION {
        let Some(key) = current else { return false };
        if key == ancestor {
            return true;
        }
        current = registry.contexts.get(&key).and_then(|context| context.parent);
    }
    false
}

fn context_expiry_us(expires_at: Option<Instant>, now: Instant) -> u64 {
    expires_at
        .map(|expires_at| {
            u64::try_from(expires_at.saturating_duration_since(now).as_micros()).unwrap_or(u64::MAX)
        })
        .unwrap_or(0)
}

fn enforce_context_source_capacity(
    shared: &Arc<ServiceShared>,
    session_id: SessionId,
    retained_pixels: u64,
    maximum_media_body: u32,
) -> Result<(), ProtocolError> {
    let quotas = session_quotas(shared, session_id).ok_or(ProtocolError {
        code: messages::ERROR_CONTEXT_REVOKED,
        message: "session context was revoked",
        fatal: true,
    })?;
    if u64::from(maximum_media_body) > quotas.maximum_media_bytes {
        return Err(ProtocolError {
            code: messages::ERROR_LIMIT_EXCEEDED,
            message: "delegated context media-byte quota is smaller than one packet",
            fatal: false,
        });
    }
    if shared
        .scene
        .configured_pixel_capacity(session_id)
        .checked_add(retained_pixels)
        .is_none_or(|projected| projected > quotas.maximum_retained_pixels)
    {
        return Err(ProtocolError {
            code: messages::ERROR_LIMIT_EXCEEDED,
            message: "delegated context retained-pixel quota exceeded",
            fatal: false,
        });
    }
    Ok(())
}

fn cleanup_revoked_session(shared: &Arc<ServiceShared>, session_id: SessionId) {
    {
        let mut registry = lock_registry(shared);
        registry.tickets.retain(|_, ticket| ticket.session_id != session_id);
    }
    let outputs = {
        let mut outputs =
            shared.audio_outputs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let keys = outputs.keys().copied().filter(|key| key.0 == session_id).collect::<Vec<_>>();
        keys.into_iter().filter_map(|key| outputs.remove(&key)).collect::<Vec<_>>()
    };
    for output in outputs {
        output.stop();
    }
    if let Err(error) = shared.scene.detach_session(session_id) {
        log::error!("Could not revoke Vivid session scene resources: {error}");
    }
}

fn audio_group(shared: &Arc<ServiceShared>, source: SourceKey) -> Vec<Arc<AudioOutput>> {
    let mut keys = if matches!(shared.scene.source_config(source), Some(SourceConfig::Audio(_))) {
        vec![source]
    } else {
        shared.scene.linked_audio_sources(source)
    };
    keys.sort_unstable();
    let outputs = shared.audio_outputs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    keys.into_iter().filter_map(|key| outputs.get(&key).cloned()).collect()
}

fn media_time_reached(shared: &Arc<ServiceShared>, source: SourceKey, pts_us: i64) -> Option<bool> {
    if let Some(output) = audio_group(shared, source).into_iter().next() {
        if output.video_gate_stalled() {
            // A producer that never sends linked audio must not freeze video forever. Once audio
            // arrives, this condition clears and the linked audio clock becomes authoritative.
            return shared.scene.presentation_due(source, pts_us);
        }
        return Some(output.pts_reached(pts_us));
    }
    shared.scene.presentation_due(source, pts_us)
}

fn apply_eos(shared: &Arc<ServiceShared>, key: SourceKey, epoch: u32) -> Result<(), ProtocolError> {
    let linked_audio = shared.scene.linked_audio_sources(key);
    shared.scene.signal_eos(key, epoch).map_err(|message| ProtocolError {
        code: messages::ERROR_STALE_EPOCH,
        message,
        fatal: false,
    })?;
    for audio_key in linked_audio {
        shared.scene.signal_eos(audio_key, epoch).map_err(|message| ProtocolError {
            code: messages::ERROR_STALE_EPOCH,
            message,
            fatal: false,
        })?;
    }
    let audio_keys = if matches!(shared.scene.source_config(key), Some(SourceConfig::Audio(_))) {
        vec![key]
    } else {
        shared.scene.linked_audio_sources(key)
    };
    for audio_key in audio_keys {
        let output = shared
            .audio_outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&audio_key)
            .cloned();
        let Some(output) = output else {
            continue;
        };
        output.signal_eos();
        let scene = shared.scene.clone();
        thread::Builder::new()
            .name("vivid-audio-playback-end".into())
            .spawn(move || {
                if output.wait_drained().is_ok()
                    && let Err(error) = scene.mark_playback_ended(audio_key)
                {
                    log::debug!("Could not record drained audio source {audio_key:?}: {error}");
                }
            })
            .map_err(|_| ProtocolError {
                code: messages::ERROR_LIMIT_EXCEEDED,
                message: "could not start audio completion observer",
                fatal: false,
            })?;
    }
    Ok(())
}

#[derive(Debug)]
enum ControlAction {
    Continue,
    Goodbye,
}

#[derive(Debug)]
struct ProtocolError {
    code: u64,
    message: &'static str,
    fatal: bool,
}

struct PendingReply {
    record_type: u16,
    object_id: u64,
    body: Vec<u8>,
}

impl PendingReply {
    fn ok(object_id: u64, request_id: u64) -> Self {
        Self { record_type: messages::OK, object_id, body: messages::ok(request_id) }
    }
}

fn spawn_pending_operation(
    pending: &Arc<PendingOperations>,
    writer: &Arc<Writer>,
    request_id: u64,
    object_id: u64,
    name: &'static str,
    operation: impl FnOnce() -> Result<PendingReply, ProtocolError> + Send + 'static,
) -> Result<(), ProtocolError> {
    register_pending_operation(pending, request_id, object_id)?;
    let worker_pending = pending.clone();
    let worker_writer = writer.clone();
    if thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let result = operation();
            if !worker_pending.complete(request_id) {
                return;
            }
            let write_result = match result {
                Ok(reply) => {
                    worker_writer.write_record(reply.record_type, reply.object_id, &reply.body)
                },
                Err(error) => worker_writer.write_record(
                    messages::ERROR,
                    object_id,
                    &messages::error(request_id, error.code, error.message),
                ),
            };
            if let Err(error) = write_result {
                log::debug!("Could not complete pending Vivid operation: {error}");
            }
        })
        .is_err()
    {
        pending.complete(request_id);
        return Err(ProtocolError {
            code: messages::ERROR_LIMIT_EXCEEDED,
            message: "could not start pending operation worker",
            fatal: false,
        });
    }
    Ok(())
}

fn register_pending_operation(
    pending: &PendingOperations,
    request_id: u64,
    object_id: u64,
) -> Result<(), ProtocolError> {
    match pending.register(request_id, object_id, PENDING_OPERATION_TIMEOUT) {
        Ok(()) => {},
        Err(PendingRegisterError::Full) => {
            return Err(ProtocolError {
                code: messages::ERROR_LIMIT_EXCEEDED,
                message: "pending operation quota exceeded",
                fatal: false,
            });
        },
        Err(PendingRegisterError::Duplicate) => {
            return Err(ProtocolError {
                code: messages::ERROR_BAD_MESSAGE,
                message: "request ID already has a pending operation",
                fatal: false,
            });
        },
    }
    Ok(())
}

fn spawn_audio_source_open(
    pending: &Arc<PendingOperations>,
    writer: &Arc<Writer>,
    shared: &Arc<ServiceShared>,
    session_id: SessionId,
    request_id: u64,
    config: messages::ParsedAudioSourceConfig,
    max_media_body: u32,
) -> Result<(), ProtocolError> {
    let source_id = config.source_id;
    register_pending_operation(pending, request_id, source_id)?;
    let worker_pending = pending.clone();
    let worker_writer = writer.clone();
    let worker_shared = shared.clone();
    if thread::Builder::new()
        .name("vivid-audio-open".into())
        .spawn(move || {
            let opened = if audio::supports(&config) {
                AudioOutput::open()
                    .inspect(|_| update_audio_device_availability(&worker_shared, true))
                    .map_err(|_| {
                        update_audio_device_availability(&worker_shared, false);
                        ProtocolError {
                            code: messages::ERROR_DEVICE_LOST,
                            message: "default audio output is unavailable",
                            fatal: false,
                        }
                    })
            } else {
                Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_CONFIG,
                    message: "audio decoder configuration is unavailable",
                    fatal: false,
                })
            };
            if !worker_pending.complete(request_id) {
                if let Ok(output) = opened {
                    output.stop();
                }
                let _ = worker_shared.scene.remove_source((session_id, source_id));
                return;
            }
            let result = opened.and_then(|output| {
                worker_shared
                    .audio_outputs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert((session_id, source_id), output.clone());
                match prepare_source_ready(
                    &worker_shared,
                    request_id,
                    (session_id, source_id),
                    ConnectionKind::Audio,
                    max_media_body,
                ) {
                    Ok(body) => Ok(PendingReply {
                        record_type: messages::SOURCE_READY,
                        object_id: source_id,
                        body,
                    }),
                    Err(_) => {
                        worker_shared
                            .audio_outputs
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&(session_id, source_id));
                        let _ = worker_shared.scene.remove_source((session_id, source_id));
                        output.stop();
                        Err(ProtocolError {
                            code: messages::ERROR_LIMIT_EXCEEDED,
                            message: "could not create audio media ticket",
                            fatal: false,
                        })
                    },
                }
            });
            if result.is_err() {
                worker_shared
                    .audio_outputs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&(session_id, source_id));
                let _ = worker_shared.scene.remove_source((session_id, source_id));
            }
            let write_result = match result {
                Ok(reply) => {
                    worker_writer.write_record(reply.record_type, reply.object_id, &reply.body)
                },
                Err(error) => worker_writer.write_record(
                    messages::ERROR,
                    source_id,
                    &messages::error(request_id, error.code, error.message),
                ),
            };
            if let Err(error) = write_result {
                log::debug!("Could not complete pending audio source creation: {error}");
            }
        })
        .is_err()
    {
        pending.complete(request_id);
        return Err(ProtocolError {
            code: messages::ERROR_LIMIT_EXCEEDED,
            message: "could not start audio device worker",
            fatal: false,
        });
    }
    Ok(())
}

fn service_source_waits(
    scene: &SharedScene,
    session_id: SessionId,
    writer: &Writer,
    waits: &mut HashMap<u64, RegisteredWait>,
    now: Instant,
) -> io::Result<()> {
    let request_ids = waits.keys().copied().collect::<Vec<_>>();
    for request_id in request_ids {
        let Some(wait) = waits.get(&request_id).copied() else {
            continue;
        };
        let evaluation =
            scene.evaluate_wait((session_id, wait.source_id), wait.condition, wait.value);
        let completion = match evaluation {
            SourceWaitEvaluation::Satisfied(satisfied) => {
                Some((messages::WAIT_SATISFIED, messages::wait_satisfied(request_id, satisfied)?))
            },
            SourceWaitEvaluation::NotVisible => Some((
                messages::ERROR,
                messages::error(
                    request_id,
                    messages::ERROR_NOT_VISIBLE,
                    "source has no eligible visible placement",
                ),
            )),
            SourceWaitEvaluation::NotFound => Some((
                messages::ERROR,
                messages::error(
                    request_id,
                    messages::ERROR_CANCELLED,
                    "source was destroyed while waiting",
                ),
            )),
            SourceWaitEvaluation::Pending if wait.deadline <= now => Some((
                messages::ERROR,
                messages::error(request_id, messages::ERROR_TIMEOUT, "source wait timed out"),
            )),
            SourceWaitEvaluation::Pending => None,
        };
        if let Some((record_type, body)) = completion {
            waits.remove(&request_id);
            writer.write_record(record_type, wait.source_id, &body)?;
        }
    }
    Ok(())
}

fn event_with_causation(
    body: &[u8],
    causation_id: Option<[u8; messages::CAUSATION_ID_BYTES]>,
) -> io::Result<Vec<u8>> {
    let Some(causation_id) = causation_id else { return Ok(body.to_vec()) };
    messages::with_request_metadata(
        body,
        &messages::RequestMetadata {
            preconditions: Default::default(),
            idempotency_key: None,
            causation_id: Some(causation_id),
        },
    )
}

fn evaluate_preconditions(
    record: &Record,
    envelope: &messages::ControlEnvelope,
    session_id: SessionId,
    shared: &Arc<ServiceShared>,
    _transactions: &HashMap<u64, Vec<SceneMutation>>,
) -> Result<(), PreconditionError> {
    if envelope.preconditions.is_empty() {
        return Ok(());
    }
    let source_target = matches!(
        record.record_type,
        messages::DESTROY_SOURCE
            | messages::SET_SOURCE_POLICY
            | messages::UPDATE_SOURCE_DESCRIPTOR
            | messages::PLAY
            | messages::PAUSE
            | messages::FLUSH
            | messages::DRAIN
            | messages::EOS
    );
    let observation = source_target
        .then(|| shared.scene.source_observation((session_id, record.object_id)))
        .flatten();
    for (&kind, &expected) in &envelope.preconditions {
        let current = match kind {
            messages::PRECONDITION_SCENE_REVISION if record.record_type == messages::COMMIT_TXN => {
                shared.scene.scene_revision(session_id).get()
            },
            messages::PRECONDITION_SOURCE_REVISION if source_target => observation
                .as_ref()
                .ok_or(PreconditionError::Malformed(
                    "source precondition targets a missing source",
                ))?
                .revision
                .get(),
            messages::PRECONDITION_SOURCE_EPOCH if source_target => u64::from(
                observation
                    .as_ref()
                    .ok_or(PreconditionError::Malformed(
                        "source precondition targets a missing source",
                    ))?
                    .epoch,
            ),
            messages::PRECONDITION_SOURCE_LIFECYCLE if source_target => {
                observation
                    .as_ref()
                    .ok_or(PreconditionError::Malformed(
                        "source precondition targets a missing source",
                    ))?
                    .lifecycle
            },
            messages::PRECONDITION_ANCHOR_STATE
                if matches!(record.record_type, messages::CREATE_NODE | messages::UPDATE_NODE) =>
            {
                if !matches!(expected, messages::ANCHOR_STATE_READY | messages::ANCHOR_STATE_GONE) {
                    return Err(PreconditionError::Malformed(
                        "anchor-state precondition has an invalid expected value",
                    ));
                }
                let (_, node) = messages::parse_scene_node(&record.body).map_err(|_| {
                    PreconditionError::Malformed(
                        "anchor-state precondition targets an invalid scene node",
                    )
                })?;
                let anchor_id = node.node.anchor_id.ok_or(PreconditionError::Malformed(
                    "anchor-state precondition requires an anchored node",
                ))?;
                shared.scene.anchor_state(session_id, anchor_id)
            },
            messages::PRECONDITION_CONTENT_REVISION
                if record.record_type == messages::UPDATE_SOURCE_DESCRIPTOR =>
            {
                shared.scene.source_content_revision((session_id, record.object_id)).ok_or(
                    PreconditionError::Malformed(
                        "content-revision precondition targets a missing source",
                    ),
                )?
            },
            _ => {
                return Err(PreconditionError::Malformed(
                    "precondition kind is meaningless for this operation",
                ));
            },
        };
        if current != expected {
            let mut detail = messages::ErrorDetail::new();
            match kind {
                messages::PRECONDITION_SCENE_REVISION => {
                    detail.insert_u64(messages::ERROR_DETAIL_SCENE_REVISION, current);
                },
                messages::PRECONDITION_SOURCE_REVISION => {
                    detail.insert_u64(messages::ERROR_DETAIL_SOURCE_REVISION, current);
                },
                messages::PRECONDITION_SOURCE_EPOCH => {
                    detail.insert_u64(messages::ERROR_DETAIL_SOURCE_EPOCH, current);
                },
                _ => {},
            }
            return Err(PreconditionError::Failed { kind, detail });
        }
    }
    Ok(())
}

fn idempotency_request_hash(record: &Record) -> io::Result<[u8; 32]> {
    let mut envelope = vivid_protocol::cbor::decode(&record.body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let vivid_protocol::cbor::Value::Map(entries) = &mut envelope else {
        return Err(invalid("control envelope is not a map"));
    };
    for (key, value) in entries.iter_mut() {
        if *key == 0 {
            *value = vivid_protocol::cbor::Value::Unsigned(0);
        }
    }
    entries.retain(|(key, _)| *key != 5);
    let canonical = vivid_protocol::cbor::encode(&envelope)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let mut digest = Sha256::new();
    digest.update(record.record_type.to_be_bytes());
    digest.update(record.flags.to_be_bytes());
    digest.update(record.object_id.to_be_bytes());
    digest.update(canonical);
    Ok(digest.finalize().into())
}

fn idempotency_outcome(
    record: &Record,
    session_id: SessionId,
    shared: &Arc<ServiceShared>,
) -> Option<IdempotencyOutcome> {
    if matches!(
        record.record_type,
        messages::CREATE_RASTER
            | messages::CREATE_VIDEO
            | messages::CREATE_AUDIO
            | messages::CREATE_IMAGE
    ) {
        return Some(IdempotencyOutcome::SourceCreated { source_id: record.object_id });
    }
    if record.record_type == messages::COMMIT_TXN {
        return Some(IdempotencyOutcome::Presented {
            scene_revision: shared.scene.scene_revision(session_id),
        });
    }
    matches!(
        record.record_type,
        messages::SET_OBSERVATION
            | messages::DESTROY_SOURCE
            | messages::SET_SOURCE_POLICY
            | messages::UPDATE_SOURCE_DESCRIPTOR
            | messages::BEGIN_TXN
            | messages::CREATE_NODE
            | messages::UPDATE_NODE
            | messages::DELETE_NODE
            | messages::ABORT_TXN
            | messages::PLAY
            | messages::PAUSE
            | messages::FLUSH
            | messages::EOS
    )
    .then_some(IdempotencyOutcome::Ok { object_id: record.object_id })
}

fn replay_idempotent_outcome(
    writer: &Writer,
    request_id: u64,
    outcome: IdempotencyOutcome,
) -> io::Result<()> {
    match outcome {
        IdempotencyOutcome::Ok { object_id } => {
            writer.write_record(messages::OK, object_id, &messages::ok(request_id))
        },
        IdempotencyOutcome::Presented { scene_revision } => writer.write_record(
            messages::PRESENTED,
            0,
            &messages::presented(request_id, scene_revision),
        ),
        IdempotencyOutcome::SourceCreated { source_id } => {
            let mut detail = messages::ErrorDetail::new();
            detail.insert_u64(messages::ERROR_DETAIL_IDEMPOTENT_OUTCOME, 2);
            writer.write_record(
                messages::ERROR,
                source_id,
                &messages::error_with_detail(
                    request_id,
                    messages::ERROR_ALREADY_APPLIED,
                    false,
                    &detail,
                    "source creation was already applied; query source state",
                )?,
            )
        },
    }
}

fn constant_time_eq<const N: usize>(left: &[u8; N], right: &[u8; N]) -> bool {
    left.iter().zip(right).fold(0_u8, |difference, (left, right)| difference | (left ^ right)) == 0
}

#[allow(clippy::too_many_arguments)]
fn dispatch_control(
    record: &Record,
    session_id: SessionId,
    root_context_id: u64,
    shared: &Arc<ServiceShared>,
    writer: &Arc<Writer>,
    pending: &Arc<PendingOperations>,
    transactions: &mut HashMap<u64, Vec<SceneMutation>>,
    waits: &mut HashMap<u64, RegisteredWait>,
    observations: &mut ObservationTracker,
) -> Result<ControlAction, ProtocolError> {
    let bad = |message| ProtocolError { code: messages::ERROR_BAD_MESSAGE, message, fatal: false };
    if let Some(required_class) = required_context_class(record.record_type) {
        let session_policy = lock_registry(shared).sessions.get(&session_id).map(|session| {
            (session.context_class_mask & required_class != 0, session.context_quotas)
        });
        let permitted = session_policy.is_some_and(|(permitted, _)| permitted);
        if !permitted {
            return Err(ProtocolError {
                code: messages::ERROR_NOT_FOUND,
                message: "object is outside the authenticated context",
                fatal: false,
            });
        }
        let quotas = session_policy.expect("permitted session has a policy").1;
        let counts = shared.scene.counts(session_id);
        if matches!(
            record.record_type,
            messages::CREATE_RASTER
                | messages::CREATE_VIDEO
                | messages::CREATE_AUDIO
                | messages::CREATE_IMAGE
        ) && counts.sources >= quotas.maximum_sources
        {
            return Err(ProtocolError {
                code: messages::ERROR_LIMIT_EXCEEDED,
                message: "delegated context source quota exceeded",
                fatal: false,
            });
        }
        if record.record_type == messages::CREATE_NODE {
            let queued_creates = transactions
                .values()
                .flatten()
                .filter(|mutation| matches!(mutation, SceneMutation::Create(_)))
                .count() as u64;
            if counts.nodes.saturating_add(queued_creates) >= quotas.maximum_nodes {
                return Err(ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message: "delegated context node quota exceeded",
                    fatal: false,
                });
            }
        }
    }
    if record.record_type != messages::CANCEL_WAIT
        && let Ok(envelope) = messages::decode_control(&record.body)
        && waits.contains_key(&envelope.request_id)
    {
        return Err(bad("request ID already has a registered wait"));
    }
    match record.record_type {
        messages::CREATE_CONTEXT => {
            if !negotiated(shared, session_id, messages::FEATURE_DELEGATED_CONTEXT_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "delegated contexts were not negotiated",
                    fatal: false,
                });
            }
            let (envelope, request) = messages::parse_create_context(&record.body)
                .map_err(|_| bad("invalid CREATE_CONTEXT"))?;
            if record.object_id != request.context_id {
                return Err(bad("CREATE_CONTEXT object ID mismatch"));
            }
            let now = Instant::now();
            let ready = {
                let mut registry = lock_registry(shared);
                let session = registry.sessions.get(&session_id).ok_or(ProtocolError {
                    code: messages::ERROR_CONTEXT_REVOKED,
                    message: "session context was revoked",
                    fatal: true,
                })?;
                let bound = ContextKey {
                    authority_root_session: session.authority_root_session,
                    context_id: session.bound_context_id,
                };
                let key = ContextKey {
                    authority_root_session: session.authority_root_session,
                    context_id: request.context_id,
                };
                let parent = ContextKey {
                    authority_root_session: session.authority_root_session,
                    context_id: request.parent_context_id,
                };
                if registry.contexts.contains_key(&key) {
                    return Err(ProtocolError {
                        code: messages::ERROR_DUPLICATE_ID,
                        message: "context ID already exists",
                        fatal: false,
                    });
                }
                if registry
                    .contexts
                    .keys()
                    .filter(|key| key.authority_root_session == session.authority_root_session)
                    .count()
                    >= messages::MAX_CONTEXTS_PER_SESSION
                {
                    return Err(ProtocolError {
                        code: messages::ERROR_LIMIT_EXCEEDED,
                        message: "context quota exceeded",
                        fatal: false,
                    });
                }
                let parent_entry = registry.contexts.get(&parent).ok_or(ProtocolError {
                    code: messages::ERROR_NOT_FOUND,
                    message: "parent context does not exist",
                    fatal: false,
                })?;
                if !context_is_descendant(&registry, parent, bound)
                    || parent_entry.revoked
                    || parent_entry.expires_at.is_some_and(|expires_at| expires_at <= now)
                {
                    return Err(ProtocolError {
                        code: messages::ERROR_NOT_FOUND,
                        message: "parent context does not exist",
                        fatal: false,
                    });
                }
                let class_mask = request.class_mask & parent_entry.class_mask;
                let quotas = request.quotas.intersect(parent_entry.quotas);
                let requested_expiry = (request.expiry_us != 0)
                    .then(|| now + Duration::from_micros(request.expiry_us));
                let expires_at = match (requested_expiry, parent_entry.expires_at) {
                    (Some(requested), Some(parent)) => Some(requested.min(parent)),
                    (Some(requested), None) => Some(requested),
                    (None, parent) => parent,
                };
                registry.contexts.insert(
                    key,
                    ContextEntry {
                        parent: Some(parent),
                        class_mask,
                        quotas,
                        _label: request.label,
                        expires_at,
                        revoked: false,
                    },
                );
                messages::ContextReady {
                    context_id: request.context_id,
                    class_mask,
                    quotas,
                    expiry_us: context_expiry_us(expires_at, now),
                }
            };
            let body = messages::context_ready(envelope.request_id, ready)
                .map_err(|_| bad("could not encode CONTEXT_READY"))?;
            writer
                .write_record(messages::CONTEXT_READY, ready.context_id, &body)
                .map_err(|_| bad("could not send CONTEXT_READY"))?;
        },
        messages::DELEGATE_CONTEXT => {
            if !negotiated(shared, session_id, messages::FEATURE_DELEGATED_CONTEXT_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "delegated contexts were not negotiated",
                    fatal: false,
                });
            }
            let (envelope, context_id) = messages::parse_object_id(&record.body, "context ID")
                .map_err(|_| bad("invalid DELEGATE_CONTEXT"))?;
            if record.object_id != context_id {
                return Err(bad("DELEGATE_CONTEXT object ID mismatch"));
            }
            let now = Instant::now();
            let mut capability = [0_u8; messages::CONTEXT_CAPABILITY_BYTES];
            getrandom::fill(&mut capability)
                .map_err(|_| bad("could not generate delegated capability"))?;
            let verifier: [u8; 32] = Sha256::digest(capability).into();
            {
                let mut registry = lock_registry(shared);
                let session = registry.sessions.get(&session_id).ok_or(ProtocolError {
                    code: messages::ERROR_CONTEXT_REVOKED,
                    message: "session context was revoked",
                    fatal: true,
                })?;
                let bound = ContextKey {
                    authority_root_session: session.authority_root_session,
                    context_id: session.bound_context_id,
                };
                if registry.capabilities.len() >= MAX_CONTEXT_CAPABILITIES {
                    capability.fill(0);
                    return Err(ProtocolError {
                        code: messages::ERROR_LIMIT_EXCEEDED,
                        message: "delegated capability quota exceeded",
                        fatal: false,
                    });
                }
                let target = ContextKey {
                    authority_root_session: session.authority_root_session,
                    context_id,
                };
                let (class_mask, quotas, expires_at, invalid) = match registry.contexts.get(&target)
                {
                    Some(context) => (
                        context.class_mask,
                        context.quotas,
                        context.expires_at,
                        context.revoked
                            || context.expires_at.is_some_and(|expires_at| expires_at <= now),
                    ),
                    None => {
                        capability.fill(0);
                        return Err(ProtocolError {
                            code: messages::ERROR_NOT_FOUND,
                            message: "context does not exist",
                            fatal: false,
                        });
                    },
                };
                if target == bound || !context_is_descendant(&registry, target, bound) || invalid {
                    capability.fill(0);
                    return Err(ProtocolError {
                        code: messages::ERROR_NOT_FOUND,
                        message: "context does not exist",
                        fatal: false,
                    });
                }
                registry.capabilities.push(CapabilityBinding {
                    verifier,
                    context: target,
                    class_mask,
                    quotas,
                    expires_at,
                });
            }
            let body = messages::context_capability(envelope.request_id, context_id, &capability);
            let sent = writer.write_record(messages::CONTEXT_CAPABILITY, context_id, &body);
            capability.fill(0);
            if sent.is_err() {
                lock_registry(shared).capabilities.retain(|binding| binding.verifier != verifier);
                return Err(bad("could not send CONTEXT_CAPABILITY"));
            }
        },
        messages::REVOKE_CONTEXT => {
            if !negotiated(shared, session_id, messages::FEATURE_DELEGATED_CONTEXT_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "delegated contexts were not negotiated",
                    fatal: false,
                });
            }
            let (envelope, context_id) = messages::parse_object_id(&record.body, "context ID")
                .map_err(|_| bad("invalid REVOKE_CONTEXT"))?;
            if record.object_id != context_id {
                return Err(bad("REVOKE_CONTEXT object ID mismatch"));
            }
            let (revoked_sessions, changed_writers) = {
                let mut registry = lock_registry(shared);
                let session = registry.sessions.get(&session_id).ok_or(ProtocolError {
                    code: messages::ERROR_CONTEXT_REVOKED,
                    message: "session context was revoked",
                    fatal: true,
                })?;
                let bound = ContextKey {
                    authority_root_session: session.authority_root_session,
                    context_id: session.bound_context_id,
                };
                let target = ContextKey {
                    authority_root_session: session.authority_root_session,
                    context_id,
                };
                if target == bound
                    || !registry.contexts.contains_key(&target)
                    || !context_is_descendant(&registry, target, bound)
                {
                    return Err(ProtocolError {
                        code: messages::ERROR_NOT_FOUND,
                        message: "context does not exist",
                        fatal: false,
                    });
                }
                let revoked = registry
                    .contexts
                    .keys()
                    .copied()
                    .filter(|key| context_is_descendant(&registry, *key, target))
                    .collect::<HashSet<_>>();
                for key in &revoked {
                    if let Some(context) = registry.contexts.get_mut(key) {
                        context.revoked = true;
                    }
                }
                registry.capabilities.retain(|binding| !revoked.contains(&binding.context));
                let mut revoked_sessions = Vec::new();
                let mut changed_writers = Vec::new();
                for (&candidate_id, candidate) in &mut registry.sessions {
                    if candidate.authority_root_session != target.authority_root_session {
                        continue;
                    }
                    let key = ContextKey {
                        authority_root_session: candidate.authority_root_session,
                        context_id: candidate.bound_context_id,
                    };
                    if revoked.contains(&key) {
                        candidate.revoked = true;
                        revoked_sessions.push((
                            candidate_id,
                            candidate.writer.upgrade(),
                            candidate.bound_context_id,
                        ));
                    } else if let Some(target_writer) = candidate.writer.upgrade() {
                        changed_writers.push(target_writer);
                    }
                }
                (revoked_sessions, changed_writers)
            };
            for (revoked_session, target_writer, revoked_context) in &revoked_sessions {
                if let Some(target_writer) = target_writer {
                    let _ = target_writer.write_record(
                        messages::INPUT_RESET,
                        0,
                        &messages::input_reset(),
                    );
                    let detail = messages::ErrorDetail::new();
                    let body = messages::error_with_detail(
                        0,
                        messages::ERROR_CONTEXT_REVOKED,
                        true,
                        &detail,
                        "delegated context revoked",
                    )
                    .map_err(|_| bad("could not encode CONTEXT_REVOKED"))?;
                    let _ = target_writer.write_record(messages::ERROR, *revoked_context, &body);
                }
                cleanup_revoked_session(shared, *revoked_session);
            }
            let authority_root_session = {
                lock_registry(shared)
                    .sessions
                    .get(&session_id)
                    .map(|session| session.authority_root_session)
                    .unwrap_or(session_id)
            };
            shared
                .scene
                .note_context_revocation(authority_root_session)
                .map_err(|_| bad("could not advance context-revocation revision"))?;
            let changed = messages::context_changed(messages::ContextChanged {
                context_id,
                state: messages::CONTEXT_STATE_REVOKED,
                reason_mask: messages::CONTEXT_CHANGED_EXPLICIT_REVOCATION,
            })
            .map_err(|_| bad("could not encode CONTEXT_CHANGED"))?;
            for target_writer in changed_writers {
                let _ = target_writer.write_record(messages::CONTEXT_CHANGED, context_id, &changed);
            }
            writer
                .write_ok(messages::OK, context_id, envelope.request_id)
                .map_err(|_| bad("could not acknowledge REVOKE_CONTEXT"))?;
        },
        messages::PING => {
            let envelope =
                messages::decode_control(&record.body).map_err(|_| bad("invalid PING"))?;
            if record.object_id != 0 || envelope.request_id == 0 {
                return Err(bad("PING is not a correlated session-level request"));
            }
            writer.write_pong(envelope.request_id).map_err(|_| bad("could not send PONG"))?;
        },
        messages::SET_OBSERVATION => {
            if !negotiated(shared, session_id, messages::FEATURE_OBSERVABILITY_CORE_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "observability was not negotiated",
                    fatal: false,
                });
            }
            let (envelope, mask) = messages::parse_set_observation(&record.body)
                .map_err(|_| bad("invalid SET_OBSERVATION"))?;
            if record.object_id != 0 {
                return Err(bad("SET_OBSERVATION must be session-level"));
            }
            observations.configure(mask, shared.scene.take_observation_snapshot(session_id));
            writer
                .write_ok(messages::OK, 0, envelope.request_id)
                .map_err(|_| bad("could not acknowledge SET_OBSERVATION"))?;
        },
        messages::QUERY_SOURCE => {
            if !negotiated(shared, session_id, messages::FEATURE_OBSERVABILITY_CORE_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "observability was not negotiated",
                    fatal: false,
                });
            }
            let (envelope, source_id) = messages::parse_query_source(&record.body)
                .map_err(|_| bad("invalid QUERY_SOURCE"))?;
            if record.object_id != source_id {
                return Err(bad("QUERY_SOURCE object ID mismatch"));
            }
            let status = shared
                .scene
                .source_status(
                    (session_id, source_id),
                    INITIAL_BYTE_CREDITS,
                    INITIAL_PACKET_CREDITS,
                )
                .ok_or(ProtocolError {
                    code: messages::ERROR_NOT_FOUND,
                    message: "source does not exist",
                    fatal: false,
                })?;
            let body = messages::source_status(envelope.request_id, &status)
                .map_err(|_| bad("could not encode SOURCE_STATUS"))?;
            writer
                .write_record(messages::SOURCE_STATUS, source_id, &body)
                .map_err(|_| bad("could not send SOURCE_STATUS"))?;
        },
        messages::QUERY_SCENE => {
            if !negotiated(shared, session_id, messages::FEATURE_OBSERVABILITY_CORE_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "observability was not negotiated",
                    fatal: false,
                });
            }
            let (envelope, query) = messages::parse_query_scene(&record.body)
                .map_err(|_| bad("invalid QUERY_SCENE"))?;
            if record.object_id != 0 {
                return Err(bad("QUERY_SCENE must be session-level"));
            }
            let mut status = match shared.scene.scene_status(session_id, &query) {
                Ok(status) => status,
                Err(precondition) => {
                    let mut detail = messages::ErrorDetail::new();
                    detail.insert_u64(
                        messages::ERROR_DETAIL_SCENE_REVISION,
                        precondition.current_revision.get(),
                    );
                    let body = messages::error_with_detail(
                        envelope.request_id,
                        messages::ERROR_PRECONDITION_FAILED,
                        false,
                        &detail,
                        "scene revision precondition failed",
                    )
                    .map_err(|_| bad("could not encode scene precondition error"))?;
                    writer
                        .write_record(messages::ERROR, 0, &body)
                        .map_err(|_| bad("could not send scene precondition error"))?;
                    return Ok(ControlAction::Continue);
                },
            };
            let first_offset = query.cursor.map_or(0, |cursor| cursor.offset);
            let body = loop {
                match messages::scene_status(envelope.request_id, &status) {
                    Ok(body) => break body,
                    Err(_) if status.nodes.len() > 1 => {
                        status.nodes.pop();
                        status.cursor = Some(messages::SceneCursor {
                            scene_revision: status.scene_revision,
                            offset: first_offset + status.nodes.len() as u64,
                        });
                    },
                    Err(_) => {
                        return Err(ProtocolError {
                            code: messages::ERROR_LIMIT_EXCEEDED,
                            message: "one scene node exceeds the status reply limit",
                            fatal: false,
                        });
                    },
                }
            };
            writer
                .write_record(messages::SCENE_STATUS, 0, &body)
                .map_err(|_| bad("could not send SCENE_STATUS"))?;
        },
        messages::QUERY_ANCHOR => {
            if !negotiated(shared, session_id, messages::FEATURE_OBSERVABILITY_CORE_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "observability was not negotiated",
                    fatal: false,
                });
            }
            let (envelope, anchor_id) = messages::parse_query_anchor(&record.body)
                .map_err(|_| bad("invalid QUERY_ANCHOR"))?;
            if record.object_id != anchor_id {
                return Err(bad("QUERY_ANCHOR object ID mismatch"));
            }
            let metrics = *lock_metrics(shared);
            let (_, display_offset) = *lock_render_state(shared);
            let status = shared.scene.anchor_status(
                session_id,
                anchor_id,
                metrics.columns,
                metrics.rows,
                display_offset,
                metrics.generation,
            );
            let body = messages::anchor_status(envelope.request_id, status)
                .map_err(|_| bad("could not encode ANCHOR_STATUS"))?;
            writer
                .write_record(messages::ANCHOR_STATUS, anchor_id, &body)
                .map_err(|_| bad("could not send ANCHOR_STATUS"))?;
        },
        messages::QUERY_LIMITS => {
            if !negotiated(shared, session_id, messages::FEATURE_OBSERVABILITY_CORE_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "observability was not negotiated",
                    fatal: false,
                });
            }
            let envelope = messages::parse_query_limits(&record.body)
                .map_err(|_| bad("invalid QUERY_LIMITS"))?;
            if record.object_id != 0 {
                return Err(bad("QUERY_LIMITS must be session-level"));
            }
            let counts = shared.scene.counts(session_id);
            let body = messages::limits_status(
                envelope.request_id,
                messages::LimitsStatus {
                    maximum_sources: 64,
                    maximum_nodes: messages::MAX_SCENE_NODES as u64,
                    maximum_transactions: MAX_TRANSACTIONS as u64,
                    maximum_anchors: messages::MAX_SCENE_NODES as u64,
                    maximum_control_body: u64::from(vivid_protocol::CONTROL_MAX_RECORD_BODY),
                    maximum_media_body: u64::from(vivid_protocol::HARD_MAX_RECORD_BODY),
                    maximum_waits: MAX_REGISTERED_WAITS as u64,
                    maximum_pending_requests: MAX_PENDING_REQUESTS as u64,
                    rolling_byte_window: INITIAL_BYTE_CREDITS,
                    rolling_packet_window: INITIAL_PACKET_CREDITS,
                    retained_pixel_budget: 8192 * 8192 * 2,
                    current_sources: counts.sources,
                    current_nodes: counts.nodes,
                    current_retained_pixels: counts.retained_pixels,
                    image_cache_budget: None,
                },
            )
            .map_err(|_| bad("could not encode LIMITS_STATUS"))?;
            writer
                .write_record(messages::LIMITS_STATUS, 0, &body)
                .map_err(|_| bad("could not send LIMITS_STATUS"))?;
        },
        messages::WAIT_SOURCE => {
            if !negotiated(shared, session_id, messages::FEATURE_OBSERVABILITY_CORE_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "observability was not negotiated",
                    fatal: false,
                });
            }
            let (envelope, wait) = messages::parse_wait_source(&record.body)
                .map_err(|_| bad("invalid WAIT_SOURCE"))?;
            if record.object_id != wait.source_id {
                return Err(bad("WAIT_SOURCE object ID mismatch"));
            }
            if waits.contains_key(&envelope.request_id) || pending.contains(envelope.request_id) {
                return Err(bad("request ID already has a registered wait"));
            }
            if waits.len() >= MAX_REGISTERED_WAITS {
                return Err(ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message: "source wait quota exceeded",
                    fatal: false,
                });
            }
            let timeout = Duration::from_micros(wait.timeout_us);
            if timeout > MAX_WAIT_TIMEOUT {
                return Err(ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message: "source wait timeout exceeds the presenter limit",
                    fatal: false,
                });
            }
            match shared.scene.evaluate_wait(
                (session_id, wait.source_id),
                wait.condition,
                wait.value,
            ) {
                SourceWaitEvaluation::Satisfied(satisfied) => {
                    let body = messages::wait_satisfied(envelope.request_id, satisfied)
                        .map_err(|_| bad("could not encode WAIT_SATISFIED"))?;
                    writer
                        .write_record(messages::WAIT_SATISFIED, wait.source_id, &body)
                        .map_err(|_| bad("could not send WAIT_SATISFIED"))?;
                },
                SourceWaitEvaluation::NotVisible => {
                    return Err(ProtocolError {
                        code: messages::ERROR_NOT_VISIBLE,
                        message: "source has no eligible visible placement",
                        fatal: false,
                    });
                },
                SourceWaitEvaluation::NotFound => {
                    return Err(ProtocolError {
                        code: messages::ERROR_NOT_FOUND,
                        message: "source does not exist",
                        fatal: false,
                    });
                },
                SourceWaitEvaluation::Pending => {
                    waits.insert(
                        envelope.request_id,
                        RegisteredWait {
                            source_id: wait.source_id,
                            condition: wait.condition,
                            value: wait.value,
                            deadline: Instant::now() + timeout,
                        },
                    );
                },
            }
        },
        messages::CANCEL_WAIT => {
            if !negotiated(shared, session_id, messages::FEATURE_OBSERVABILITY_CORE_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "observability was not negotiated",
                    fatal: false,
                });
            }
            let (envelope, wait_request_id) = messages::parse_cancel_wait(&record.body)
                .map_err(|_| bad("invalid CANCEL_WAIT"))?;
            if record.object_id != 0 {
                return Err(bad("CANCEL_WAIT must be session-level"));
            }
            writer
                .write_ok(messages::OK, 0, envelope.request_id)
                .map_err(|_| bad("could not acknowledge CANCEL_WAIT"))?;
            if let Some(wait) = waits.remove(&wait_request_id) {
                writer
                    .write_record(
                        messages::ERROR,
                        wait.source_id,
                        &messages::error(
                            wait_request_id,
                            messages::ERROR_CANCELLED,
                            "source wait was cancelled",
                        ),
                    )
                    .map_err(|_| bad("could not report cancelled source wait"))?;
            }
        },
        messages::PROBE_VIDEO_CONFIG => {
            let (envelope, config) = messages::parse_probe_video_config(&record.body)
                .map_err(|_| bad("invalid PROBE_VIDEO_CONFIG"))?;
            if record.object_id != 0 || config.source_id != 0 {
                return Err(bad("PROBE_VIDEO_CONFIG must be session-level"));
            }
            let request_id = envelope.request_id;
            let capability_generation = shared.capability_generation.load(Ordering::Acquire);
            spawn_pending_operation(
                pending,
                writer,
                request_id,
                0,
                "vivid-video-probe",
                move || {
                    let supported =
                        media::is_portable_packetization(&config.codec, &config.packetization)
                            && Decoder::new(&config).is_ok();
                    Ok(PendingReply {
                        record_type: messages::VIDEO_SUPPORT,
                        object_id: 0,
                        body: messages::capability_support(
                            request_id,
                            supported,
                            &config.codec,
                            capability_generation,
                        ),
                    })
                },
            )?;
        },
        messages::PROBE_AUDIO_CONFIG => {
            if !negotiated(shared, session_id, messages::FEATURE_AUDIO_ACCESS_UNIT_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "audio access units were not negotiated",
                    fatal: false,
                });
            }
            let (envelope, config) = messages::parse_probe_audio_config(&record.body)
                .map_err(|_| bad("invalid PROBE_AUDIO_CONFIG"))?;
            if record.object_id != 0 || config.source_id != 0 {
                return Err(bad("PROBE_AUDIO_CONFIG must be session-level"));
            }
            let request_id = envelope.request_id;
            let capability_generation = shared.capability_generation.load(Ordering::Acquire);
            spawn_pending_operation(
                pending,
                writer,
                request_id,
                0,
                "vivid-audio-probe",
                move || {
                    let supported =
                        messages::audio_config_supported(&config) && audio::supports(&config);
                    Ok(PendingReply {
                        record_type: messages::AUDIO_SUPPORT,
                        object_id: 0,
                        body: messages::capability_support(
                            request_id,
                            supported,
                            &config.codec,
                            capability_generation,
                        ),
                    })
                },
            )?;
        },
        messages::CREATE_RASTER => {
            let (envelope, config, capture_policy, descriptor) =
                messages::parse_create_raster_with_extensions(&record.body)
                    .map_err(|_| bad("invalid CREATE_RASTER"))?;
            writer.mark_source_policy(config.source_id, capture_policy);
            if record.object_id != config.source_id {
                return Err(bad("CREATE_RASTER object ID mismatch"));
            }
            if !negotiated(shared, session_id, messages::FEATURE_RASTER_RGBA8)
                || (config.compression_mode == messages::COMPRESSION_RAW_OR_ZSTD
                    && !negotiated(shared, session_id, messages::FEATURE_RASTER_ZSTD_V1))
                || (config.alpha_mode == messages::ALPHA_PREMULTIPLIED
                    && !negotiated(
                        shared,
                        session_id,
                        messages::FEATURE_RASTER_PREMULTIPLIED_ALPHA,
                    ))
                || (envelope.payload.map_value(9).is_some()
                    && !negotiated(shared, session_id, messages::FEATURE_SOURCE_CAPTURE_POLICY_V1))
                || (envelope.payload.map_value(10).is_some()
                    && !negotiated(shared, session_id, messages::FEATURE_SOURCE_DESCRIPTOR_V1))
            {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "raster configuration uses a feature that was not negotiated",
                    fatal: false,
                });
            }
            let max_body =
                media::rgba8_raw_frame_body_len(config.width, config.height).map_err(|_| {
                    ProtocolError {
                        code: messages::ERROR_LIMIT_EXCEEDED,
                        message: "raster frame exceeds the media-body limit",
                        fatal: false,
                    }
                })?;
            enforce_context_source_capacity(
                shared,
                session_id,
                u64::from(config.width) * u64::from(config.height),
                max_body,
            )?;
            shared
                .scene
                .add_source_with_extensions(
                    session_id,
                    config.source_id,
                    SourceConfig::Raster(config.clone()),
                    capture_policy,
                    descriptor,
                )
                .map_err(|message| ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message,
                    fatal: false,
                })?;
            issue_source_ready(
                shared,
                writer,
                envelope.request_id,
                (session_id, config.source_id),
                ConnectionKind::Raster,
                max_body,
            )
            .map_err(|_| bad("could not create raster media ticket"))?;
        },
        messages::CREATE_VIDEO => {
            let (envelope, config, capture_policy, descriptor) =
                messages::parse_create_video_with_extensions(&record.body)
                    .map_err(|_| bad("invalid CREATE_VIDEO"))?;
            writer.mark_source_policy(config.source_id, capture_policy);
            if !negotiated(shared, session_id, messages::FEATURE_VIDEO_ACCESS_UNIT_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "portable video was not negotiated",
                    fatal: false,
                });
            }
            if config.source_id == 0
                || record.object_id != config.source_id
                || !media::is_portable_packetization(&config.codec, &config.packetization)
            {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_CONFIG,
                    message: "unsupported video packetization",
                    fatal: false,
                });
            }
            if envelope.payload.map_value(23).is_some()
                && !negotiated(shared, session_id, messages::FEATURE_SOURCE_CAPTURE_POLICY_V1)
            {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "source capture policy was not negotiated",
                    fatal: false,
                });
            }
            if envelope.payload.map_value(24).is_some()
                && !negotiated(shared, session_id, messages::FEATURE_SOURCE_DESCRIPTOR_V1)
            {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "source descriptors were not negotiated",
                    fatal: false,
                });
            }
            Decoder::new(&config).map_err(|_| ProtocolError {
                code: messages::ERROR_UNSUPPORTED_CONFIG,
                message: "video decoder configuration is unavailable",
                fatal: false,
            })?;
            let max_body =
                media::video_body_len(config.max_access_unit_bytes).map_err(|_| ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message: "maximum video access unit exceeds the media-body limit",
                    fatal: false,
                })?;
            enforce_context_source_capacity(
                shared,
                session_id,
                u64::from(config.width) * u64::from(config.height),
                max_body,
            )?;
            shared
                .scene
                .add_source_with_extensions(
                    session_id,
                    config.source_id,
                    SourceConfig::Video(config.clone()),
                    capture_policy,
                    descriptor,
                )
                .map_err(|message| ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message,
                    fatal: false,
                })?;
            issue_source_ready(
                shared,
                writer,
                envelope.request_id,
                (session_id, config.source_id),
                ConnectionKind::Video,
                max_body,
            )
            .map_err(|_| bad("could not create video media ticket"))?;
        },
        messages::CREATE_AUDIO => {
            if !negotiated(shared, session_id, messages::FEATURE_AUDIO_ACCESS_UNIT_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "audio access units were not negotiated",
                    fatal: false,
                });
            }
            let (envelope, config, capture_policy, descriptor) =
                messages::parse_create_audio_with_extensions(&record.body)
                    .map_err(|_| bad("invalid CREATE_AUDIO"))?;
            writer.mark_source_policy(config.source_id, capture_policy);
            if config.source_id == 0 || record.object_id != config.source_id {
                return Err(bad("CREATE_AUDIO object ID mismatch"));
            }
            if envelope.payload.map_value(12).is_some()
                && !negotiated(shared, session_id, messages::FEATURE_SOURCE_CAPTURE_POLICY_V1)
            {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "source capture policy was not negotiated",
                    fatal: false,
                });
            }
            if envelope.payload.map_value(13).is_some()
                && !negotiated(shared, session_id, messages::FEATURE_SOURCE_DESCRIPTOR_V1)
            {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "source descriptors were not negotiated",
                    fatal: false,
                });
            }
            if !messages::audio_config_supported(&config) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_CONFIG,
                    message: "unsupported audio codec, layout, or size",
                    fatal: false,
                });
            }
            if let Some(video_id) = config.linked_video_source_id
                && !matches!(
                    shared.scene.source_config((session_id, video_id)),
                    Some(SourceConfig::Video(_))
                )
            {
                return Err(ProtocolError {
                    code: messages::ERROR_NOT_FOUND,
                    message: "linked video source does not exist",
                    fatal: false,
                });
            }
            let max_body =
                media::audio_body_len(config.max_access_unit_bytes).map_err(|_| ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message: "maximum audio access unit exceeds the media-body limit",
                    fatal: false,
                })?;
            enforce_context_source_capacity(shared, session_id, 0, max_body)?;
            // Reserve the source in receive order before device opening yields to a worker. The
            // worker may finish out of order, but it only completes the admitted operation and
            // issues its ticket; it does not reorder the source-creation mutation.
            shared
                .scene
                .add_source_with_extensions(
                    session_id,
                    config.source_id,
                    SourceConfig::Audio(config.clone()),
                    capture_policy,
                    descriptor,
                )
                .map_err(|message| ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message,
                    fatal: false,
                })?;
            if let Err(error) = spawn_audio_source_open(
                pending,
                writer,
                shared,
                session_id,
                envelope.request_id,
                config,
                max_body,
            ) {
                let _ = shared.scene.remove_source((session_id, record.object_id));
                return Err(error);
            }
        },
        messages::CREATE_IMAGE => {
            let (envelope, config, capture_policy, descriptor) =
                messages::parse_create_image_with_extensions(&record.body)
                    .map_err(|_| bad("invalid CREATE_IMAGE"))?;
            writer.mark_source_policy(config.source_id, capture_policy);
            if !negotiated(shared, session_id, messages::FEATURE_ENCODED_IMAGE_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "encoded images were not negotiated",
                    fatal: false,
                });
            }
            if record.object_id != config.source_id {
                return Err(bad("CREATE_IMAGE object ID mismatch"));
            }
            if envelope.payload.map_value(9).is_some()
                && !negotiated(shared, session_id, messages::FEATURE_SOURCE_CAPTURE_POLICY_V1)
            {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "source capture policy was not negotiated",
                    fatal: false,
                });
            }
            if envelope.payload.map_value(10).is_some()
                && !negotiated(shared, session_id, messages::FEATURE_SOURCE_DESCRIPTOR_V1)
            {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "source descriptors were not negotiated",
                    fatal: false,
                });
            }
            if config.encoded_length > vivid_protocol::HARD_MAX_RECORD_BODY {
                return Err(ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message: "encoded image exceeds the media-body limit",
                    fatal: false,
                });
            }
            media::rgba8_pixel_len(config.width, config.height).map_err(|_| ProtocolError {
                code: messages::ERROR_LIMIT_EXCEEDED,
                message: "decoded image size is not representable",
                fatal: false,
            })?;
            enforce_context_source_capacity(
                shared,
                session_id,
                u64::from(config.width) * u64::from(config.height),
                config.encoded_length,
            )?;
            shared
                .scene
                .add_source_with_extensions(
                    session_id,
                    config.source_id,
                    SourceConfig::Image(config.clone()),
                    capture_policy,
                    descriptor,
                )
                .map_err(|message| ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message,
                    fatal: false,
                })?;
            issue_source_ready(
                shared,
                writer,
                envelope.request_id,
                (session_id, config.source_id),
                ConnectionKind::Blob,
                config.encoded_length,
            )
            .map_err(|_| bad("could not create image media ticket"))?;
        },
        messages::SET_SOURCE_POLICY => {
            if !negotiated(shared, session_id, messages::FEATURE_SOURCE_CAPTURE_POLICY_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "source capture policy was not negotiated",
                    fatal: false,
                });
            }
            let (envelope, source_id, requested) = messages::parse_set_source_policy(&record.body)
                .map_err(|_| bad("invalid SET_SOURCE_POLICY"))?;
            writer.mark_source_policy(source_id, requested);
            if record.object_id != source_id {
                return Err(bad("SET_SOURCE_POLICY object ID mismatch"));
            }
            shared.scene.tighten_source_policy((session_id, source_id), requested).map_err(
                |message| ProtocolError {
                    code: if message == "source does not exist" {
                        messages::ERROR_NOT_FOUND
                    } else {
                        messages::ERROR_BAD_STATE
                    },
                    message,
                    fatal: false,
                },
            )?;
            writer
                .write_ok(messages::OK, source_id, envelope.request_id)
                .map_err(|_| bad("could not acknowledge source policy"))?;
            wake(shared);
        },
        messages::UPDATE_SOURCE_DESCRIPTOR => {
            if !negotiated(shared, session_id, messages::FEATURE_SOURCE_DESCRIPTOR_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "source descriptors were not negotiated",
                    fatal: false,
                });
            }
            let (envelope, source_id, descriptor) =
                messages::parse_update_source_descriptor(&record.body)
                    .map_err(|_| bad("invalid UPDATE_SOURCE_DESCRIPTOR"))?;
            if record.object_id != source_id {
                return Err(bad("UPDATE_SOURCE_DESCRIPTOR object ID mismatch"));
            }
            shared.scene.update_source_descriptor((session_id, source_id), descriptor).map_err(
                |message| ProtocolError {
                    code: if message == "source does not exist" {
                        messages::ERROR_NOT_FOUND
                    } else {
                        messages::ERROR_BAD_STATE
                    },
                    message,
                    fatal: false,
                },
            )?;
            writer
                .write_ok(messages::OK, source_id, envelope.request_id)
                .map_err(|_| bad("could not acknowledge source descriptor"))?;
            wake(shared);
        },
        messages::DESTROY_SOURCE => {
            let (envelope, source_id) = messages::parse_object_id(&record.body, "source ID")
                .map_err(|_| bad("invalid DESTROY_SOURCE"))?;
            if record.object_id != source_id {
                return Err(bad("DESTROY_SOURCE object ID mismatch"));
            }
            shared.scene.remove_source((session_id, source_id)).map_err(|message| {
                ProtocolError { code: messages::ERROR_NOT_FOUND, message, fatal: false }
            })?;
            if let Some(output) = shared
                .audio_outputs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&(session_id, source_id))
            {
                output.stop();
            }
            lock_registry(shared)
                .tickets
                .retain(|_, ticket| ticket.source_key != (session_id, source_id));
            writer
                .write_ok(messages::OK, source_id, envelope.request_id)
                .map_err(|_| bad("could not acknowledge source destruction"))?;
            wake(shared);
        },
        messages::BEGIN_TXN => {
            let envelope =
                messages::decode_control(&record.body).map_err(|_| bad("invalid BEGIN_TXN"))?;
            let transaction_id =
                envelope.transaction_id.ok_or_else(|| bad("missing transaction ID"))?;
            if envelope.payload.map_value(0).and_then(vivid_protocol::cbor::Value::as_u64)
                != Some(transaction_id)
            {
                return Err(bad("BEGIN_TXN transaction ID mismatch"));
            }
            if transactions.contains_key(&transaction_id) {
                return Err(ProtocolError {
                    code: messages::ERROR_DUPLICATE_ID,
                    message: "transaction ID already exists",
                    fatal: false,
                });
            }
            if transactions.len() >= MAX_TRANSACTIONS {
                return Err(ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message: "transaction quota exceeded",
                    fatal: false,
                });
            }
            transactions.insert(transaction_id, Vec::new());
            writer
                .write_ok(messages::OK, 0, envelope.request_id)
                .map_err(|_| bad("could not acknowledge transaction"))?;
        },
        messages::CREATE_NODE => {
            let (envelope, config) =
                messages::parse_scene_node(&record.body).map_err(|_| bad("invalid CREATE_NODE"))?;
            if config.clip.is_some()
                && !negotiated(shared, session_id, messages::FEATURE_NODE_CLIP_RECT_V1)
            {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "node clipping was not negotiated",
                    fatal: false,
                });
            }
            if record.object_id != config.node.node_id || config.node.context_id != root_context_id
            {
                return Err(bad("CREATE_NODE object or context ID mismatch"));
            }
            let transaction_id =
                envelope.transaction_id.ok_or_else(|| bad("missing transaction ID"))?;
            let nodes = transactions.get_mut(&transaction_id).ok_or(ProtocolError {
                code: messages::ERROR_BAD_STATE,
                message: "transaction has not begun",
                fatal: false,
            })?;
            nodes.push(SceneMutation::Create(SceneNode::from_protocol(session_id, config)));
            writer
                .write_ok(messages::OK, record.object_id, envelope.request_id)
                .map_err(|_| bad("could not acknowledge node"))?;
        },
        messages::UPDATE_NODE => {
            let (envelope, config) = messages::parse_update_scene_node(&record.body)
                .map_err(|_| bad("invalid UPDATE_NODE"))?;
            if config.clip.is_some()
                && !negotiated(shared, session_id, messages::FEATURE_NODE_CLIP_RECT_V1)
            {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "node clipping was not negotiated",
                    fatal: false,
                });
            }
            if record.object_id != config.node.node_id || config.node.context_id != root_context_id
            {
                return Err(bad("UPDATE_NODE object or context ID mismatch"));
            }
            let transaction_id =
                envelope.transaction_id.ok_or_else(|| bad("missing transaction ID"))?;
            let mutations = transactions.get_mut(&transaction_id).ok_or(ProtocolError {
                code: messages::ERROR_BAD_STATE,
                message: "transaction has not begun",
                fatal: false,
            })?;
            mutations.push(SceneMutation::Update(SceneNode::from_protocol(session_id, config)));
            writer
                .write_ok(messages::OK, record.object_id, envelope.request_id)
                .map_err(|_| bad("could not acknowledge node update"))?;
        },
        messages::DELETE_NODE => {
            let (envelope, node_id) = messages::parse_object_id(&record.body, "node ID")
                .map_err(|_| bad("invalid DELETE_NODE"))?;
            if record.object_id != node_id {
                return Err(bad("DELETE_NODE object ID mismatch"));
            }
            let transaction_id =
                envelope.transaction_id.ok_or_else(|| bad("missing transaction ID"))?;
            let mutations = transactions.get_mut(&transaction_id).ok_or(ProtocolError {
                code: messages::ERROR_BAD_STATE,
                message: "transaction has not begun",
                fatal: false,
            })?;
            mutations.push(SceneMutation::Delete { session_id, node_id });
            writer
                .write_ok(messages::OK, record.object_id, envelope.request_id)
                .map_err(|_| bad("could not acknowledge node deletion"))?;
        },
        messages::COMMIT_TXN => {
            let envelope =
                messages::decode_control(&record.body).map_err(|_| bad("invalid COMMIT_TXN"))?;
            let metrics = *lock_metrics(shared);
            if envelope.expected_generation != Some(metrics.generation) {
                writer
                    .write_record(
                        messages::DISPLAY_CHANGED,
                        0,
                        &messages::display_changed(
                            0,
                            DisplayChanged {
                                display_generation: metrics.generation,
                                viewport_width: metrics.viewport_width,
                                viewport_height: metrics.viewport_height,
                                grid_columns: metrics.columns,
                                grid_rows: metrics.rows,
                                cell_width: metrics.cell_width,
                                cell_height: metrics.cell_height,
                                settled: lock_pending_display_change(shared).is_none(),
                            },
                        ),
                    )
                    .map_err(|_| bad("could not report current display generation"))?;
                return Err(ProtocolError {
                    code: messages::ERROR_STALE_DISPLAY_GENERATION,
                    message: "display generation is stale",
                    fatal: false,
                });
            }
            let transaction_id =
                envelope.transaction_id.ok_or_else(|| bad("missing transaction ID"))?;
            let nodes = transactions.remove(&transaction_id).ok_or(ProtocolError {
                code: messages::ERROR_BAD_STATE,
                message: "transaction has not begun",
                fatal: false,
            })?;
            let scene_revision =
                shared.scene.commit_mutations(session_id, nodes).map_err(|message| {
                    ProtocolError { code: messages::ERROR_BAD_STATE, message, fatal: false }
                })?;
            writer
                .write_record(
                    messages::PRESENTED,
                    0,
                    &messages::presented(envelope.request_id, scene_revision),
                )
                .map_err(|_| bad("could not acknowledge scene commit"))?;
            // A newly committed node may make a previously off-screen source visible (or vice
            // versa). Visibility is otherwise only recomputed on screen-swap/occlusion/scroll, so
            // without this a source evaluated as hidden before its node existed stays hidden.
            emit_visibility(shared);
            wake(shared);
        },
        messages::ABORT_TXN => {
            let (envelope, transaction_id) =
                messages::parse_object_id(&record.body, "transaction ID")
                    .map_err(|_| bad("invalid ABORT_TXN"))?;
            if envelope.transaction_id != Some(transaction_id) {
                return Err(bad("ABORT_TXN transaction ID mismatch"));
            }
            if transactions.remove(&transaction_id).is_none() {
                return Err(ProtocolError {
                    code: messages::ERROR_NOT_FOUND,
                    message: "transaction does not exist",
                    fatal: false,
                });
            }
            writer
                .write_ok(messages::OK, 0, envelope.request_id)
                .map_err(|_| bad("could not acknowledge transaction abort"))?;
        },
        messages::PLAY => {
            if !negotiated(shared, session_id, messages::FEATURE_VIDEO_CONTROL_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "video controls were not negotiated",
                    fatal: false,
                });
            }
            let (envelope, play) =
                messages::parse_play(&record.body).map_err(|_| bad("invalid PLAY"))?;
            let source_id = play.source_id;
            if source_id != record.object_id {
                return Err(bad("PLAY object ID mismatch"));
            }
            if !matches!(
                shared.scene.source_config((session_id, source_id)),
                Some(SourceConfig::Video(_) | SourceConfig::Audio(_))
            ) {
                return Err(ProtocolError {
                    code: messages::ERROR_BAD_STATE,
                    message: "PLAY applies only to video or audio",
                    fatal: false,
                });
            }
            shared.scene.start_playback((session_id, source_id), play).map_err(|message| {
                ProtocolError { code: messages::ERROR_NOT_FOUND, message, fatal: false }
            })?;
            for output in audio_group(shared, (session_id, source_id)) {
                output.configure_play(play.start_pts_us, play.minimum_buffer_us);
                output.start();
            }
            writer
                .write_ok(messages::OK, source_id, envelope.request_id)
                .map_err(|_| bad("could not acknowledge PLAY"))?;
        },
        messages::PAUSE => {
            if !negotiated(shared, session_id, messages::FEATURE_VIDEO_CONTROL_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "video controls were not negotiated",
                    fatal: false,
                });
            }
            let (envelope, source_id) = messages::parse_object_id(&record.body, "source ID")
                .map_err(|_| bad("invalid PAUSE"))?;
            if source_id != record.object_id {
                return Err(bad("PAUSE object ID mismatch"));
            }
            shared.scene.pause_playback((session_id, source_id)).map_err(|message| {
                ProtocolError { code: messages::ERROR_BAD_STATE, message, fatal: false }
            })?;
            for output in audio_group(shared, (session_id, source_id)) {
                output.pause();
            }
            writer
                .write_ok(messages::OK, source_id, envelope.request_id)
                .map_err(|_| bad("could not acknowledge PAUSE"))?;
        },
        messages::FLUSH => {
            if !negotiated(shared, session_id, messages::FEATURE_VIDEO_CONTROL_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "video controls were not negotiated",
                    fatal: false,
                });
            }
            let (envelope, source_id, epoch) =
                messages::parse_flush(&record.body).map_err(|_| bad("invalid FLUSH"))?;
            if source_id != record.object_id {
                return Err(bad("FLUSH object ID mismatch"));
            }
            let key = (session_id, source_id);
            let linked_audio = shared.scene.linked_audio_sources(key);
            shared.scene.flush_playback(key, epoch).map_err(|message| ProtocolError {
                code: messages::ERROR_BAD_STATE,
                message,
                fatal: false,
            })?;
            for audio_key in linked_audio {
                shared.scene.flush_playback(audio_key, epoch).map_err(|message| ProtocolError {
                    code: messages::ERROR_BAD_STATE,
                    message,
                    fatal: false,
                })?;
            }
            for output in audio_group(shared, key) {
                output.flush();
            }
            writer
                .write_ok(messages::OK, source_id, envelope.request_id)
                .map_err(|_| bad("could not acknowledge FLUSH"))?;
        },
        messages::EOS => {
            let (envelope, request) =
                messages::parse_eos(&record.body).map_err(|_| bad("invalid EOS"))?;
            if record.object_id != request.source_id {
                return Err(bad("EOS object ID mismatch"));
            }
            let key = (session_id, request.source_id);
            if let Some(barrier) = request.barrier {
                if !negotiated(shared, session_id, messages::FEATURE_MEDIA_ORDER_BARRIER_V1) {
                    return Err(ProtocolError {
                        code: messages::ERROR_UNSUPPORTED_FEATURE,
                        message: "media-order barrier was not negotiated",
                        fatal: false,
                    });
                }
                let observation = shared.scene.source_observation(key).ok_or(ProtocolError {
                    code: messages::ERROR_NOT_FOUND,
                    message: "source does not exist",
                    fatal: false,
                })?;
                if observation.attachment_generation != barrier.attachment_generation {
                    return Err(ProtocolError {
                        code: messages::ERROR_BAD_STATE,
                        message: "EOS attachment generation is not current",
                        fatal: false,
                    });
                }
                let scene = shared.scene.clone();
                let worker_shared = shared.clone();
                let source_id = request.source_id;
                let epoch = request.epoch;
                let request_id = envelope.request_id;
                spawn_pending_operation(
                    pending,
                    writer,
                    request_id,
                    source_id,
                    "vivid-media-order-barrier",
                    move || {
                        match scene.wait_media_barrier(
                            key,
                            barrier.attachment_generation,
                            barrier.final_record_sequence,
                            MEDIA_ORDER_BARRIER_TIMEOUT,
                        ) {
                            MediaBarrierWait::Accepted => apply_eos(&worker_shared, key, epoch)?,
                            MediaBarrierWait::TimedOut => {
                                return Err(ProtocolError {
                                    code: messages::ERROR_TIMEOUT,
                                    message: "EOS media-order barrier timed out",
                                    fatal: false,
                                });
                            },
                            MediaBarrierWait::AttachmentChanged
                            | MediaBarrierWait::AttachmentClosed
                            | MediaBarrierWait::SourceLost => {
                                return Err(ProtocolError {
                                    code: messages::ERROR_BAD_STATE,
                                    message: "EOS media attachment ended before the barrier",
                                    fatal: false,
                                });
                            },
                        }
                        Ok(PendingReply::ok(source_id, request_id))
                    },
                )?;
            } else {
                apply_eos(shared, key, request.epoch)?;
                writer
                    .write_ok(messages::OK, request.source_id, envelope.request_id)
                    .map_err(|_| bad("could not acknowledge EOS"))?;
            }
        },
        messages::DRAIN => {
            if !negotiated(shared, session_id, messages::FEATURE_AUDIO_ACCESS_UNIT_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "audio drain was not negotiated",
                    fatal: false,
                });
            }
            let (envelope, source_id) = messages::parse_object_id(&record.body, "source ID")
                .map_err(|_| bad("invalid DRAIN"))?;
            if record.object_id != source_id {
                return Err(bad("DRAIN object ID mismatch"));
            }
            let output = shared
                .audio_outputs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&(session_id, source_id))
                .cloned()
                .ok_or(ProtocolError {
                    code: messages::ERROR_NOT_FOUND,
                    message: "audio source does not exist",
                    fatal: false,
                })?;
            let request_id = envelope.request_id;
            spawn_pending_operation(
                pending,
                writer,
                request_id,
                source_id,
                "vivid-audio-drain",
                move || {
                    output.wait_drained().map_err(|_| ProtocolError {
                        code: messages::ERROR_DEVICE_LOST,
                        message: "audio output failed while draining",
                        fatal: false,
                    })?;
                    Ok(PendingReply::ok(source_id, request_id))
                },
            )?;
        },
        messages::GOODBYE => {
            let envelope =
                messages::decode_control(&record.body).map_err(|_| bad("invalid GOODBYE"))?;
            writer
                .write_ok(messages::OK, 0, envelope.request_id)
                .map_err(|_| bad("could not acknowledge GOODBYE"))?;
            return Ok(ControlAction::Goodbye);
        },
        _ if record.flags & RECORD_OPTIONAL != 0 => {},
        _ => {
            return Err(ProtocolError {
                code: messages::ERROR_UNSUPPORTED_FEATURE,
                message: "required Vivid opcode is unsupported",
                fatal: false,
            });
        },
    }
    Ok(ControlAction::Continue)
}

fn issue_source_ready(
    shared: &Arc<ServiceShared>,
    writer: &Arc<Writer>,
    request_id: u64,
    source_key: SourceKey,
    kind: ConnectionKind,
    max_media_body: u32,
) -> io::Result<()> {
    let body = prepare_source_ready(shared, request_id, source_key, kind, max_media_body)?;
    writer.write_record(messages::SOURCE_READY, source_key.1, &body)
}

fn prepare_source_ready(
    shared: &Arc<ServiceShared>,
    request_id: u64,
    source_key: SourceKey,
    kind: ConnectionKind,
    max_media_body: u32,
) -> io::Result<Vec<u8>> {
    let mut ticket_bytes = vec![0_u8; 32];
    getrandom::fill(&mut ticket_bytes)
        .map_err(|error| io::Error::other(format!("could not generate media ticket: {error}")))?;
    let media_byte_quota = session_quotas(shared, source_key.0)
        .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "control session is gone"))?
        .maximum_media_bytes;
    if u64::from(max_media_body) > media_byte_quota {
        return Err(io::Error::new(
            ErrorKind::OutOfMemory,
            "context media-byte quota is smaller than one packet",
        ));
    }
    let byte_credits = INITIAL_BYTE_CREDITS.min(media_byte_quota).max(u64::from(max_media_body));
    let initial_source_revision = shared
        .scene
        .source_observation(source_key)
        .ok_or_else(|| invalid("source disappeared before SOURCE_READY"))?
        .revision;
    lock_registry(shared)
        .tickets
        .insert(ticket_bytes.clone(), Ticket { session_id: source_key.0, source_key, kind });
    messages::source_ready_with_observability(
        request_id,
        &messages::SourceReady {
            source_id: source_key.1,
            media_ticket: ticket_bytes,
            byte_credits,
            packet_credits: INITIAL_PACKET_CREDITS,
            fragment_credits: 0,
            max_media_body,
            rolling_byte_window: byte_credits,
            rolling_packet_window: INITIAL_PACKET_CREDITS,
            initial_source_revision,
            media_connection_required: true,
            delta_operation_limit: None,
        },
    )
}

fn handle_media(
    reader: &mut Reader,
    kind: ConnectionKind,
    shared: Arc<ServiceShared>,
) -> io::Result<()> {
    let attach = reader.read_record()?;
    if attach.record_type != messages::ATTACH_CHANNEL {
        return Err(invalid("media channel must begin with ATTACH_CHANNEL"));
    }
    let ticket_bytes = messages::parse_attach_channel(&attach.body)?;
    let (ticket, writer) = {
        let mut registry = lock_registry(&shared);
        let ticket = registry.tickets.remove(&ticket_bytes).ok_or_else(|| {
            io::Error::new(ErrorKind::PermissionDenied, "invalid or reused media ticket")
        })?;
        if ticket.kind != kind || ticket.source_key.1 != attach.object_id {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "media ticket channel mismatch",
            ));
        }
        let session = registry
            .sessions
            .get_mut(&ticket.session_id)
            .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "control session is gone"))?;
        if session.active_media_connections >= session.context_quotas.maximum_media_connections {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "delegated context media connection quota exceeded",
            ));
        }
        let writer = session
            .writer
            .upgrade()
            .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "control session is gone"))?;
        session.active_media_connections = session.active_media_connections.saturating_add(1);
        (ticket, writer)
    };
    let _active_media =
        ActiveMediaConnection { shared: shared.clone(), session_id: ticket.session_id };
    if let Some(policy) = shared.scene.source_capture_policy(ticket.source_key) {
        reader.mark_source_policy(ticket.source_key.1, policy);
    }
    shared.scene.mark_attached(ticket.source_key).map_err(invalid)?;
    wake(&shared);
    let max_media_body = match shared.scene.source_config(ticket.source_key) {
        Some(SourceConfig::Raster(config)) => {
            media::rgba8_raw_frame_body_len(config.width, config.height)
                .map_err(|_| invalid("invalid raster source size"))?
        },
        Some(SourceConfig::Video(config)) => media::video_body_len(config.max_access_unit_bytes)
            .map_err(|_| invalid("invalid video source size"))?,
        Some(SourceConfig::Image(config)) => config.encoded_length,
        Some(SourceConfig::Audio(config)) => media::audio_body_len(config.max_access_unit_bytes)
            .map_err(|_| invalid("invalid audio source size"))?,
        None => return Err(invalid("media ticket references a missing source")),
    };
    reader.set_maximum(max_media_body);

    let source_key = ticket.source_key;
    let result = match kind {
        ConnectionKind::Raster => handle_raster(reader, &shared, &writer, ticket.source_key),
        ConnectionKind::Video => handle_video(reader, &shared, &writer, ticket.source_key),
        ConnectionKind::Blob => handle_image(reader, &shared, &writer, ticket.source_key),
        ConnectionKind::Audio => handle_audio(reader, &shared, &writer, ticket.source_key),
        _ => unreachable!(),
    };
    if let Err(error) = result {
        if let Some(output) = shared
            .audio_outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&source_key)
        {
            output.stop();
        }
        let detailed_diagnostic = error.to_string();
        let device_lost =
            kind == ConnectionKind::Audio && error.kind() == io::ErrorKind::NotConnected;
        let code = if device_lost {
            messages::ERROR_DEVICE_LOST
        } else if detailed_diagnostic.contains("hash mismatch") {
            messages::ERROR_HASH_MISMATCH
        } else {
            messages::ERROR_DECODER
        };
        if device_lost {
            update_audio_device_availability(&shared, false);
        }
        let reduce_diagnostics = shared
            .scene
            .source_capture_policy(source_key)
            .is_some_and(|policy| policy & messages::CAPTURE_POLICY_REDUCE_DIAGNOSTICS != 0);
        let diagnostic = if reduce_diagnostics {
            String::from("source processing failed")
        } else {
            detailed_diagnostic
        };
        let final_revision = shared
            .scene
            .lose_source(source_key, code)
            .map(|observation| observation.revision)
            .unwrap_or(vivid_protocol::revision::SourceRevision::ZERO);
        let _ = writer.write_record(
            messages::SOURCE_LOST,
            source_key.1,
            &messages::source_lost_with_observability(
                source_key.1,
                code,
                &diagnostic,
                final_revision,
                &messages::ErrorDetail::new(),
            )
            .unwrap_or_else(|_| messages::source_lost(source_key.1, code, &diagnostic)),
        );
        wake(&shared);
        return Ok(());
    }
    let _ = shared.scene.mark_attachment_closed(source_key);
    Ok(())
}

fn handle_raster(
    reader: &mut Reader,
    shared: &Arc<ServiceShared>,
    writer: &Arc<Writer>,
    key: SourceKey,
) -> io::Result<()> {
    let config = match shared.scene.source_config(key) {
        Some(SourceConfig::Raster(config)) => config,
        _ => return Err(invalid("raster ticket references a non-raster source")),
    };
    let mut sequence = media::MediaSequence::default();
    let mut body = Vec::new();
    loop {
        let record = match reader.read_record_into(&mut body) {
            Ok(record) => record,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        let _charge = ChargedBody::new(writer, key.1, record.body.len() as u64);
        if record.record_type != messages::RASTER_FRAME || record.object_id != key.1 {
            return Err(invalid("unexpected record on raster media channel"));
        }
        let raster = media::parse_full_raster_frame(record.body)?;
        sequence.accept(raster.frame_id, raster.epoch)?;
        if (raster.width, raster.height) != (config.width, config.height) {
            return Err(invalid("raster frame dimensions differ from source"));
        }
        if raster.compressed && config.compression_mode != messages::COMPRESSION_RAW_OR_ZSTD {
            return Err(invalid("zstd raster was not enabled for the source"));
        }
        let pixels = media::decode_raster_pixels(raster)?;
        if config.alpha_mode == messages::ALPHA_PREMULTIPLIED
            && pixels
                .chunks_exact(4)
                .any(|pixel| pixel[0] > pixel[3] || pixel[1] > pixel[3] || pixel[2] > pixel[3])
        {
            return Err(invalid("premultiplied raster color exceeds alpha"));
        }
        shared
            .scene
            .publish_frame(
                key,
                raster.epoch,
                Frame {
                    frame_id: raster.frame_id,
                    pts_us: raster.pts_us,
                    width: raster.width,
                    height: raster.height,
                    rgba: Arc::from(pixels),
                    alpha_mode: config.alpha_mode,
                    sar_num: 1,
                    sar_den: 1,
                },
            )
            .map_err(invalid)?;
        shared
            .scene
            .mark_media_accepted(key, raster.epoch, raster.frame_id, record.sequence, false)
            .map_err(invalid)?;
        wake(shared);
    }
}

const MAX_QUEUED_VIDEO_FRAMES: usize = 32;

struct QueuedVideoFrame {
    epoch: u32,
    frame: Option<Frame>,
    pixels: u64,
    scene: SharedScene,
}

impl Drop for QueuedVideoFrame {
    fn drop(&mut self) {
        if self.pixels != 0 {
            self.scene.release_queued_pixels(self.pixels);
        }
    }
}

fn queue_decoded_video_frame(
    shared: &Arc<ServiceShared>,
    key: SourceKey,
    epoch: u32,
    config: &messages::ParsedVideoSourceConfig,
    frame_id: &mut u64,
    decoded: DecodedFrame,
    pending: &mut VecDeque<QueuedVideoFrame>,
) -> io::Result<()> {
    shared.scene.observe_buffered_pts(key, decoded.pts_us).map_err(invalid)?;
    shared.scene.mark_decoded_output(key, decoded.pts_us).map_err(invalid)?;
    *frame_id = frame_id.saturating_add(1);
    let pixels = u64::from(decoded.width)
        .checked_mul(u64::from(decoded.height))
        .ok_or_else(|| invalid("decoded frame pixel count overflow"))?;
    if !shared.scene.reserve_queued_pixels(pixels) {
        return Err(io::Error::new(
            ErrorKind::OutOfMemory,
            "aggregate queued video-frame quota exceeded",
        ));
    }
    pending.push_back(QueuedVideoFrame {
        epoch,
        frame: Some(Frame {
            frame_id: *frame_id,
            pts_us: decoded.pts_us,
            width: decoded.width,
            height: decoded.height,
            rgba: Arc::from(decoded.rgba),
            alpha_mode: messages::ALPHA_STRAIGHT,
            sar_num: config.sar_num,
            sar_den: config.sar_den,
        }),
        pixels,
        scene: shared.scene.clone(),
    });
    Ok(())
}

fn present_ready_video_frames(
    shared: &Arc<ServiceShared>,
    key: SourceKey,
    pending: &mut VecDeque<QueuedVideoFrame>,
) -> io::Result<bool> {
    loop {
        let Some(queued) = pending.front() else {
            return Ok(true);
        };
        let frame = queued.frame.as_ref().unwrap();
        match media_time_reached(shared, key, frame.pts_us) {
            None => return Ok(false),
            Some(false) => return Ok(true),
            Some(true) => {},
        }
        let mut queued = pending.pop_front().unwrap();
        let frame = queued.frame.take().unwrap();
        if shared.scene.is_before_play_start(key, frame.pts_us) {
            continue;
        }
        let pixels = std::mem::take(&mut queued.pixels);
        shared.scene.publish_queued_frame(key, queued.epoch, frame, pixels).map_err(invalid)?;
        wake(shared);
    }
}

fn handle_video(
    reader: &mut Reader,
    shared: &Arc<ServiceShared>,
    writer: &Arc<Writer>,
    key: SourceKey,
) -> io::Result<()> {
    let config = match shared.scene.source_config(key) {
        Some(SourceConfig::Video(config)) => config,
        _ => return Err(invalid("video ticket references a non-video source")),
    };
    let mut decoder = Decoder::new(&config)?;
    shared.scene.mark_decoder_initialized(key).map_err(invalid)?;
    let mut current_epoch = None;
    let mut sequence = media::MediaSequence::default();
    let mut frame_id = 0_u64;
    let mut pending = VecDeque::with_capacity(MAX_QUEUED_VIDEO_FRAMES);
    let mut body = Vec::new();
    loop {
        if !present_ready_video_frames(shared, key, &mut pending)? {
            return Ok(());
        }
        if pending.len() >= MAX_QUEUED_VIDEO_FRAMES {
            thread::sleep(Duration::from_millis(2));
            continue;
        }
        if !reader.wait_readable(Duration::from_millis(10))? {
            if shared.scene.eos_epoch(key).is_some() {
                break;
            }
            continue;
        }
        let record = match reader.read_record_into(&mut body) {
            Ok(record) => record,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        let _charge = ChargedBody::new(writer, key.1, record.body.len() as u64);
        if record.record_type != messages::VIDEO_PACKET || record.object_id != key.1 {
            return Err(invalid("unexpected record on video media channel"));
        }
        let packet = media::parse_video_packet(record.body)?;
        sequence.accept(packet.packet_id, packet.epoch)?;
        if packet.epoch < shared.scene.source_epoch(key).unwrap_or(packet.epoch) {
            continue;
        }
        if !packet.side_data.is_empty() || packet.data.len() > config.max_access_unit_bytes as usize
        {
            return Err(invalid("portable video packet violates its declared bounds"));
        }
        media::validate_portable_packetization(&config.codec, &config.packetization, packet.data)?;
        if media::access_unit_is_key(&config.codec, packet.data)?
            != (packet.flags & VIDEO_PACKET_KEY != 0)
        {
            return Err(invalid("video key/delta flag disagrees with codec syntax"));
        }
        match current_epoch {
            None if packet.flags & VIDEO_PACKET_KEY == 0 => {
                writer.write_record(
                    messages::NEED_KEYFRAME,
                    key.1,
                    &messages::need_keyframe(
                        key.1,
                        packet.epoch,
                        messages::KEYFRAME_REASON_INITIAL,
                        None,
                    ),
                )?;
                continue;
            },
            Some(epoch) if packet.epoch < epoch => {
                continue;
            },
            Some(epoch) if packet.epoch > epoch && packet.flags & VIDEO_PACKET_KEY == 0 => {
                writer.write_record(
                    messages::NEED_KEYFRAME,
                    key.1,
                    &messages::need_keyframe(
                        key.1,
                        packet.epoch,
                        messages::KEYFRAME_REASON_EPOCH_DISCONTINUITY,
                        Some(packet.packet_id),
                    ),
                )?;
                continue;
            },
            Some(epoch) if packet.epoch > epoch => decoder = Decoder::new(&config)?,
            _ => {},
        }
        current_epoch = Some(packet.epoch);
        let epoch = packet.epoch;
        let packet_id = packet.packet_id;
        let random_access = packet.flags & VIDEO_PACKET_KEY != 0;
        let decoded_frames = match decoder.push(packet) {
            Ok(frames) => frames,
            Err(_) => {
                let minimum_epoch = epoch.saturating_add(1);
                writer.write_record(
                    messages::NEED_KEYFRAME,
                    key.1,
                    &messages::need_keyframe(
                        key.1,
                        minimum_epoch,
                        messages::KEYFRAME_REASON_DECODER_ERROR,
                        Some(packet.packet_id),
                    ),
                )?;
                decoder = Decoder::new(&config)?;
                current_epoch = None;
                continue;
            },
        };
        shared
            .scene
            .mark_media_accepted(key, epoch, packet_id, record.sequence, random_access)
            .map_err(invalid)?;
        for decoded in decoded_frames {
            while pending.len() >= MAX_QUEUED_VIDEO_FRAMES {
                if !present_ready_video_frames(shared, key, &mut pending)? {
                    return Ok(());
                }
                if pending.len() >= MAX_QUEUED_VIDEO_FRAMES {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            queue_decoded_video_frame(
                shared,
                key,
                epoch,
                &config,
                &mut frame_id,
                decoded,
                &mut pending,
            )?;
        }
    }
    for decoded in decoder.finish()? {
        while pending.len() >= MAX_QUEUED_VIDEO_FRAMES {
            if !present_ready_video_frames(shared, key, &mut pending)? {
                return Ok(());
            }
            if pending.len() >= MAX_QUEUED_VIDEO_FRAMES {
                thread::sleep(Duration::from_millis(2));
            }
        }
        queue_decoded_video_frame(
            shared,
            key,
            current_epoch.unwrap_or(1),
            &config,
            &mut frame_id,
            decoded,
            &mut pending,
        )?;
    }
    while !pending.is_empty() {
        if !present_ready_video_frames(shared, key, &mut pending)? {
            break;
        }
        if !pending.is_empty() {
            thread::sleep(Duration::from_millis(2));
        }
    }
    if pending.is_empty() && shared.scene.eos_epoch(key).is_some() {
        shared.scene.mark_playback_ended(key).map_err(invalid)?;
    }
    Ok(())
}

fn handle_audio(
    reader: &mut Reader,
    shared: &Arc<ServiceShared>,
    writer: &Arc<Writer>,
    key: SourceKey,
) -> io::Result<()> {
    let config = match shared.scene.source_config(key) {
        Some(SourceConfig::Audio(config)) => config,
        _ => return Err(invalid("audio ticket references a non-audio source")),
    };
    let output = shared
        .audio_outputs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
        .cloned()
        .ok_or_else(|| invalid("audio output is missing"))?;
    let result = (|| {
        let mut decoder = output.decoder(&config)?;
        shared.scene.mark_decoder_initialized(key).map_err(invalid)?;
        let mut sequence = media::MediaSequence::default();
        let mut decoder_epoch = None;
        let mut body = Vec::new();
        loop {
            if !reader.wait_readable(Duration::from_millis(50))? {
                if shared.scene.eos_epoch(key).is_some() {
                    break;
                }
                continue;
            }
            let record = match reader.read_record_into(&mut body) {
                Ok(record) => record,
                Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            };
            let _charge = ChargedBody::new(writer, key.1, record.body.len() as u64);
            if record.record_type != messages::AUDIO_PACKET || record.object_id != key.1 {
                return Err(invalid("unexpected record on audio media channel"));
            }
            let packet = media::parse_audio_packet(record.body)?;
            sequence.accept(packet.packet_id, packet.epoch)?;
            if packet.epoch < shared.scene.source_epoch(key).unwrap_or(packet.epoch) {
                continue;
            }
            if decoder_epoch != Some(packet.epoch) {
                decoder = output.decoder(&config)?;
                decoder_epoch = Some(packet.epoch);
            }
            if packet.data.len() > config.max_access_unit_bytes as usize {
                return Err(invalid("audio packet exceeds its declared bound"));
            }
            let packet_id = packet.packet_id;
            let epoch = packet.epoch;
            let pts_us = packet.pts_us;
            let duration_us = packet.duration_us;
            let mut samples = decoder.push(packet)?;
            shared
                .scene
                .mark_media_accepted(key, epoch, packet_id, record.sequence, false)
                .map_err(invalid)?;
            output.trim_before_start(pts_us, duration_us, &mut samples);
            if !samples.is_empty() {
                shared.scene.mark_decoded_output(key, pts_us).map_err(invalid)?;
                output.observe_audio_pts(pts_us);
                output.push(&samples)?;
            }
        }
        output.push(&decoder.finish()?)?;
        Ok(())
    })();
    output.finish_decode();
    result
}

fn handle_image(
    reader: &mut Reader,
    shared: &Arc<ServiceShared>,
    writer: &Arc<Writer>,
    key: SourceKey,
) -> io::Result<()> {
    let config = match shared.scene.source_config(key) {
        Some(SourceConfig::Image(config)) => config,
        _ => return Err(invalid("image ticket references a non-image source")),
    };
    let mut body = Vec::new();
    let record = reader.read_record_into(&mut body)?;
    let _charge = ChargedBody::new(writer, key.1, record.body.len() as u64);
    if record.record_type != messages::IMAGE_DATA
        || record.object_id != key.1
        || record.body.len() != config.encoded_length as usize
    {
        return Err(invalid("invalid IMAGE_DATA record"));
    }
    if let Some(expected) = config.sha256 {
        let actual: [u8; 32] = Sha256::digest(record.body).into();
        if actual != expected {
            return Err(invalid("encoded image hash mismatch"));
        }
    }
    if encoded_image_has_multiple_pictures(config.encoding, record.body)? {
        return Err(invalid("animated or multi-picture image is not supported"));
    }
    let format = if config.encoding == messages::IMAGE_PNG {
        image::ImageFormat::Png
    } else {
        image::ImageFormat::Jpeg
    };
    let decoded_bytes = u64::from(config.width)
        .checked_mul(u64::from(config.height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| invalid("decoded image size overflow"))?;
    let mut image_reader = image::ImageReader::with_format(Cursor::new(record.body), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(config.width);
    limits.max_image_height = Some(config.height);
    limits.max_alloc = Some(decoded_bytes.saturating_add(16 * 1024 * 1024));
    image_reader.limits(limits);
    let decoded = image_reader.decode().map_err(|_| invalid("encoded image decoder failed"))?;
    shared.scene.mark_decoder_initialized(key).map_err(invalid)?;
    if decoded.dimensions() != (config.width, config.height) {
        return Err(invalid("decoded image dimensions differ from declaration"));
    }
    let rgba = decoded.into_rgba8().into_raw();
    shared
        .scene
        .publish_frame(
            key,
            1,
            Frame {
                frame_id: 1,
                pts_us: 0,
                width: config.width,
                height: config.height,
                rgba: Arc::from(rgba),
                alpha_mode: messages::ALPHA_STRAIGHT,
                sar_num: 1,
                sar_den: 1,
            },
        )
        .map_err(invalid)?;
    shared.scene.mark_media_accepted(key, 1, 1, record.sequence, false).map_err(invalid)?;
    wake(shared);
    Ok(())
}

fn encoded_image_has_multiple_pictures(encoding: u64, data: &[u8]) -> io::Result<bool> {
    if encoding == messages::IMAGE_PNG {
        if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(invalid("invalid PNG signature"));
        }
        let mut offset = 8_usize;
        while offset < data.len() {
            let header =
                data.get(offset..offset + 8).ok_or_else(|| invalid("truncated PNG chunk"))?;
            let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
            let chunk_type = &header[4..8];
            let end = offset
                .checked_add(12)
                .and_then(|value| value.checked_add(length))
                .filter(|value| *value <= data.len())
                .ok_or_else(|| invalid("PNG chunk exceeds image body"))?;
            if chunk_type == b"acTL" {
                return Ok(true);
            }
            offset = end;
            if chunk_type == b"IEND" {
                return Ok(false);
            }
        }
        return Err(invalid("PNG has no IEND chunk"));
    }

    if !data.starts_with(&[0xff, 0xd8]) {
        return Err(invalid("invalid JPEG signature"));
    }
    let mut offset = 2_usize;
    while offset < data.len() {
        while data.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *data.get(offset).ok_or_else(|| invalid("truncated JPEG marker"))?;
        offset += 1;
        if marker == 0xda || marker == 0xd9 {
            return Ok(false);
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length_bytes =
            data.get(offset..offset + 2).ok_or_else(|| invalid("truncated JPEG segment"))?;
        let length = usize::from(u16::from_be_bytes(length_bytes.try_into().unwrap()));
        if length < 2 {
            return Err(invalid("invalid JPEG segment length"));
        }
        let end = offset
            .checked_add(length)
            .filter(|value| *value <= data.len())
            .ok_or_else(|| invalid("JPEG segment exceeds image body"))?;
        if marker == 0xe2
            && data.get(offset + 2..end).is_some_and(|body| body.starts_with(b"MPF\0"))
        {
            return Ok(true);
        }
        offset = end;
    }
    Err(invalid("JPEG has no scan or end marker"))
}

fn return_credit(writer: &Writer, source_id: u64, bytes: u64) -> io::Result<()> {
    writer.write_credit(source_id, bytes, 1, 0)
}

/// Owns ingress storage and its one bounded queue slot. Dropping it makes both reusable and emits
/// the corresponding credit exactly once, including every error and early-return path.
struct ChargedBody<'a> {
    writer: &'a Writer,
    source_id: u64,
    bytes: u64,
}

impl<'a> ChargedBody<'a> {
    fn new(writer: &'a Writer, source_id: u64, bytes: u64) -> Self {
        Self { writer, source_id, bytes }
    }
}

impl Drop for ChargedBody<'_> {
    fn drop(&mut self) {
        let _ = return_credit(self.writer, self.source_id, self.bytes);
    }
}

struct ActiveConnection<'a>(&'a AtomicUsize);

impl Drop for ActiveConnection<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ActiveMediaConnection {
    shared: Arc<ServiceShared>,
    session_id: SessionId,
}

impl Drop for ActiveMediaConnection {
    fn drop(&mut self) {
        if let Some(session) = lock_registry(&self.shared).sessions.get_mut(&self.session_id) {
            session.active_media_connections = session.active_media_connections.saturating_sub(1);
        }
    }
}

fn cleanup_session(shared: &Arc<ServiceShared>, session_id: SessionId) {
    let mut registry = lock_registry(shared);
    let root_authority = registry
        .sessions
        .get(&session_id)
        .filter(|session| session.authority_root_session == session_id)
        .map(|session| session.authority_root_session);
    let mut revoked_children = Vec::new();
    if let Some(root_authority) = root_authority {
        for (&candidate_id, candidate) in &mut registry.sessions {
            if candidate_id != session_id && candidate.authority_root_session == root_authority {
                candidate.revoked = true;
                revoked_children.push((
                    candidate_id,
                    candidate.writer.upgrade(),
                    candidate.bound_context_id,
                ));
            }
        }
        registry.contexts.retain(|key, _| key.authority_root_session != root_authority);
        registry
            .capabilities
            .retain(|binding| binding.context.authority_root_session != root_authority);
    }
    registry.sessions.remove(&session_id);
    registry.tickets.retain(|_, ticket| ticket.session_id != session_id);
    drop(registry);
    for (child_id, child_writer, context_id) in revoked_children {
        if let Some(child_writer) = child_writer {
            let _ = child_writer.write_record(messages::INPUT_RESET, 0, &messages::input_reset());
            if let Ok(body) = messages::error_with_detail(
                0,
                messages::ERROR_CONTEXT_REVOKED,
                true,
                &messages::ErrorDetail::new(),
                "root authority session closed",
            ) {
                let _ = child_writer.write_record(messages::ERROR, context_id, &body);
            }
        }
        cleanup_revoked_session(shared, child_id);
    }
    let outputs = {
        let mut outputs =
            shared.audio_outputs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let keys = outputs.keys().copied().filter(|key| key.0 == session_id).collect::<Vec<_>>();
        keys.into_iter().filter_map(|key| outputs.remove(&key)).collect::<Vec<_>>()
    };
    for output in outputs {
        output.stop();
    }
    if let Err(error) = shared.scene.detach_session(session_id) {
        log::error!("Could not advance scene revision during session cleanup: {error}");
    }
}

fn lock_registry(shared: &Arc<ServiceShared>) -> std::sync::MutexGuard<'_, Registry> {
    shared.registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_metrics(shared: &Arc<ServiceShared>) -> std::sync::MutexGuard<'_, DisplayMetrics> {
    shared.metrics.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_pending_display_change(
    shared: &Arc<ServiceShared>,
) -> std::sync::MutexGuard<'_, Option<PendingDisplayChange>> {
    shared.pending_display_change.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_render_state(shared: &Arc<ServiceShared>) -> std::sync::MutexGuard<'_, (bool, usize)> {
    shared.render_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Recompute per-source visibility from the current scene and the last render state reported by the
/// UI thread, emitting `VISIBILITY` records only for sources whose state changed. Callable from the
/// control-dispatcher thread so a source evaluated as hidden before its scene node existed becomes
/// visible once the node is committed; visibility is otherwise only recomputed on
/// screen-swap/occlusion/scroll.
fn emit_visibility(shared: &Arc<ServiceShared>) {
    let (renderable, display_offset) = *lock_render_state(shared);
    let metrics = *lock_metrics(shared);
    let Ok(states) = shared.scene.aggregate_visibility(
        metrics.columns,
        metrics.rows,
        display_offset,
        renderable,
    ) else {
        log::error!("Could not advance source revision while updating visibility");
        return;
    };
    let mut registry = lock_registry(shared);
    for ((session_id, source_id), visible, reasons) in states {
        let Some(session) = registry.sessions.get_mut(&session_id) else { continue };
        if !session.accepted_features.contains(&messages::FEATURE_VISIBILITY_EVENTS_V1) {
            continue;
        }
        if session.last_visibility.insert(source_id, visible) == Some(visible) {
            continue;
        }
        let Some(writer) = session.writer.upgrade() else { continue };
        let _ = writer.write_record(
            messages::VISIBILITY,
            source_id,
            &messages::visibility(source_id, visible, reasons, metrics.generation),
        );
    }
}

fn wake(shared: &ServiceShared) {
    (shared.wake)();
}

fn advance_capability_generation(shared: &Arc<ServiceShared>, reason_mask: u64) -> io::Result<u64> {
    if reason_mask == 0 || reason_mask & !messages::CAPS_CHANGE_REASON_MASK != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Vivid capability change reason",
        ));
    }
    let previous = shared
        .capability_generation
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| generation.checked_add(1))
        .map_err(|_| io::Error::other("Vivid capability generation exhausted"))?;
    let generation = previous + 1;
    let body = messages::caps_changed(generation, reason_mask)?;
    let writers = {
        let registry = lock_registry(shared);
        registry
            .sessions
            .values()
            .filter_map(|session| session.writer.upgrade())
            .collect::<Vec<_>>()
    };
    for writer in writers {
        let _ = writer.write_record(messages::CAPS_CHANGED, 0, &body);
    }
    Ok(generation)
}

fn update_audio_device_availability(shared: &Arc<ServiceShared>, available: bool) {
    if shared.audio_device_available.swap(available, Ordering::AcqRel) != available {
        let _ = advance_capability_generation(shared, messages::CAPS_CHANGE_DEVICE_AVAILABILITY);
    }
}

fn diagnostic_trace_guard(component: TraceComponent) -> io::Result<Option<TraceGuard>> {
    let Some(directory) = std::env::var_os("VIVID_DIAGNOSTIC_TRACE_DIR") else {
        return Ok(None);
    };
    let mut hint = [0_u8; 16];
    getrandom::fill(&mut hint)
        .map_err(|error| io::Error::other(format!("trace hint generation failed: {error}")))?;
    let path =
        std::path::PathBuf::from(directory).join(format!("vivido-{}.ndjson", std::process::id()));
    TraceGuard::file(&path, component, TraceHop::Presenter, hint).map(Some)
}

#[cfg(test)]
fn constant_time_token_eq(expected: &[u8; 32], candidate_hex: &[u8]) -> bool {
    let mut decoded = [0_u8; 32];
    let valid_length = candidate_hex.len() == 64;
    let mut valid_digits = true;
    for (index, byte) in decoded.iter_mut().enumerate() {
        let high = candidate_hex.get(index * 2).copied().unwrap_or(0);
        let low = candidate_hex.get(index * 2 + 1).copied().unwrap_or(0);
        let (high, high_valid) = unhex(high);
        let (low, low_valid) = unhex(low);
        *byte = (high << 4) | low;
        valid_digits &= high_valid & low_valid;
    }
    let difference = expected
        .iter()
        .zip(decoded)
        .fold(0_u8, |difference, (expected, actual)| difference | (expected ^ actual));
    valid_length & valid_digits & (difference == 0)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    output
}

#[cfg(test)]
fn unhex(byte: u8) -> (u8, bool) {
    match byte {
        b'0'..=b'9' => (byte - b'0', true),
        b'a'..=b'f' => (byte - b'a' + 10, true),
        b'A'..=b'F' => (byte - b'A' + 10, true),
        _ => (0, false),
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

#[cfg(unix)]
fn bind_local_listener() -> io::Result<(LocalListener, String, Option<TempDir>)> {
    let directory =
        tempfile::Builder::new().prefix("vivido-vivid-").tempdir_in(std::env::temp_dir())?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
    let socket_path = directory.path().join("endpoint.sock");
    let listener = UnixListener::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok((listener, format!("unix:{}", socket_path.display()), Some(directory)))
}

#[cfg(windows)]
fn bind_local_listener() -> io::Result<(LocalListener, String, Option<TempDir>)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    Ok((listener, format!("tcp:{address}"), None))
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd"))]
fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    let mut uid = 0;
    let mut gid = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(ErrorKind::PermissionDenied, "peer UID does not match"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if credentials.uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(ErrorKind::PermissionDenied, "peer UID does not match"));
    }
    Ok(())
}

#[cfg(all(
    unix,
    not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "linux"
    ))
))]
fn verify_peer(_stream: &UnixStream) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn verify_peer(stream: &TcpStream) -> io::Result<()> {
    if stream.peer_addr()?.ip().is_loopback() {
        Ok(())
    } else {
        Err(io::Error::new(ErrorKind::PermissionDenied, "Vivid peer is not local"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use vivid_protocol::media;
    use vivid_protocol::messages::{
        NodeConfig, parse_display_changed, parse_source_ready, parse_welcome,
    };
    use vivid_protocol::wire::{Connection, Endpoint};

    #[cfg(unix)]
    fn stream_pair() -> (LocalStream, LocalStream) {
        UnixStream::pair().unwrap()
    }

    #[cfg(windows)]
    fn stream_pair() -> (LocalStream, LocalStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    fn read_correlated(connection: &mut Connection, record_type: u16, request_id: u64) -> Record {
        loop {
            let record = connection.read_record().unwrap();
            if record.record_type == record_type
                && messages::request_id(&record.body).unwrap() == request_id
            {
                return record;
            }
        }
    }

    fn assert_record_excludes(record: &Record, secrets: &[(&str, &[u8])]) {
        for (name, secret) in secrets {
            assert!(
                !secret.is_empty()
                    && !record.body.windows(secret.len()).any(|window| window == *secret),
                "{name} leaked through record type {:#06x}",
                record.record_type
            );
        }
    }

    fn context_hello(request_id: u64, credential: &str, authentication_kind: u64) -> Vec<u8> {
        messages::try_encode_hello(
            request_id,
            &messages::HelloConfig {
                minimum_major: 1,
                minimum_minor: 1,
                maximum_major: 1,
                maximum_minor: 1,
                token: credential,
                producer: "context-test",
                producer_version: "1",
                required_features: &[
                    messages::FEATURE_RASTER_RGBA8,
                    messages::FEATURE_RASTER_ZSTD_V1,
                    messages::FEATURE_TEXT_ANCHORS_V2,
                    messages::FEATURE_OBSERVABILITY_CORE_V1,
                    messages::FEATURE_DELEGATED_CONTEXT_V1,
                ],
                optional_features: &[],
                maximum_record_body: vivid_protocol::CONTROL_MAX_RECORD_BODY,
                authentication_kind,
                preserved_fields: &[],
            },
        )
        .unwrap()
    }

    #[test]
    fn pending_operations_are_bounded_and_timeout_once() {
        let pending = PendingOperations::default();
        for request_id in 1..=MAX_PENDING_OPERATIONS as u64 {
            pending.register(request_id, request_id + 100, Duration::from_secs(1)).unwrap();
        }
        assert!(matches!(
            pending.register(1000, 1, Duration::from_secs(1)),
            Err(PendingRegisterError::Full)
        ));
        assert!(pending.complete(1));
        assert!(matches!(
            pending.register(2, 1, Duration::from_secs(1)),
            Err(PendingRegisterError::Duplicate)
        ));

        pending.register(1000, 77, Duration::ZERO).unwrap();
        assert_eq!(pending.expire(Instant::now()), vec![(1000, 77)]);
        assert!(pending.expire(Instant::now()).is_empty());
        assert!(!pending.complete(1000));
    }

    #[test]
    fn capability_generation_advances_without_mutating_accepted_features() {
        let (shared, output) = linked_av_shared();
        let (mut client, server) = stream_pair();
        client
            .write_all(&vivid_protocol::wire::encode_preface(ConnectionKind::Control, 4096))
            .unwrap();
        let (reader, _) = Reader::new(server).unwrap();
        let writer = Arc::new(reader.writer().unwrap());
        let accepted_features =
            HashSet::from([messages::FEATURE_RASTER_RGBA8, messages::FEATURE_VIDEO_ACCESS_UNIT_V1]);
        lock_registry(&shared).sessions.insert(
            1,
            SessionRuntime {
                writer: Arc::downgrade(&writer),
                tag: [0; 16],
                anchor_key: anchor::derive_key(&[0; 32], &[0; 16]),
                seen_anchors: HashSet::new(),
                last_visibility: HashMap::new(),
                accepted_features: accepted_features.clone(),
                authority_root_session: 1,
                bound_context_id: 1,
                context_class_mask: messages::CONTEXT_CLASS_MASK,
                context_quotas: root_context_quotas(),
                active_media_connections: 0,
                revoked: false,
            },
        );

        assert!(
            advance_capability_generation(&shared, messages::CAPS_CHANGE_REASON_MASK << 1).is_err()
        );
        assert_eq!(shared.capability_generation.load(Ordering::Acquire), 1);
        let generation =
            advance_capability_generation(&shared, messages::CAPS_CHANGE_DECODER_AVAILABILITY)
                .unwrap();
        assert_eq!(generation, 2);

        let mut header = [0; vivid_protocol::wire::HEADER_SIZE];
        client.read_exact(&mut header).unwrap();
        let header = vivid_protocol::wire::RecordHeader::decode(header);
        assert_eq!(header.record_type, messages::CAPS_CHANGED);
        assert_eq!(header.object_id, 0);
        let mut body = vec![0; header.body_length as usize];
        client.read_exact(&mut body).unwrap();
        assert_eq!(
            messages::parse_caps_changed(&body).unwrap(),
            messages::CapsChanged {
                capability_generation: 2,
                reason_mask: messages::CAPS_CHANGE_DECODER_AVAILABILITY,
            }
        );
        update_audio_device_availability(&shared, false);
        assert_eq!(shared.capability_generation.load(Ordering::Acquire), 3);
        update_audio_device_availability(&shared, false);
        assert_eq!(
            shared.capability_generation.load(Ordering::Acquire),
            3,
            "repeated device failures are not capability changes"
        );
        update_audio_device_availability(&shared, true);
        assert_eq!(shared.capability_generation.load(Ordering::Acquire), 4);
        assert_eq!(lock_registry(&shared).sessions[&1].accepted_features, accepted_features);
        output.stop();
    }

    #[test]
    fn stalled_drain_preserves_control_liveness_and_unrelated_credit() {
        let (shared, output) = linked_av_shared();
        let (mut client, server) = stream_pair();
        client
            .write_all(&vivid_protocol::wire::encode_preface(ConnectionKind::Control, 1024))
            .unwrap();
        let (reader, _) = Reader::new(server).unwrap();
        let writer = Arc::new(reader.writer().unwrap());
        lock_registry(&shared).sessions.insert(
            1,
            SessionRuntime {
                writer: Arc::downgrade(&writer),
                tag: [0; 16],
                anchor_key: anchor::derive_key(&[0; 32], &[0; 16]),
                seen_anchors: HashSet::new(),
                last_visibility: HashMap::new(),
                accepted_features: HashSet::from([
                    messages::FEATURE_AUDIO_ACCESS_UNIT_V1,
                    messages::FEATURE_VIDEO_CONTROL_V1,
                ]),
                authority_root_session: 1,
                bound_context_id: 1,
                context_class_mask: messages::CONTEXT_CLASS_MASK,
                context_quotas: root_context_quotas(),
                active_media_connections: 0,
                revoked: false,
            },
        );
        let pending = Arc::new(PendingOperations::default());
        let mut transactions = HashMap::new();
        let mut waits = HashMap::new();
        let mut observations = ObservationTracker::default();
        let drain_body = messages::drain(41, 11);
        dispatch_control(
            &Record {
                record_type: messages::DRAIN,
                flags: 0,
                object_id: 11,
                sequence: 1,
                body: drain_body,
            },
            1,
            1,
            &shared,
            &writer,
            &pending,
            &mut transactions,
            &mut waits,
            &mut observations,
        )
        .unwrap();
        assert_eq!(
            pending.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len(),
            1
        );

        writer.write_credit(12, 4096, 1, 0).unwrap();
        dispatch_control(
            &Record {
                record_type: messages::BEGIN_TXN,
                flags: 0,
                object_id: 0,
                sequence: 2,
                body: messages::begin_transaction(43, 7),
            },
            1,
            1,
            &shared,
            &writer,
            &pending,
            &mut transactions,
            &mut waits,
            &mut observations,
        )
        .unwrap();
        dispatch_control(
            &Record {
                record_type: messages::PING,
                flags: 0,
                object_id: 0,
                sequence: 3,
                body: messages::ping(42),
            },
            1,
            1,
            &shared,
            &writer,
            &pending,
            &mut transactions,
            &mut waits,
            &mut observations,
        )
        .unwrap();
        let mut replies = Vec::new();
        for _ in 0..3 {
            let mut header = [0; vivid_protocol::wire::HEADER_SIZE];
            client.read_exact(&mut header).unwrap();
            let header = vivid_protocol::wire::RecordHeader::decode(header);
            let mut body = vec![0; header.body_length as usize];
            client.read_exact(&mut body).unwrap();
            replies.push((
                header.record_type,
                header.object_id,
                messages::request_id(&body).unwrap(),
            ));
        }
        assert_eq!(
            replies,
            [(messages::CREDIT, 12, 0), (messages::OK, 0, 43), (messages::PONG, 0, 42),]
        );
        assert_eq!(
            pending.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len(),
            1,
            "the DRAIN must remain outstanding throughout unrelated work"
        );

        pending.cancel_all();
        output.stop();
    }

    #[test]
    fn observation_queue_is_bounded_coalesced_and_reports_gaps() {
        let mut tracker = ObservationTracker {
            mask: messages::OBSERVATION_CLASS_MASK,
            ..ObservationTracker::default()
        };
        for source_id in 1..=(MAX_OBSERVATION_QUEUE as u64 + 1) {
            let sequence = tracker.next_sequence().unwrap();
            tracker.push(QueuedObservation::Source {
                source_id,
                source_revision: SourceRevision::new(source_id),
                changed_fields: messages::SOURCE_CHANGED_LIFECYCLE,
                sequence,
                causation_id: None,
            });
        }
        assert_eq!(tracker.queue.len(), MAX_OBSERVATION_QUEUE);
        assert_eq!(tracker.source_gap, Some(ObservationSequence::new(1)));

        let sequence = tracker.next_sequence().unwrap();
        tracker.push(QueuedObservation::Source {
            source_id: MAX_OBSERVATION_QUEUE as u64 + 1,
            source_revision: SourceRevision::new(99),
            changed_fields: messages::SOURCE_CHANGED_MILESTONES,
            sequence,
            causation_id: None,
        });
        let latest = tracker.queue.back().copied().unwrap();
        assert!(matches!(
            latest,
            QueuedObservation::Source {
                source_revision,
                changed_fields,
                ..
            } if source_revision == SourceRevision::new(99)
                && changed_fields
                    == messages::SOURCE_CHANGED_LIFECYCLE
                        | messages::SOURCE_CHANGED_MILESTONES
        ));
        assert_eq!(tracker.queue.len(), MAX_OBSERVATION_QUEUE);
    }

    #[test]
    fn playback_observation_emits_transitions_not_clock_ticks() {
        let (shared, output) = linked_av_shared();
        let mut tracker = ObservationTracker::default();
        tracker
            .configure(messages::OBSERVATION_CLASS_MASK, shared.scene.take_observation_snapshot(1));

        thread::sleep(Duration::from_millis(2));
        tracker.collect(shared.scene.take_observation_snapshot(1)).unwrap();
        assert!(tracker.queue.is_empty(), "clock progress is not an observation transition");

        shared.scene.pause_playback((1, 10)).unwrap();
        tracker.collect(shared.scene.take_observation_snapshot(1)).unwrap();
        assert!(
            tracker
                .queue
                .iter()
                .any(|event| matches!(event, QueuedObservation::Playback { source_id: 10, .. }))
        );
        assert!(tracker.queue.iter().any(|event| matches!(
            event,
            QueuedObservation::Source {
                source_id: 10,
                changed_fields,
                ..
            } if changed_fields & messages::SOURCE_CHANGED_PLAYBACK != 0
        )));
        output.stop();
    }

    #[test]
    fn source_wait_registry_is_bounded_and_cancel_is_correlated() {
        let (shared, output) = linked_av_shared();
        let (mut client, server) = stream_pair();
        client
            .write_all(&vivid_protocol::wire::encode_preface(ConnectionKind::Control, 4096))
            .unwrap();
        let (reader, _) = Reader::new(server).unwrap();
        let writer = Arc::new(reader.writer().unwrap());
        lock_registry(&shared).sessions.insert(
            1,
            SessionRuntime {
                writer: Arc::downgrade(&writer),
                tag: [0; 16],
                anchor_key: anchor::derive_key(&[0; 32], &[0; 16]),
                seen_anchors: HashSet::new(),
                last_visibility: HashMap::new(),
                accepted_features: HashSet::from([messages::FEATURE_OBSERVABILITY_CORE_V1]),
                authority_root_session: 1,
                bound_context_id: 1,
                context_class_mask: messages::CONTEXT_CLASS_MASK,
                context_quotas: root_context_quotas(),
                active_media_connections: 0,
                revoked: false,
            },
        );
        let pending = Arc::new(PendingOperations::default());
        let mut transactions = HashMap::new();
        let mut waits = HashMap::new();
        let mut observations = ObservationTracker::default();
        for request_id in 1..=MAX_REGISTERED_WAITS as u64 {
            dispatch_control(
                &Record {
                    record_type: messages::WAIT_SOURCE,
                    flags: 0,
                    object_id: 10,
                    sequence: request_id,
                    body: messages::wait_source(
                        request_id,
                        messages::WaitSource {
                            source_id: 10,
                            condition: messages::WAIT_SOURCE_REVISION,
                            value: Some(u64::MAX),
                            timeout_us: 1_000_000,
                        },
                    )
                    .unwrap(),
                },
                1,
                1,
                &shared,
                &writer,
                &pending,
                &mut transactions,
                &mut waits,
                &mut observations,
            )
            .unwrap();
        }
        let overflow = dispatch_control(
            &Record {
                record_type: messages::WAIT_SOURCE,
                flags: 0,
                object_id: 10,
                sequence: 100,
                body: messages::wait_source(
                    100,
                    messages::WaitSource {
                        source_id: 10,
                        condition: messages::WAIT_SOURCE_REVISION,
                        value: Some(u64::MAX),
                        timeout_us: 1_000_000,
                    },
                )
                .unwrap(),
            },
            1,
            1,
            &shared,
            &writer,
            &pending,
            &mut transactions,
            &mut waits,
            &mut observations,
        )
        .unwrap_err();
        assert_eq!(overflow.code, messages::ERROR_LIMIT_EXCEEDED);

        dispatch_control(
            &Record {
                record_type: messages::CANCEL_WAIT,
                flags: 0,
                object_id: 0,
                sequence: 101,
                body: messages::cancel_wait(101, 1).unwrap(),
            },
            1,
            1,
            &shared,
            &writer,
            &pending,
            &mut transactions,
            &mut waits,
            &mut observations,
        )
        .unwrap();
        let mut replies = Vec::new();
        let mut cancelled_code = None;
        for _ in 0..2 {
            let mut header = [0; vivid_protocol::wire::HEADER_SIZE];
            client.read_exact(&mut header).unwrap();
            let header = vivid_protocol::wire::RecordHeader::decode(header);
            let mut body = vec![0; header.body_length as usize];
            client.read_exact(&mut body).unwrap();
            if header.record_type == messages::ERROR {
                cancelled_code = Some(messages::parse_error_reply(&body).unwrap().code);
            }
            replies.push((header.record_type, messages::request_id(&body).unwrap()));
        }
        assert_eq!(replies, [(messages::OK, 101), (messages::ERROR, 1)]);
        assert_eq!(cancelled_code, Some(messages::ERROR_CANCELLED));
        assert!(!waits.contains_key(&1));
        output.stop();
    }

    #[test]
    fn source_waits_timeout_and_cancel_when_the_source_is_destroyed() {
        let (shared, output) = linked_av_shared();
        let (mut client, server) = stream_pair();
        client
            .write_all(&vivid_protocol::wire::encode_preface(ConnectionKind::Control, 4096))
            .unwrap();
        let (reader, _) = Reader::new(server).unwrap();
        let writer = reader.writer().unwrap();
        let mut waits = HashMap::from([
            (
                1,
                RegisteredWait {
                    source_id: 10,
                    condition: messages::WAIT_SOURCE_REVISION,
                    value: Some(u64::MAX),
                    deadline: Instant::now() - Duration::from_millis(1),
                },
            ),
            (
                2,
                RegisteredWait {
                    source_id: 11,
                    condition: messages::WAIT_SOURCE_REVISION,
                    value: Some(u64::MAX),
                    deadline: Instant::now() + Duration::from_secs(1),
                },
            ),
        ]);
        shared.scene.remove_source((1, 11)).unwrap();
        service_source_waits(&shared.scene, 1, &writer, &mut waits, Instant::now()).unwrap();
        assert!(waits.is_empty());

        let mut codes = HashMap::new();
        for _ in 0..2 {
            let mut header = [0; vivid_protocol::wire::HEADER_SIZE];
            client.read_exact(&mut header).unwrap();
            let header = vivid_protocol::wire::RecordHeader::decode(header);
            assert_eq!(header.record_type, messages::ERROR);
            let mut body = vec![0; header.body_length as usize];
            client.read_exact(&mut body).unwrap();
            let error = messages::parse_error_reply(&body).unwrap();
            codes.insert(error.request_id, error.code);
        }
        assert_eq!(codes.get(&1), Some(&messages::ERROR_TIMEOUT));
        assert_eq!(codes.get(&2), Some(&messages::ERROR_CANCELLED));
        output.stop();
    }

    #[test]
    fn play_acknowledges_admission_before_preroll_completes() {
        let (shared, output) = linked_av_shared();
        let (mut client, server) = stream_pair();
        client
            .write_all(&vivid_protocol::wire::encode_preface(ConnectionKind::Control, 4096))
            .unwrap();
        let (reader, _) = Reader::new(server).unwrap();
        let writer = Arc::new(reader.writer().unwrap());
        lock_registry(&shared).sessions.insert(
            1,
            SessionRuntime {
                writer: Arc::downgrade(&writer),
                tag: [0; 16],
                anchor_key: anchor::derive_key(&[0; 32], &[0; 16]),
                seen_anchors: HashSet::new(),
                last_visibility: HashMap::new(),
                accepted_features: HashSet::from([messages::FEATURE_VIDEO_CONTROL_V1]),
                authority_root_session: 1,
                bound_context_id: 1,
                context_class_mask: messages::CONTEXT_CLASS_MASK,
                context_quotas: root_context_quotas(),
                active_media_connections: 0,
                revoked: false,
            },
        );
        let pending = Arc::new(PendingOperations::default());
        let mut transactions = HashMap::new();
        let mut waits = HashMap::new();
        let mut observations = ObservationTracker::default();
        let started = Instant::now();
        dispatch_control(
            &Record {
                record_type: messages::PLAY,
                flags: 0,
                object_id: 10,
                sequence: 1,
                body: messages::play(77, 10, 500_000),
            },
            1,
            1,
            &shared,
            &writer,
            &pending,
            &mut transactions,
            &mut waits,
            &mut observations,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(shared.scene.presentation_due((1, 10), 0), Some(false));

        let mut header = [0; vivid_protocol::wire::HEADER_SIZE];
        client.read_exact(&mut header).unwrap();
        let header = vivid_protocol::wire::RecordHeader::decode(header);
        assert_eq!(header.record_type, messages::OK);
        let mut body = vec![0; header.body_length as usize];
        client.read_exact(&mut body).unwrap();
        assert_eq!(messages::request_id(&body).unwrap(), 77);
        output.stop();
    }

    fn linked_av_shared() -> (Arc<ServiceShared>, Arc<AudioOutput>) {
        let scene = SharedScene::default();
        scene
            .add_source(
                1,
                10,
                SourceConfig::Video(messages::ParsedVideoSourceConfig {
                    source_id: 10,
                    codec: "h264".into(),
                    packetization: "h264-annexb-au-v1".into(),
                    extradata: Vec::new(),
                    width: 1,
                    height: 1,
                    profile: 0,
                    level: 0,
                    bitrate: 0,
                    color_primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    range: 1,
                    sar_num: 1,
                    sar_den: 1,
                    max_access_unit_bytes: 1024,
                    codec_string: None,
                    decoder_config: None,
                }),
            )
            .unwrap();
        scene
            .add_source(
                1,
                11,
                SourceConfig::Audio(messages::ParsedAudioSourceConfig {
                    source_id: 11,
                    linked_video_source_id: Some(10),
                    codec: "pcm_s16le".into(),
                    packetization: "pcm-packet-v1".into(),
                    extradata: Vec::new(),
                    sample_rate: 48_000,
                    channels: 2,
                    channel_mask: 3,
                    bitrate: 1_536_000,
                    max_access_unit_bytes: 4096,
                    codec_string: None,
                }),
            )
            .unwrap();
        scene.start_playback((1, 10), messages::PlayRequest::baseline(10, 0)).unwrap();
        let output = AudioOutput::test_output();
        output.configure_play(0, 100_000);
        output.start();
        let shared = Arc::new(ServiceShared {
            token: [0; 32],
            scene,
            registry: Mutex::new(Registry::default()),
            metrics: Mutex::new(DisplayMetrics {
                viewport_width: 1,
                viewport_height: 1,
                columns: 1,
                rows: 1,
                cell_width: 1,
                cell_height: 1,
                generation: 1,
            }),
            pending_display_change: Mutex::new(None),
            capability_generation: AtomicU64::new(1),
            audio_device_available: AtomicBool::new(true),
            active_connections: AtomicUsize::new(0),
            audio_outputs: Mutex::new(HashMap::from([((1, 11), output.clone())])),
            render_state: Mutex::new((true, 0)),
            wake: Arc::new(|| {}),
            trace: None,
            _trace_guard: None,
        });
        (shared, output)
    }

    fn one_pending_frame(scene: &SharedScene) -> VecDeque<QueuedVideoFrame> {
        assert!(scene.reserve_queued_pixels(1));
        VecDeque::from([QueuedVideoFrame {
            epoch: 0,
            frame: Some(Frame {
                frame_id: 1,
                pts_us: 0,
                width: 1,
                height: 1,
                rgba: Arc::from([0, 0, 0, 255]),
                alpha_mode: messages::ALPHA_STRAIGHT,
                sar_num: 1,
                sar_den: 1,
            }),
            pixels: 1,
            scene: scene.clone(),
        }])
    }

    #[test]
    fn linked_video_falls_back_after_empty_audio_stall_and_rejoins_audio_clock() {
        let (shared, output) = linked_av_shared();
        let mut pending = one_pending_frame(&shared.scene);
        assert!(present_ready_video_frames(&shared, (1, 10), &mut pending).unwrap());
        assert_eq!(pending.len(), 1);

        output.force_video_gate_stall_for_test();
        assert!(present_ready_video_frames(&shared, (1, 10), &mut pending).unwrap());
        assert!(pending.is_empty());

        let mut pending = one_pending_frame(&shared.scene);
        output.push(&[0.0, 0.0]).unwrap();
        assert!(present_ready_video_frames(&shared, (1, 10), &mut pending).unwrap());
        assert_eq!(pending.len(), 1);

        shared.scene.remove_source((1, 11)).unwrap();
        shared
            .audio_outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(1, 11));
        assert!(present_ready_video_frames(&shared, (1, 10), &mut pending).unwrap());
        assert!(pending.is_empty());
    }

    #[test]
    fn capability_tokens_are_hex_and_compared_without_early_exit() {
        let token = [0xab; 32];
        let text = hex(&token);
        assert_eq!(text.len(), 64);
        assert!(constant_time_token_eq(&token, text.as_bytes()));
        assert!(!constant_time_token_eq(&token, b"abcd"));
    }

    #[test]
    fn authenticated_anchor_marker_parses_strictly() {
        let key = anchor::derive_key(&[0; 32], &[0; 16]);
        let encoded = anchor::encode_marker(&key, &[0; 16], 7).unwrap();
        let marker = anchor::parse_marker(&encoded[2..encoded.len() - 2]).unwrap();
        assert!(anchor::verify_marker(&key, &marker));
        assert!(anchor::parse_marker("VIVID;1;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000007").is_err());
    }

    #[test]
    fn vivid_version_selection_accepts_only_ranges_containing_1_1() {
        assert!(!offers_vivid_version(0, 9, 0, 9));
        assert!(!offers_vivid_version(1, 0, 1, 0));
        assert!(offers_vivid_version(1, 1, 1, 1));
        assert!(offers_vivid_version(1, 0, 1, 1));
        assert!(offers_vivid_version(0, 9, 2, 0));
        assert!(!offers_vivid_version(1, 2, 2, 0));
    }

    #[test]
    fn charged_body_returns_its_credit_once() {
        use std::io::{Read, Write};
        use vivid_protocol::wire::{HEADER_SIZE, RecordHeader, encode_preface};

        let (mut client, server) = stream_pair();
        client.write_all(&encode_preface(ConnectionKind::Raster, 1024)).unwrap();
        let (reader, _) = Reader::new(server).unwrap();
        let writer = reader.writer().unwrap();
        drop(ChargedBody::new(&writer, 7, 99));

        let mut header = [0; HEADER_SIZE];
        client.read_exact(&mut header).unwrap();
        let header = RecordHeader::decode(header);
        assert_eq!(
            (header.record_type, header.object_id, header.sequence),
            (messages::CREDIT, 7, 1)
        );
        let mut body = vec![0; header.body_length as usize];
        client.read_exact(&mut body).unwrap();
        let credits = messages::parse_credit(&body).unwrap();
        assert_eq!((credits.bytes, credits.packets), (99, 1));

        client.set_nonblocking(true).unwrap();
        let mut extra = [0];
        assert_eq!(client.read(&mut extra).unwrap_err().kind(), ErrorKind::WouldBlock);
    }

    #[test]
    fn encoded_image_container_rejects_animation_and_multipicture() {
        let mut apng = b"\x89PNG\r\n\x1a\n".to_vec();
        apng.extend_from_slice(&[0, 0, 0, 0]);
        apng.extend_from_slice(b"acTL");
        apng.extend_from_slice(&[0; 4]);
        assert!(encoded_image_has_multiple_pictures(messages::IMAGE_PNG, &apng).unwrap());

        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0, 0, 0, 0]);
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&[0; 4]);
        assert!(!encoded_image_has_multiple_pictures(messages::IMAGE_PNG, &png).unwrap());

        assert!(
            encoded_image_has_multiple_pictures(
                messages::IMAGE_JPEG,
                &[0xff, 0xd8, 0xff, 0xe2, 0, 6, b'M', b'P', b'F', 0],
            )
            .unwrap()
        );
        assert!(
            !encoded_image_has_multiple_pictures(messages::IMAGE_JPEG, &[0xff, 0xd8, 0xff, 0xd9],)
                .unwrap()
        );
    }

    #[test]
    fn live_socket_authenticates_commits_and_delivers_raster_without_pty_bytes() {
        let service = VividService::start_with_wake(
            DisplayMetrics {
                viewport_width: 800,
                viewport_height: 600,
                columns: 80,
                rows: 30,
                cell_width: 10,
                cell_height: 20,
                generation: 1,
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let endpoint = Endpoint::parse(service.endpoint()).unwrap();

        let mut unsupported = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        let mut hello = vivid_protocol::cbor::Encoder::new();
        hello.map(2);
        hello.u64(0);
        hello.u64(1);
        hello.u64(3);
        hello.map(10);
        hello.u64(0);
        hello.u64(1);
        hello.u64(1);
        hello.u64(0);
        hello.u64(2);
        hello.u64(1);
        hello.u64(3);
        hello.u64(0);
        hello.u64(4);
        hello.text(service.token());
        hello.u64(5);
        hello.text("unsupported-version-test");
        hello.u64(6);
        hello.text("unsupported");
        hello.u64(7);
        hello.array(0);
        hello.u64(8);
        hello.array(0);
        hello.u64(9);
        hello.u64(u64::from(vivid_protocol::CONTROL_MAX_RECORD_BODY));
        unsupported.write_record(messages::HELLO, 0, 0, &hello.into_vec()).unwrap();
        let rejection =
            messages::parse_error_reply(&unsupported.read_record().unwrap().body).unwrap();
        assert_eq!(rejection.code, messages::ERROR_UNSUPPORTED_VERSION);
        assert_eq!(rejection.supported_version, Some((1, 1)));

        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control.write_record(messages::HELLO, 0, 0, &messages::hello(1, service.token())).unwrap();
        let welcome = parse_welcome(&control.read_record().unwrap().body).unwrap();
        assert_eq!(welcome.initial_scene_revision, vivid_protocol::revision::SceneRevision::ZERO);
        let token: [u8; 32] = *anchor::decode_token(service.token()).unwrap();
        let tag: [u8; 16] = welcome.session_tag.as_slice().try_into().unwrap();
        let key = anchor::derive_key(&token, &tag);
        let marker = anchor::encode_marker(&key, &tag, 77).unwrap();
        let marker = &marker[2..marker.len() - 2];
        service.handle_terminal_marker(marker, 1, 2, false);
        service.handle_terminal_marker(marker, 9, 9, false);
        assert_eq!(
            lock_registry(&service.shared)
                .sessions
                .get(&welcome.session_id)
                .unwrap()
                .seen_anchors
                .len(),
            1
        );
        let anchor_ready = control.read_record().unwrap();
        assert_eq!(anchor_ready.record_type, messages::ANCHOR_READY);
        assert_eq!(messages::parse_anchor_event(&anchor_ready.body).unwrap(), 77);
        service.update_metrics(DisplayMetrics {
            viewport_width: 1000,
            viewport_height: 700,
            columns: 100,
            rows: 35,
            cell_width: 10,
            cell_height: 20,
            generation: 0,
        });
        service.flush_display_change(None);
        let changed_record = control.read_record().unwrap();
        assert_eq!(changed_record.record_type, messages::DISPLAY_CHANGED);
        let changed = parse_display_changed(&changed_record.body).unwrap();
        assert_eq!(changed.display_generation, 2);
        assert_eq!((changed.grid_columns, changed.grid_rows), (100, 35));
        assert!(!changed.settled);
        service.flush_display_change(Some(changed.display_generation));
        let settled_record = control.read_record().unwrap();
        assert_eq!(settled_record.record_type, messages::DISPLAY_CHANGED);
        let settled = parse_display_changed(&settled_record.body).unwrap();
        assert_eq!(settled.display_generation, changed.display_generation);
        assert!(settled.settled);

        control
            .write_record(messages::CREATE_RASTER, 0, 1, &messages::create_raster(2, 1, 2, 1))
            .unwrap();
        let ready = parse_source_ready(&control.read_record().unwrap().body).unwrap();
        assert!(ready.byte_credits >= u64::from(ready.max_media_body));
        assert!(ready.packet_credits >= 1);

        control
            .write_record(messages::BEGIN_TXN, 0, 0, &messages::begin_transaction(3, 3))
            .unwrap();
        control
            .write_record(
                messages::CREATE_NODE,
                0,
                2,
                &messages::create_node(
                    4,
                    3,
                    NodeConfig {
                        node_id: 2,
                        source_id: 1,
                        context_id: welcome.root_context_id,
                        columns: 2,
                        rows: 1,
                        anchor_id: None,
                    },
                ),
            )
            .unwrap();
        control
            .write_record(messages::COMMIT_TXN, 0, 0, &messages::commit_transaction(5, 3, 2))
            .unwrap();
        let presented = loop {
            let record = control.read_record().unwrap();
            if messages::request_id(&record.body).unwrap() == 5 {
                break record;
            }
        };
        assert_eq!(presented.record_type, messages::PRESENTED);
        assert_eq!(
            messages::parse_presented(&presented.body).unwrap(),
            (5, vivid_protocol::revision::SceneRevision::new(1))
        );

        let mut media_channel = Connection::open(&endpoint, ConnectionKind::Raster).unwrap();
        media_channel
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                1,
                &messages::attach_channel(&ready.media_ticket),
            )
            .unwrap();
        media_channel
            .write_record(
                messages::RASTER_FRAME,
                0,
                1,
                &media::raster_frame_body(1, 1, 2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]).unwrap(),
            )
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            let items = service.scene().snapshot().1;
            if let Some(item) = items.first() {
                assert_eq!(item.frame.rgba.as_ref(), &[255, 0, 0, 255, 0, 255, 0, 255]);
                break;
            }
            assert!(std::time::Instant::now() < deadline, "raster frame was not delivered");
            thread::sleep(Duration::from_millis(5));
        }

        control.write_record(messages::GOODBYE, 0, 0, &messages::goodbye(6)).unwrap();
    }

    #[test]
    fn delegated_context_auth_is_confined_and_revocation_is_actionable() {
        let service = VividService::start_with_wake(
            DisplayMetrics {
                viewport_width: 800,
                viewport_height: 600,
                columns: 80,
                rows: 30,
                cell_width: 10,
                cell_height: 20,
                generation: 1,
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let root_token = service.token().as_bytes().to_vec();
        let root_token_binary = anchor::decode_token(service.token()).unwrap().to_vec();
        let endpoint = Endpoint::parse(service.endpoint()).unwrap();
        let mut root = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        root.write_record(
            messages::HELLO,
            0,
            0,
            &context_hello(1, service.token(), messages::AUTHENTICATION_WINDOW_ROOT),
        )
        .unwrap();
        let welcome = parse_welcome(&root.read_record().unwrap().body).unwrap();
        let child_id = welcome.root_context_id + 1;
        let create = messages::create_context(
            2,
            &messages::CreateContextRequest {
                context_id: child_id,
                parent_context_id: welcome.root_context_id,
                class_mask: messages::CONTEXT_CLASS_CREATE_SOURCE
                    | messages::CONTEXT_CLASS_OBSERVE
                    | messages::CONTEXT_CLASS_CREATE_ANCHOR,
                label: "isolated raster worker".into(),
                expiry_us: 5_000_000,
                quotas: messages::ContextQuotas {
                    maximum_sources: 1,
                    maximum_nodes: 1,
                    maximum_retained_pixels: 4,
                    maximum_media_bytes: 4096,
                    maximum_media_connections: 1,
                },
            },
        )
        .unwrap();
        root.write_record(messages::CREATE_CONTEXT, 0, child_id, &create).unwrap();
        let ready = read_correlated(&mut root, messages::CONTEXT_READY, 2);
        assert_eq!(
            messages::parse_context_ready(&ready.body).unwrap().1.class_mask,
            messages::CONTEXT_CLASS_CREATE_SOURCE
                | messages::CONTEXT_CLASS_OBSERVE
                | messages::CONTEXT_CLASS_CREATE_ANCHOR
        );

        root.write_record(
            messages::DELEGATE_CONTEXT,
            0,
            child_id,
            &messages::delegate_context(3, child_id),
        )
        .unwrap();
        let capability_record = read_correlated(&mut root, messages::CONTEXT_CAPABILITY, 3);
        let (_, _, capability) =
            messages::parse_context_capability(&capability_record.body).unwrap();
        let verifier: [u8; 32] = Sha256::digest(capability).into();
        assert!(
            lock_registry(&service.shared)
                .capabilities
                .iter()
                .any(|binding| binding.verifier == verifier)
        );
        assert_ne!(capability, verifier);
        root.write_record(
            messages::QUERY_SCENE,
            0,
            0,
            &messages::query_scene(
                30,
                &messages::SceneQuery {
                    expected_revision: None,
                    cursor: None,
                    maximum_nodes: Some(16),
                },
            )
            .unwrap(),
        )
        .unwrap();
        let status = read_correlated(&mut root, messages::SCENE_STATUS, 30);
        assert!(!status.body.windows(capability.len()).any(|window| window == capability));

        let mut delegated = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        delegated
            .write_record(
                messages::HELLO,
                0,
                0,
                &context_hello(1, &hex(&capability), messages::AUTHENTICATION_DELEGATED_CONTEXT),
            )
            .unwrap();
        let delegated_welcome = parse_welcome(&delegated.read_record().unwrap().body).unwrap();
        assert_eq!(delegated_welcome.root_context_id, child_id);
        assert!(
            !delegated_welcome.accepted_features.contains(&messages::FEATURE_IMAGE_CACHE_V1),
            "the Stage 3 presenter must not expose a cache-hit oracle before context-local caching"
        );

        root.write_record(messages::CREATE_RASTER, 0, 999, &messages::create_raster(31, 999, 1, 1))
            .unwrap();
        let root_ready = messages::parse_source_ready(
            &read_correlated(&mut root, messages::SOURCE_READY, 31).body,
        )
        .unwrap();
        delegated
            .write_record(messages::QUERY_SOURCE, 0, 999, &messages::query_source(2, 999).unwrap())
            .unwrap();
        let denied = read_correlated(&mut delegated, messages::ERROR, 2);
        assert_eq!(
            messages::parse_error_reply(&denied.body).unwrap().code,
            messages::ERROR_NOT_FOUND
        );
        assert_record_excludes(
            &denied,
            &[
                ("root token", &root_token),
                ("raw root token", &root_token_binary),
                ("delegated capability", &capability),
                ("foreign ticket", &root_ready.media_ticket),
            ],
        );
        delegated
            .write_record(messages::CREATE_RASTER, 0, 7, &messages::create_raster(3, 7, 1, 1))
            .unwrap();
        let source_reply = delegated.read_record().unwrap();
        assert_eq!(
            source_reply.record_type,
            messages::SOURCE_READY,
            "{:?}",
            (source_reply.record_type == messages::ERROR)
                .then(|| messages::parse_error_reply(&source_reply.body).unwrap())
        );
        let delegated_ready = messages::parse_source_ready(&source_reply.body).unwrap();
        assert!(
            lock_registry(&service.shared).tickets.contains_key(&delegated_ready.media_ticket),
            "the revocation fixture needs an outstanding single-use ticket"
        );

        delegated.write_record(messages::QUERY_LIMITS, 0, 0, &messages::query_limits(5)).unwrap();
        let limits_record = read_correlated(&mut delegated, messages::LIMITS_STATUS, 5);
        let (_, limits) = messages::parse_limits_status(&limits_record.body).unwrap();
        assert_eq!(
            limits.current_sources, 1,
            "a delegated session must not enumerate the root context's source"
        );
        assert_eq!(
            limits.image_cache_budget, None,
            "no cache-probe surface is advertised before IMAGE_CACHE_V1"
        );
        assert_record_excludes(
            &limits_record,
            &[
                ("root token", &root_token),
                ("raw root token", &root_token_binary),
                ("delegated capability", &capability),
                ("foreign ticket", &root_ready.media_ticket),
                ("owned ticket", &delegated_ready.media_ticket),
            ],
        );

        let delegated_tag: [u8; 16] = delegated_welcome.session_tag.as_slice().try_into().unwrap();
        let delegated_key =
            anchor::derive_key(capability.as_slice().try_into().unwrap(), &delegated_tag);
        let marker = anchor::encode_marker(&delegated_key, &delegated_tag, 77).unwrap();
        service.handle_terminal_marker(&marker[2..marker.len() - 2], 1, 2, false);
        loop {
            let event = delegated.read_record().unwrap();
            assert_record_excludes(
                &event,
                &[
                    ("root token", &root_token),
                    ("raw root token", &root_token_binary),
                    ("delegated capability", &capability),
                    ("foreign ticket", &root_ready.media_ticket),
                    ("owned ticket", &delegated_ready.media_ticket),
                    ("anchor marker", marker.as_bytes()),
                ],
            );
            if event.record_type == messages::ANCHOR_READY {
                break;
            }
        }

        root.write_record(
            messages::REVOKE_CONTEXT,
            0,
            child_id,
            &messages::revoke_context(4, child_id),
        )
        .unwrap();
        loop {
            let record = root.read_record().unwrap();
            assert_record_excludes(
                &record,
                &[
                    ("root token", &root_token),
                    ("raw root token", &root_token_binary),
                    ("delegated capability", &capability),
                    ("foreign ticket", &root_ready.media_ticket),
                    ("owned ticket", &delegated_ready.media_ticket),
                    ("anchor marker", marker.as_bytes()),
                ],
            );
            if record.record_type == messages::OK
                && messages::request_id(&record.body).unwrap() == 4
            {
                break;
            }
        }
        let reset = delegated.read_record().unwrap();
        assert_eq!(reset.record_type, messages::INPUT_RESET);
        assert_record_excludes(
            &reset,
            &[
                ("root token", &root_token),
                ("raw root token", &root_token_binary),
                ("delegated capability", &capability),
                ("foreign ticket", &root_ready.media_ticket),
                ("owned ticket", &delegated_ready.media_ticket),
                ("anchor marker", marker.as_bytes()),
            ],
        );
        let revoked = delegated.read_record().unwrap();
        assert_eq!(revoked.record_type, messages::ERROR);
        assert_record_excludes(
            &revoked,
            &[
                ("root token", &root_token),
                ("raw root token", &root_token_binary),
                ("delegated capability", &capability),
                ("foreign ticket", &root_ready.media_ticket),
                ("owned ticket", &delegated_ready.media_ticket),
                ("anchor marker", marker.as_bytes()),
            ],
        );
        let error = messages::parse_error_reply(&revoked.body).unwrap();
        assert_eq!(error.code, messages::ERROR_CONTEXT_REVOKED);
        assert!(error.fatal);
        assert!(
            lock_registry(&service.shared)
                .capabilities
                .iter()
                .all(|binding| binding.verifier != verifier)
        );
        assert!(
            service.shared.scene.source_observation((delegated_welcome.session_id, 7)).is_none()
        );
        assert!(
            !lock_registry(&service.shared).tickets.contains_key(&delegated_ready.media_ticket),
            "context revocation must synchronously destroy unused tickets"
        );
        assert!(service.shared.scene.scene_revision(welcome.session_id).get() > 0);
    }

    #[test]
    fn display_changes_coalesce_per_frame_and_end_settled() {
        let initial = DisplayMetrics {
            viewport_width: 800,
            viewport_height: 600,
            columns: 80,
            rows: 24,
            cell_width: 10,
            cell_height: 25,
            generation: 1,
        };
        let mut pending =
            PendingDisplayChange { metrics: initial, last_unsettled_generation: None };
        let first = pending.event(false).unwrap();
        assert!(!first.settled);
        assert!(pending.event(false).is_none(), "one compositor frame emits at most once");

        pending.metrics.generation = 3;
        let coalesced = pending.event(false).unwrap();
        assert_eq!(coalesced.display_generation, 3);
        assert!(!coalesced.settled);
        let final_event = pending.event(true).unwrap();
        assert_eq!(final_event.display_generation, 3);
        assert!(final_event.settled);
    }

    #[test]
    fn live_observability_queries_events_and_waits_share_authoritative_state() {
        let service = VividService::start_with_wake(
            DisplayMetrics {
                viewport_width: 800,
                viewport_height: 600,
                columns: 80,
                rows: 30,
                cell_width: 10,
                cell_height: 20,
                generation: 1,
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let endpoint = Endpoint::parse(service.endpoint()).unwrap();
        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control.write_record(messages::HELLO, 0, 0, &messages::hello(1, service.token())).unwrap();
        let welcome = parse_welcome(&control.read_record().unwrap().body).unwrap();
        assert!(welcome.accepted_features.contains(&messages::FEATURE_OBSERVABILITY_CORE_V1));

        control
            .write_record(
                messages::SET_OBSERVATION,
                0,
                0,
                &messages::set_observation(2, messages::OBSERVATION_CLASS_MASK).unwrap(),
            )
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);

        control
            .write_record(messages::CREATE_RASTER, 0, 1, &messages::create_raster(3, 1, 2, 1))
            .unwrap();
        let first = control.read_record().unwrap();
        let second = control.read_record().unwrap();
        let (ready_record, changed_record) = if first.record_type == messages::SOURCE_READY {
            (first, second)
        } else {
            (second, first)
        };
        let ready = parse_source_ready(&ready_record.body).unwrap();
        assert_eq!(changed_record.record_type, messages::SOURCE_CHANGED);
        let created = messages::parse_source_changed(&changed_record.body).unwrap();
        assert_eq!(created.source_id, 1);
        assert_ne!(created.changed_fields & messages::SOURCE_CHANGED_LIFECYCLE, 0);

        control
            .write_record(messages::QUERY_SOURCE, 0, 1, &messages::query_source(4, 1).unwrap())
            .unwrap();
        let source_status_record = read_correlated(&mut control, messages::SOURCE_STATUS, 4);
        let (_, source_status) = messages::parse_source_status(&source_status_record.body).unwrap();
        assert_eq!(source_status.source_revision, SourceRevision::new(1));
        assert_eq!(source_status.attachment_state, messages::ATTACHMENT_NEVER);

        control
            .write_record(
                messages::WAIT_SOURCE,
                0,
                1,
                &messages::wait_source(
                    5,
                    messages::WaitSource {
                        source_id: 1,
                        condition: messages::WAIT_MEDIA_ATTACHED,
                        value: None,
                        timeout_us: 1_000_000,
                    },
                )
                .unwrap(),
            )
            .unwrap();
        let mut media_channel = Connection::open(&endpoint, ConnectionKind::Raster).unwrap();
        media_channel
            .write_record(
                messages::ATTACH_CHANNEL,
                0,
                1,
                &messages::attach_channel(&ready.media_ticket),
            )
            .unwrap();
        media_channel
            .write_record(
                messages::RASTER_FRAME,
                0,
                1,
                &media::raster_frame_body(1, 1, 2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]).unwrap(),
            )
            .unwrap();

        let mut attached_wait = None;
        let mut attached_change = None;
        while attached_wait.is_none() || attached_change.is_none() {
            let record = control.read_record().unwrap();
            match record.record_type {
                messages::WAIT_SATISFIED => {
                    attached_wait = Some(messages::parse_wait_satisfied(&record.body).unwrap().1);
                },
                messages::SOURCE_CHANGED => {
                    attached_change = Some(messages::parse_source_changed(&record.body).unwrap());
                },
                _ => {},
            }
        }
        assert_eq!(attached_wait.unwrap().condition, messages::WAIT_MEDIA_ATTACHED);
        assert_ne!(
            attached_change.unwrap().changed_fields & messages::SOURCE_CHANGED_ATTACHMENT,
            0
        );

        control.write_record(messages::QUERY_LIMITS, 0, 0, &messages::query_limits(6)).unwrap();
        let limits = read_correlated(&mut control, messages::LIMITS_STATUS, 6);
        let (_, limits) = messages::parse_limits_status(&limits.body).unwrap();
        assert!(limits.maximum_waits >= 32);
        assert_eq!(limits.current_sources, 1);

        control
            .write_record(messages::BEGIN_TXN, 0, 0, &messages::begin_transaction(7, 1))
            .unwrap();
        read_correlated(&mut control, messages::OK, 7);
        control
            .write_record(
                messages::CREATE_NODE,
                0,
                1,
                &messages::create_node(
                    8,
                    1,
                    NodeConfig {
                        node_id: 1,
                        source_id: 1,
                        context_id: welcome.root_context_id,
                        columns: 2,
                        rows: 1,
                        anchor_id: None,
                    },
                ),
            )
            .unwrap();
        read_correlated(&mut control, messages::OK, 8);
        control
            .write_record(messages::COMMIT_TXN, 0, 0, &messages::commit_transaction(9, 1, 1))
            .unwrap();
        let mut presented = None;
        let mut scene_changed = None;
        while presented.is_none() || scene_changed.is_none() {
            let record = control.read_record().unwrap();
            match record.record_type {
                messages::PRESENTED => {
                    presented = Some(messages::parse_presented(&record.body).unwrap());
                },
                messages::SCENE_CHANGED => {
                    scene_changed = Some(messages::parse_scene_changed(&record.body).unwrap());
                },
                _ => {},
            }
        }
        assert_eq!(presented.unwrap().1, SceneRevision::new(1));
        assert_ne!(scene_changed.unwrap().reason_mask & messages::SCENE_CHANGED_PRODUCER_COMMIT, 0);

        control.write_record(messages::GOODBYE, 0, 0, &messages::goodbye(10)).unwrap();
    }

    #[test]
    fn atomic_preconditions_idempotency_and_causation_use_authoritative_revisions() {
        let service = VividService::start_with_wake(
            DisplayMetrics {
                viewport_width: 800,
                viewport_height: 600,
                columns: 80,
                rows: 30,
                cell_width: 10,
                cell_height: 20,
                generation: 1,
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let endpoint = Endpoint::parse(service.endpoint()).unwrap();
        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control.write_record(messages::HELLO, 0, 0, &messages::hello(1, service.token())).unwrap();
        let welcome = parse_welcome(&control.read_record().unwrap().body).unwrap();
        assert!(welcome.accepted_features.contains(&messages::FEATURE_ATOMIC_CONTROL_V1));
        control
            .write_record(
                messages::SET_OBSERVATION,
                0,
                0,
                &messages::set_observation(2, messages::OBSERVE_SOURCE_TRANSITIONS).unwrap(),
            )
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);

        let idempotency_key = [0x17; messages::IDEMPOTENCY_KEY_BYTES];
        let causation_id = [0x29; messages::CAUSATION_ID_BYTES];
        let create_metadata = messages::RequestMetadata {
            preconditions: Default::default(),
            idempotency_key: Some(idempotency_key),
            causation_id: Some(causation_id),
        };
        let create =
            messages::with_request_metadata(&messages::create_raster(3, 7, 2, 1), &create_metadata)
                .unwrap();
        control.write_record(messages::CREATE_RASTER, 0, 7, &create).unwrap();
        let mut ready = None;
        let mut saw_causation = false;
        while ready.is_none() || !saw_causation {
            let record = control.read_record().unwrap();
            if record.record_type == messages::SOURCE_READY {
                ready = Some(messages::parse_source_ready(&record.body).unwrap());
            } else if record.record_type == messages::SOURCE_CHANGED {
                saw_causation = messages::decode_control(&record.body).unwrap().causation_id
                    == Some(causation_id);
            }
        }
        let ready = ready.unwrap();

        let retry =
            messages::with_request_metadata(&messages::create_raster(4, 7, 2, 1), &create_metadata)
                .unwrap();
        control.write_record(messages::CREATE_RASTER, 0, 7, &retry).unwrap();
        let replay_record = read_correlated(&mut control, messages::ERROR, 4);
        assert!(
            !replay_record
                .body
                .windows(ready.media_ticket.len())
                .any(|window| window == ready.media_ticket),
            "an idempotent source-creation retry replayed the media ticket"
        );
        let replay = messages::parse_error_reply(&replay_record.body).unwrap();
        assert_eq!(replay.code, messages::ERROR_ALREADY_APPLIED);
        control.write_record(messages::QUERY_LIMITS, 0, 0, &messages::query_limits(40)).unwrap();
        let (_, limits) = messages::parse_limits_status(
            &read_correlated(&mut control, messages::LIMITS_STATUS, 40).body,
        )
        .unwrap();
        assert_eq!(limits.current_sources, 1, "a retried create duplicated the source");

        let stale = messages::with_request_metadata(
            &messages::destroy_source(5, 7),
            &messages::RequestMetadata {
                preconditions: std::collections::BTreeMap::from([(
                    messages::PRECONDITION_SOURCE_REVISION,
                    999,
                )]),
                idempotency_key: None,
                causation_id: None,
            },
        )
        .unwrap();
        control.write_record(messages::DESTROY_SOURCE, 0, 7, &stale).unwrap();
        let failure =
            messages::parse_error_reply(&read_correlated(&mut control, messages::ERROR, 5).body)
                .unwrap();
        assert_eq!(failure.code, messages::ERROR_PRECONDITION_FAILED);

        control
            .write_record(messages::QUERY_SOURCE, 0, 7, &messages::query_source(6, 7).unwrap())
            .unwrap();
        let (_, status) = messages::parse_source_status(
            &read_correlated(&mut control, messages::SOURCE_STATUS, 6).body,
        )
        .unwrap();
        assert_eq!(status.source_id, 7, "stale destroy mutated the source");
        control.write_record(messages::GOODBYE, 0, 0, &messages::goodbye(7)).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn conpty_anchor_arrival_recomputes_visibility_after_early_node_commit() {
        let service = VividService::start_with_wake(
            DisplayMetrics {
                viewport_width: 800,
                viewport_height: 600,
                columns: 80,
                rows: 30,
                cell_width: 10,
                cell_height: 20,
                generation: 1,
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let endpoint = Endpoint::parse(service.endpoint()).unwrap();
        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control.write_record(messages::HELLO, 0, 0, &messages::hello(1, service.token())).unwrap();
        let welcome = parse_welcome(&control.read_record().unwrap().body).unwrap();

        let token: [u8; 32] = *anchor::decode_token(service.token()).unwrap();
        let tag: [u8; 16] = welcome.session_tag.as_slice().try_into().unwrap();
        let key = anchor::derive_key(&token, &tag);
        let marker = anchor::encode_marker(&key, &tag, 77).unwrap();
        let marker = &marker[2..marker.len() - 2];

        control
            .write_record(messages::CREATE_RASTER, 0, 1, &messages::create_raster(2, 1, 2, 1))
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::SOURCE_READY);
        control
            .write_record(messages::BEGIN_TXN, 0, 0, &messages::begin_transaction(3, 3))
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        control
            .write_record(
                messages::CREATE_NODE,
                0,
                2,
                &messages::create_node(
                    4,
                    3,
                    NodeConfig {
                        node_id: 2,
                        source_id: 1,
                        context_id: welcome.root_context_id,
                        columns: 2,
                        rows: 1,
                        anchor_id: Some(77),
                    },
                ),
            )
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::OK);
        control
            .write_record(
                messages::COMMIT_TXN,
                0,
                0,
                &messages::commit_transaction(5, 3, welcome.display_generation),
            )
            .unwrap();
        assert_eq!(control.read_record().unwrap().record_type, messages::PRESENTED);

        let hidden = control.read_record().unwrap();
        assert_eq!((hidden.record_type, hidden.object_id), (messages::VISIBILITY, 1));
        assert!(!messages::parse_visibility(&hidden.body).unwrap().visible);

        // ConPTY can deliver the control-channel commit before the earlier alternate-screen swap,
        // full-screen clear, and marker reach the UI. The clear must preserve the hidden pending
        // node, and accepting its marker must make the source visible without an unrelated event.
        service.handle_screen_swap(true);
        service.update_visibility(true, 0);
        service.handle_terminal_clear();
        service.handle_terminal_marker(marker, 1, 2, true);
        assert_eq!(control.read_record().unwrap().record_type, messages::ANCHOR_READY);
        let visible = control.read_record().unwrap();
        assert_eq!((visible.record_type, visible.object_id), (messages::VISIBILITY, 1));
        assert!(messages::parse_visibility(&visible.body).unwrap().visible);

        control.write_record(messages::GOODBYE, 0, 0, &messages::goodbye(6)).unwrap();
    }
}
