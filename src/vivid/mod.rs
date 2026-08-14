//! Per-window Vivid Protocol 1.5 presenter.

mod actor;
mod audio;
mod clock;
mod decoder;
mod ffmpeg;
pub(crate) mod file_drop;
pub(crate) mod hid;
mod lane;
mod lease;
pub mod scene;
pub mod target;
pub(crate) mod trace;
mod transport;

use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::fs;
use std::io::{self, ErrorKind};
#[cfg(windows)]
use std::net::{Ipv4Addr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use vivid_protocol::anchor::{self, AnchorKey};
use vivid_protocol::auth::{self, Secret32};
use vivid_protocol::cbor::Value;
use vivid_protocol::context::{
    ContextDefinition, ContextState, OP_DELEGATE, OP_DESKTOP_INPUT, OP_RECEIVE_FILE_DROP,
    OP_SURFACE_TRACK_MEDIA, OP_TERMINAL_ANCHOR,
};
use vivid_protocol::grant::{self, Eligibility, InputGrant, reason as grant_reason};
use vivid_protocol::identity::{
    AnchorIdentity, ContextIdentity, PresenterInstanceId, SessionIdentity, SurfaceIdentity,
    TrackIdentity,
};
use vivid_protocol::input::{InputBinding, InputEvent};
use vivid_protocol::media;
use vivid_protocol::messages::{
    self, ChannelOpen, Envelope, ErrorDetail, ErrorReply, Hello, HelloAuthentication, LaneClass,
    StrictMap, Welcome, WelcomeAuthentication,
};
use vivid_protocol::observation::{self, ObservationKey, ObservationQueue};
use vivid_protocol::registry;
use vivid_protocol::resource::{Resource, ResourceContract, TokenBucket};
use vivid_protocol::revision::{
    ChannelGeneration, SceneRevision, SurfaceGeneration, SurfaceRevision, TargetGeneration,
};
use vivid_protocol::scene::SceneNode;
use vivid_protocol::surface::{DesktopSurfaceParameters, SurfaceDefinition, SurfaceDescriptor};
use vivid_protocol::track::{
    ChannelOpenDecision, ChannelOpenState, KindConfiguration, TrackConfiguration, TrackMode,
};
use vivid_protocol::wire::{ConnectionKind, RECORD_OPTIONAL, Record};

use crate::event::{EventProxy, EventType};
use crate::terminal::event::EventListener;
use crate::terminal::grid::Dimensions;
use crate::terminal::index::{Column, Line, Point};
use crate::terminal::term::{ResizePoint, Term};
use crate::vivid::actor::{AdmissionError, Egress, Pending, PendingSet};
use crate::vivid::audio::{AudioOutput, supports as supports_audio};
use crate::vivid::decoder::Decoder;
use crate::vivid::lease::{Lease, LeaseKey, LeaseTable, profile_fingerprint, reason};
use crate::vivid::scene::{
    CommitRejection, Frame, RgbaBuffer, SceneStatus, SharedScene, SurfaceStatus, TrackStatus,
    TrackWaitEvaluation, TrackWaitSatisfied,
};
pub use crate::vivid::target::DisplayGeometry;
use crate::vivid::target::{DesktopTarget, PresentationTarget, TerminalTarget};
use crate::vivid::transport::{ReadShutdown, Reader, Writer};
use vivid_protocol::lease::{AttemptDecision, SessionLeaseDefinition};

use crate::display::SizeInfo;

#[cfg(windows)]
type LocalListener = TcpListener;
#[cfg(windows)]
type LocalStream = TcpStream;
#[cfg(unix)]
type LocalListener = UnixListener;
#[cfg(unix)]
type LocalStream = UnixStream;

fn fixed_to_f32(value: i64) -> f32 {
    value as f32 / (1_u64 << 32) as f32
}

pub(crate) const MAX_SESSIONS: usize = 16;
const MAX_CONNECTIONS: usize = 64;
/// How many accepted connections may sit in their handshake at once.
///
/// A connection is unauthenticated until `HELLO`, `CHANNEL_OPEN` or `LANE_OPEN` proves who is on
/// the other end, and until then anything that can reach the endpoint can hold a slot. Giving that
/// phase its own, much smaller budget means a peer that opens sockets and says nothing can never
/// consume the connection budget legitimate producers draw from — it can only fill this fraction
/// of it, and only for [`transport::HANDSHAKE_TIMEOUT`]. Sixteen is well above the handful of
/// control, track and lane connections a producer opens back to back at startup, and a quarter of
/// `MAX_CONNECTIONS`.
const MAX_PENDING_CONNECTIONS: usize = 16;
const MAX_ACTIVE_ANCHORS: usize = 4096;
const MAX_SEEN_ANCHORS: usize = 8192;
// Audio continuity takes precedence over live video freshness. This remains finite while covering
// ordinary SSH scheduling stalls; the producer drops video when its audio reserve begins filling.
const LIVE_AUDIO_FLOW_RESERVE_US: u64 = 2_000_000;
/// A recovery key frame is the largest unit a producer can send. Asking for one because media is
/// late, on a link that is late *because* it is saturated, is how a slow session becomes a stopped
/// one, so latency-driven requests are spaced at least this far apart.
const LATENCY_KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);
/// How often the live audio/video delay is allowed to shrink back toward zero.
const LIVE_DELAY_REVIEW: Duration = Duration::from_secs(5);
/// Video arrival margin kept when shrinking the delay, so a shrink cannot cause the next frame to
/// be late.
const LIVE_DELAY_HEADROOM_US: i64 = 100_000;
/// A live audio timeline step at least this large is a real gap, not capture or transport jitter.
/// Three Opus packets: smaller steps are bridged by the decoder's own continuity.
const AUDIO_GAP_US: i64 = 60_000;
const CHANNEL_OPEN_DEADLINE_US: u64 = 30_000_000;
/// The longest a single ingress-pacing sleep may last before the shortfall is recomputed.
const MAXIMUM_PACING_SLEEP: Duration = Duration::from_millis(50);
/// How long a video frame waiting on the linked audio clock may sleep before re-checking whether
/// that clock is still advancing and still the one to follow.
const LINKED_AUDIO_RECHECK: Duration = Duration::from_millis(20);
/// A floor on that wait, so a frame due imminently does not turn the sleep into a spin.
const MINIMUM_LINKED_AUDIO_WAIT: Duration = Duration::from_millis(1);
pub(crate) const MAX_SCENE_NODES: usize = 256;
const MAX_LEASES: usize = 32;
const MAX_STATUS_ENTRIES: usize = 64;
/// Core §4.3 caps a lane control body at 64 KiB.
const LANE_MAX_RECORD_BODY: u32 = 64 * 1024;
/// `SURFACE_CHANGED` changed-field bits, core §10.
const SURFACE_CHANGED_LIFECYCLE: u64 = 1 << 0;
const SURFACE_CHANGED_GEOMETRY: u64 = 1 << 1;
const SURFACE_CHANGED_SLOTS: u64 = 1 << 4;
/// `TRACK_CHANGED` changed-field bits, media §8.
const TRACK_CHANGED_LIFECYCLE: u64 = 1 << 0;
const TRACK_CHANGED_CHANNEL: u64 = 1 << 1;
const TRACK_CHANGED_ACTIVATION: u64 = 1 << 3;
const TRACK_CHANGED_AUDIO_GAIN: u64 = 1 << 4;
/// `SCENE_CHANGED` reason bits, core §10.
const SCENE_CHANGED_PRODUCER_COMMIT: u64 = 1 << 0;
/// `NEED_KEYFRAME` and `NEED_FULL_FRAME` reasons, media §13.
const NEED_KEYFRAME_DECODER_RESET: u64 = 2;
const NEED_FULL_FRAME_NO_BASE: u64 = 1;

struct SessionRuntime {
    identity: SessionIdentity,
    root_context: ContextIdentity,
    session_tag: [u8; 16],
    channel_key: Secret32,
    anchor_key: AnchorKey,
    accepted_profiles: Vec<String>,
    /// The control connection's egress, installed as soon as the session begins serving records.
    ///
    /// There is deliberately no writer here. The handshake is written by the connection's own
    /// thread before this exists; everything afterwards queues, because a blocking write from the
    /// PTY parser, the winit UI thread, a track channel, or another session's actor freezes
    /// whatever that thread was for.
    egress: Mutex<Option<Arc<Egress>>>,
    /// Wake the control actor when another connection introduces a deadline, notably an input
    /// binding arriving on the interactive lane.
    actor_ingress: Mutex<Option<mpsc::SyncSender<ActorMessage>>>,
    contexts: Mutex<HashMap<u64, ContextState>>,
    seen_anchors: Mutex<HashSet<(u64, u64)>>,
    /// The lease this session was activated from, if it is a leased child rather than a root.
    lease: Option<LeaseKey>,
    /// This generation's resume key, handed to the lease if the transport is lost uncleanly.
    resume_key: Secret32,
    /// The one interactive transport this session may have, core §7.
    lane: Mutex<Option<lane::LaneState>>,
    /// Its writer, independent of the control writer and of every track writer, so a saturated
    /// bulk track cannot delay input revocation. Held only so the lane's own reader thread can
    /// answer `PING` and `SET_INPUT_BINDING` inline; presenter-originated input goes through
    /// `lane_egress`.
    lane_writer: Mutex<Option<Arc<Writer>>>,
    /// The lane's egress. Input events and revocations are queued here because they originate on
    /// the winit UI thread and on the session actor, neither of which may block on a producer.
    lane_egress: Mutex<Option<Arc<Egress>>>,
    /// The desktop input grant, `desktop-input-v1`.
    grant: Mutex<InputGrant>,
    /// The bounded, coalescing observation queue, `observability-v1`.
    observations: Mutex<ObservationQueue>,
    /// Bounds what an anchor-marker flood can cost the PTY parser thread.
    markers: Mutex<MarkerAdmission>,
}

/// Anchor markers admitted for verification per second, per session.
///
/// Verifying a marker is an HMAC-SHA256, it runs on the PTY parser thread, and it runs before the
/// `seen_anchors` dedup can help: a marker whose tag names a live session but whose authenticator
/// is wrong is verified in full every single time. Any program that has ever seen one marker knows
/// the tag it carries, so a loop printing marker-shaped APCs could spend the terminal's output
/// thread on hashing. Admitting `MAX_ACTIVE_ANCHORS` a second costs a few milliseconds of work
/// where an unbounded flood cost a core, and no producer can notice the limit: the bucket starts
/// full at that same figure, so a complete anchor set still registers in one burst.
const MARKER_ADMISSION_RATE: u64 = MAX_ACTIVE_ANCHORS as u64;

/// A session's bounded budget for marker verification.
struct MarkerAdmission {
    bucket: TokenBucket,
    replenished: Instant,
    /// Markers this session has let through to verification — the work the budget bounds.
    admitted: u64,
}

impl MarkerAdmission {
    fn new(now: Instant) -> Self {
        Self { bucket: TokenBucket::new(MARKER_ADMISSION_RATE, 1), replenished: now, admitted: 0 }
    }

    /// Admit one marker for verification, or refuse to do the work.
    fn admit(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.replenished);
        self.replenished = now;
        if self.bucket.replenish(elapsed).is_err() || self.bucket.charge(1).is_err() {
            return false;
        }
        self.admitted = self.admitted.saturating_add(1);
        true
    }
}

#[derive(Default)]
struct Registry {
    sessions: HashMap<u64, Arc<SessionRuntime>>,
    /// Session tags to the sessions that own them, so an anchor marker off the PTY finds its
    /// session by lookup rather than by scanning every live one.
    by_tag: HashMap<[u8; 16], u64>,
    channel_opens: HashMap<TrackIdentity, ChannelOpenState>,
    leases: LeaseTable,
}

impl Registry {
    /// Publish a session under both the ID its producer uses and the tag its markers carry.
    fn insert_session(&mut self, runtime: Arc<SessionRuntime>) {
        self.by_tag.insert(runtime.session_tag, runtime.identity.session_id);
        self.sessions.insert(runtime.identity.session_id, runtime);
    }

    /// Retire one session from both indexes, leaving every other session's entries alone.
    fn remove_session(&mut self, session_id: u64) -> Option<Arc<SessionRuntime>> {
        let runtime = self.sessions.remove(&session_id)?;
        // A resume keeps the session ID but derives fresh key material, so by the time the old
        // runtime is retired its tag may already name the new one. Only unpublish a tag that
        // still points here.
        if self.by_tag.get(&runtime.session_tag) == Some(&session_id) {
            self.by_tag.remove(&runtime.session_tag);
        }
        Some(runtime)
    }

    fn session_by_tag(&self, tag: &[u8; 16]) -> Option<&Arc<SessionRuntime>> {
        self.sessions.get(self.by_tag.get(tag)?)
    }
}

struct ServiceShared {
    root_secret: Secret32,
    presenter: PresenterInstanceId,
    scene: SharedScene,
    registry: Mutex<Registry>,
    audio_outputs: Mutex<HashMap<TrackIdentity, Arc<AudioOutput>>>,
    next_session: AtomicU64,
    active_connections: AtomicUsize,
    pending_handshakes: Mutex<PendingHandshakes>,
    wake: Arc<dyn Fn() + Send + Sync>,
    wake_pending: AtomicBool,
    frame_wake_events: AtomicU64,
    actor_timeout_services: AtomicU64,
    trace: Mutex<trace::TraceJournal>,
    file_drops: Mutex<file_drop::FileDropManager>,
}

enum ActorMessage {
    Record(Record),
    Wake,
}

/// One accepted connection's claim on the global connection budget.
///
/// Moving this into the serving closure makes ordinary return, panic unwinding, and failed thread
/// creation release the claim through the same path.
struct ConnectionSlot {
    shared: Arc<ServiceShared>,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let previous = self.shared.active_connections.fetch_sub(1, Ordering::AcqRel);
        self.shared.trace(
            trace::TraceCategory::Connection,
            "connection_closed",
            None,
            serde_json::json!({"active_connections": previous.saturating_sub(1)}),
        );
    }
}

/// Retire one published control session exactly once on every exit path.
struct SessionCleanup {
    shared: Arc<ServiceShared>,
    session: Arc<SessionRuntime>,
    egress: Arc<Egress>,
    clean_goodbye: Arc<AtomicBool>,
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        *lock(&self.session.actor_ingress) = None;
        self.egress.close();
        self.egress.join();
        finish_session(&self.shared, &self.session, self.clean_goodbye.load(Ordering::Acquire));
    }
}

/// Undo one accepted track transport unless its normal loop already detached it.
struct TrackAttachmentCleanup {
    shared: Arc<ServiceShared>,
    identity: TrackIdentity,
    generation: ChannelGeneration,
    armed: bool,
}

impl TrackAttachmentCleanup {
    fn detach(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(state) = lock(&self.shared.registry).channel_opens.get_mut(&self.identity) {
            state.transport_lost(self.generation);
        }
        let _ = self.shared.scene.detach_channel(self.identity, self.generation);
        self.armed = false;
    }
}

impl Drop for TrackAttachmentCleanup {
    fn drop(&mut self) {
        // Normal exits explicitly detach before this guard is dropped. Reaching Drop while still
        // armed means setup returned early or the channel worker unwound; do not leave a realtime
        // audio producer alive without its owning transport.
        if self.armed {
            stop_failed_audio_output(&self.shared.audio_outputs, self.identity);
        }
        self.detach();
    }
}

/// Release one admitted interactive transport without touching a later generation.
struct LaneCleanup {
    session: Arc<SessionRuntime>,
    generation: u64,
    writer: Arc<Writer>,
    egress: Option<Arc<Egress>>,
}

impl Drop for LaneCleanup {
    fn drop(&mut self) {
        let owns_lane = lock(&self.session.lane)
            .as_ref()
            .is_some_and(|state| state.generation() == self.generation);
        if owns_lane {
            lane::confirm_lost(&mut lock(&self.session.lane), self.generation);
            revoke_input(&self.session, grant_reason::LANE_LOSS);
        }
        let mut writer = lock(&self.session.lane_writer);
        if writer.as_ref().is_some_and(|current| Arc::ptr_eq(current, &self.writer)) {
            *writer = None;
        }
        drop(writer);
        if let Some(egress) = &self.egress {
            let mut installed = lock(&self.session.lane_egress);
            if installed.as_ref().is_some_and(|current| Arc::ptr_eq(current, egress)) {
                *installed = None;
            }
            drop(installed);
            egress.close();
            egress.join();
        }
    }
}

/// The accepted connections that have not authenticated yet, oldest first.
#[derive(Default)]
struct PendingHandshakes {
    next_id: u64,
    open: Vec<(u64, LocalStream)>,
}

/// One accepted connection's claim on the pre-handshake budget.
///
/// Held from accept until the peer authenticates, then released by
/// [`PendingConnection::authenticated`], which is also where the connection's handshake deadline is
/// lifted — the two are the same fact, so they are established in one place. A connection that
/// fails, is evicted, or is dropped before that releases its claim through `Drop`.
struct PendingConnection {
    shared: Arc<ServiceShared>,
    id: u64,
}

impl PendingConnection {
    /// Admit a freshly accepted connection to the pre-handshake budget.
    ///
    /// The budget is enforced by evicting the oldest unauthenticated connection rather than by
    /// refusing the newest arrival. Refusing would hand an attacker the outcome it wants: sixteen
    /// silent sockets would bar every real producer from the endpoint until their deadlines
    /// expired. Evicting inverts that — a producer authenticates in the time it takes to write one
    /// record, so the oldest pending connection is all but always a peer that has said nothing,
    /// and a flood only evicts itself.
    fn admit(shared: &Arc<ServiceShared>, stream: &LocalStream) -> io::Result<Self> {
        let handle = stream.try_clone()?;
        let mut pending = lock(&shared.pending_handshakes);
        let id = pending.next_id;
        pending.next_id = pending.next_id.wrapping_add(1);
        pending.open.push((id, handle));
        while pending.open.len() > MAX_PENDING_CONNECTIONS {
            let (_, evicted) = pending.open.remove(0);
            // The evicted reader is parked in a blocking read; the shutdown ends it, and its own
            // `PendingConnection` is already gone from this list.
            let _ = evicted.shutdown(std::net::Shutdown::Both);
        }
        drop(pending);
        Ok(Self { shared: shared.clone(), id })
    }

    /// Record that this connection's peer is authenticated.
    fn authenticated(&self, reader: &mut Reader) -> io::Result<()> {
        reader.finish_handshake()?;
        self.release();
        Ok(())
    }

    fn release(&self) {
        lock(&self.shared.pending_handshakes).open.retain(|(id, _)| *id != self.id);
    }
}

impl Drop for PendingConnection {
    fn drop(&mut self) {
        self.release();
    }
}

pub struct VividService {
    control_endpoint: String,
    root_secret: String,
    scene: SharedScene,
    shared: Arc<ServiceShared>,
    shutdown: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
    _directory: Option<TempDir>,
}

impl VividService {
    pub fn start(geometry: DisplayGeometry, event_proxy: EventProxy) -> io::Result<Self> {
        Self::start_with_wake(
            geometry,
            Arc::new(move || event_proxy.send_event(EventType::VividFrame)),
        )
    }

    /// Start a window that presents `desktop-surface-v1` instead of a terminal.
    ///
    /// Stage 1 D1: a window presents exactly one target profile, chosen when it is created, so a
    /// session never has to reconcile two coordinate truths.
    pub fn start_desktop(geometry: DisplayGeometry, event_proxy: EventProxy) -> io::Result<Self> {
        let target = Arc::new(DesktopTarget::new(geometry).map_err(io::Error::other)?);
        Self::start_with_target(
            target,
            Arc::new(move || event_proxy.send_event(EventType::VividFrame)),
        )
    }

    fn start_with_wake(
        geometry: DisplayGeometry,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> io::Result<Self> {
        let target = Arc::new(TerminalTarget::new(geometry).map_err(io::Error::other)?);
        Self::start_with_target(target, wake)
    }

    fn start_with_target(
        target: Arc<dyn PresentationTarget>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> io::Result<Self> {
        let (listener, control_endpoint, directory) = bind_local_listener()?;
        let mut secret = [0_u8; 32];
        getrandom::fill(&mut secret).map_err(|error| {
            io::Error::other(format!("could not generate root secret: {error}"))
        })?;
        let mut presenter = [0_u8; 16];
        getrandom::fill(&mut presenter).map_err(|error| {
            io::Error::other(format!("could not generate presenter identity: {error}"))
        })?;
        let root_secret = encode_hex(&secret);
        let scene = SharedScene::new(target);
        let shared = Arc::new(ServiceShared {
            root_secret: Secret32::new(secret),
            presenter: PresenterInstanceId(presenter),
            scene: scene.clone(),
            registry: Mutex::new(Registry::default()),
            audio_outputs: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            active_connections: AtomicUsize::new(0),
            pending_handshakes: Mutex::new(PendingHandshakes::default()),
            wake,
            wake_pending: AtomicBool::new(false),
            frame_wake_events: AtomicU64::new(0),
            actor_timeout_services: AtomicU64::new(0),
            trace: Mutex::new(trace::TraceJournal::default()),
            file_drops: Mutex::new(file_drop::FileDropManager::default()),
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let listener_thread = thread::Builder::new().name("vivid-1.5-listener".into()).spawn({
            let shared = shared.clone();
            let shutdown = shutdown.clone();
            move || listener_loop(listener, shared, shutdown)
        })?;
        Ok(Self {
            control_endpoint,
            root_secret,
            scene,
            shared,
            shutdown,
            listener_thread: Some(listener_thread),
            _directory: directory,
        })
    }

    pub fn control_endpoint(&self) -> &str {
        &self.control_endpoint
    }

    pub fn root_secret(&self) -> &str {
        &self.root_secret
    }

    pub fn scene(&self) -> SharedScene {
        self.scene.clone()
    }

    /// Acknowledge the one coalesced frame event before consuming the latest retained scene.
    pub fn acknowledge_frame_wake(&self) {
        self.shared.wake_pending.store(false, Ordering::Release);
    }

    /// Accept a new terminal geometry, returning the target generation assigned to it.
    ///
    /// Every later `WELCOME` reports the accepted geometry, and the change is queued for the next
    /// `flush_display_change` so live sessions observe it as `TARGET_CHANGED`.
    pub fn update_metrics(&self, geometry: DisplayGeometry) -> Option<u64> {
        self.shared.scene.target().offer_geometry(geometry)
    }

    /// Announce a queued display change, or re-announce the current one as settled.
    ///
    /// A resize is announced unsettled on the frame that applies it, and the settle timer only
    /// fires afterwards. The settled announcement therefore has to be rebuilt from the current
    /// metrics, because the queued change was already consumed by that earlier frame.
    pub fn flush_display_change(&self, settled_generation: Option<u64>) {
        let Some(change) = self.shared.scene.target().take_change(settled_generation) else {
            return;
        };

        // The scene validates every commit against the generation it was planned for, so it has
        // to reach the new target before any producer can name it.
        self.scene.advance_target_generation(TargetGeneration::new(change.generation));

        let body = target_change_body(&change);
        // This runs on the thread that draws the window. Queue the announcement rather than
        // writing it: one producer that has stopped reading its control replies would otherwise
        // block the redraw of every window.
        let sessions = lock(&self.shared.registry).sessions.values().cloned().collect::<Vec<_>>();
        for session in sessions {
            if !session.post_control(messages::TARGET_CHANGED, 0, body.clone()) {
                log::debug!(
                    "could not queue TARGET_CHANGED for Vivid session {}",
                    session.identity.session_id
                );
            }
        }
    }

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
        let updates = anchors.into_iter().zip(positions).map(|((identity, _, _, _), position)| {
            (
                identity,
                position.map(|position| {
                    (position.point.column.0, position.point.line.0, position.alternate)
                }),
            )
        });
        let removed = self.scene.apply_anchor_resize(updates);
        self.notify_anchor_events(messages::ANCHOR_GONE, &removed);
    }

    pub fn handle_terminal_marker(&self, marker: &str, line: i32, column: usize, alternate: bool) {
        if !self.scene.target().accepts_anchors() {
            return;
        }
        let Ok(marker) = anchor::parse_marker(marker) else {
            return;
        };
        // A tag naming no live session is refused by lookup, before anything is hashed.
        let session = lock(&self.shared.registry).session_by_tag(&marker.session_tag).cloned();
        let Some(session) = session else {
            return;
        };
        // Then the session's own budget, still before hashing: whoever is printing these markers
        // is not necessarily the producer whose tag they carry.
        if !lock(&session.markers).admit(Instant::now()) {
            return;
        }
        if !anchor::verify_marker(&session.anchor_key, &marker)
            || lock(&session.contexts)
                .get(&marker.context_id)
                .is_none_or(|context| context.operation_classes & OP_TERMINAL_ANCHOR == 0)
        {
            return;
        }
        let Ok(context) = session.identity.context(marker.context_id) else {
            return;
        };
        let Ok(identity) = context.anchor(marker.anchor_id) else {
            return;
        };
        let mut seen = lock(&session.seen_anchors);
        if seen.len() >= MAX_SEEN_ANCHORS
            || !seen.insert((marker.context_id, marker.anchor_id))
            || seen.len() > MAX_ACTIVE_ANCHORS
        {
            return;
        }
        drop(seen);
        if let Err(error) = self.scene.add_anchor(identity, column, line, alternate) {
            log::debug!("rejected Vivid anchor {identity:?}: {error}");
            return;
        }
        let body = Envelope::new(
            0,
            vec![
                (0, Value::Unsigned(marker.context_id)),
                (1, Value::Unsigned(marker.anchor_id)),
                (2, Value::Unsigned(1)),
                (3, Value::Unsigned(column as u64)),
                (4, Value::Unsigned(u64::try_from(line).unwrap_or_default())),
                (5, Value::Bool(line >= 0)),
                (6, Value::Unsigned(self.shared.scene.target().generation())),
            ],
        )
        .encode()
        .expect("anchor event is valid");
        // The PTY parser thread is here. Writing the socket directly would stop terminal output
        // for as long as the producer declines to read it.
        session.post_control(messages::ANCHOR_READY, marker.anchor_id, body);
        self.shared.request_frame_wake();
    }

    pub fn handle_grid_scroll(&self, origin: i32, end: i32, lines: i32, history_size: usize) {
        let removed = self.scene.scroll_anchors(origin, end, lines, history_size);
        self.notify_anchor_events(messages::ANCHOR_GONE, &removed);
        self.shared.request_frame_wake();
    }

    pub fn handle_terminal_clear(&self) {
        let removed = self.scene.clear_terminal();
        self.notify_anchor_events(messages::ANCHOR_GONE, &removed);
        self.shared.request_frame_wake();
    }

    pub fn handle_screen_swap(&self, alternate: bool) {
        let removed = self.scene.set_alternate_screen(alternate);
        self.notify_anchor_events(messages::ANCHOR_GONE, &removed);
        self.shared.request_frame_wake();
    }

    pub fn update_visibility(&self, _visible: bool, _display_offset: usize) {}

    /// Vivido-owned hover text for the currently effective file-drop binding.
    pub fn file_drop_hover_label(
        &self,
        x: usize,
        y: usize,
        size: &SizeInfo,
        display_offset: usize,
    ) -> Option<&'static str> {
        let hit = self.file_drop_surface_at(x, y, size, display_offset);
        lock(&self.shared.file_drops).hover_label(hit)
    }

    /// Route one local regular file to the effective binding without ever exposing its path.
    pub(crate) fn handle_file_drop(
        &self,
        path: &std::path::Path,
        x: usize,
        y: usize,
        size: &SizeInfo,
        display_offset: usize,
    ) -> file_drop::LocalDropDisposition {
        let hit = self.file_drop_surface_at(x, y, size, display_offset);
        let (disposition, offer) = lock(&self.shared.file_drops).offer_local_file(path, hit);
        let Some((owner, offer)) = offer else {
            return disposition;
        };
        let session = lock(&self.shared.registry).sessions.get(&owner.session_id).cloned();
        let Some(session) = session else {
            return file_drop::LocalDropDisposition::Rejected("The remote receiver disconnected");
        };
        let Ok(payload) = offer.payload() else {
            return file_drop::LocalDropDisposition::Rejected("The dropped filename is invalid");
        };
        let Ok(body) = Envelope::new(0, payload).encode() else {
            return file_drop::LocalDropDisposition::Rejected(
                "The file-drop offer could not be encoded",
            );
        };
        if session.post_control(messages::FILE_DROP_OFFER, offer.binding.drop_id, body) {
            if let Some(ingress) = lock(&session.actor_ingress).as_ref() {
                let _ = ingress.try_send(ActorMessage::Wake);
            }
            disposition
        } else {
            file_drop::LocalDropDisposition::Rejected("The remote receiver stopped responding")
        }
    }

    /// Resolve the topmost rendered Vivid surface under a window-space pointer.
    ///
    /// This deliberately uses the same fixed-point placement, target scale, scroll offset, clip,
    /// and z-order as the renderer. A surface-qualified binding therefore cannot capture a drop
    /// merely because it is the only such binding in the process.
    fn file_drop_surface_at(
        &self,
        x: usize,
        y: usize,
        size: &SizeInfo,
        display_offset: usize,
    ) -> Option<(SurfaceIdentity, SurfaceGeneration)> {
        let (scale_x, scale_y) = self.scene.target().placement_scale(size);
        let pointer_x = x as f32;
        let pointer_y = y as f32;
        self.scene.snapshot().items.iter().rev().find_map(|item| {
            let scroll = if item.text_anchored { display_offset as f32 } else { 0.0 };
            let left = size.padding_x() + fixed_to_f32(item.x) * scale_x;
            let top = size.padding_y() + (fixed_to_f32(item.y) + scroll) * scale_y;
            let mut right = left + fixed_to_f32(item.width) * scale_x;
            let mut bottom = top + fixed_to_f32(item.height) * scale_y;
            let mut left = left.max(0.0);
            let mut top = top.max(0.0);
            right = right.min(size.width());
            bottom = bottom.min(size.height());
            if let Some(clip) = item.clip {
                let clip_left = size.padding_x() + fixed_to_f32(clip.x) * scale_x;
                let clip_top = size.padding_y() + (fixed_to_f32(clip.y) + scroll) * scale_y;
                left = left.max(clip_left);
                top = top.max(clip_top);
                right = right.min(clip_left + fixed_to_f32(clip.width) * scale_x);
                bottom = bottom.min(clip_top + fixed_to_f32(clip.height) * scale_y);
            }
            (pointer_x >= left && pointer_x < right && pointer_y >= top && pointer_y < bottom)
                .then_some((item.surface_key, item.surface_generation))
        })
    }

    /// Send one desktop input event to whichever session holds the grant.
    ///
    /// This is the presenter-to-producer direction of `desktop-input-v1`: the window's input
    /// handler calls it, and every event carries the complete binding tuple so the producer's
    /// final injection gate can reject a stale one. An event the presenter cannot tag is dropped
    /// rather than sent, because sending it would only widen the window in which a stale tuple
    /// exists on the wire.
    pub fn send_input(&self, event: InputEvent) -> bool {
        let sessions = lock(&self.shared.registry).sessions.values().cloned().collect::<Vec<_>>();
        for session in sessions {
            let tagged = {
                let grant = lock(&session.grant);
                grant.tag(event.class()).map(|tag| grant::event_payload(tag, &event))
            };
            let Some(payload) = tagged else {
                continue;
            };
            let surface_id = payload
                .iter()
                .find(|entry| entry.0 == 3)
                .and_then(|entry| entry.1.as_u64())
                .unwrap_or(0);
            let Ok(body) = Envelope::new(0, payload).encode() else {
                continue;
            };
            // The winit event loop calls this for every keystroke and pointer motion. Admission is
            // the queue: core §7 makes a generation unrepeatable once an event is admitted, and a
            // producer that stops draining its lane must lose input, not freeze the window.
            if session.post_lane(event.record_type(), surface_id, body) {
                // Core §7: once an event is admitted, this lane generation cannot be reopened.
                if let Some(state) = lock(&session.lane).as_mut() {
                    state.note_input();
                }
                return true;
            }
        }
        false
    }

    /// Revoke input on every session, for a focus or policy transition the window observed.
    pub fn revoke_all_input(&self, reason: u64) {
        let sessions = lock(&self.shared.registry).sessions.values().cloned().collect::<Vec<_>>();
        for session in sessions {
            revoke_input(&session, reason);
        }
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn automation_sessions(&self) -> Vec<SessionIdentity> {
        self.scene.session_ids()
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn automation_surface_keys(&self) -> Vec<SurfaceIdentity> {
        self.scene.surface_keys()
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn automation_surface_status(
        &self,
        identity: SurfaceIdentity,
    ) -> Option<SurfaceStatus> {
        self.scene.surface_status(identity)
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn automation_track_keys(&self) -> Vec<TrackIdentity> {
        self.scene.track_keys()
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn automation_track_status(&self, identity: TrackIdentity) -> Option<TrackStatus> {
        self.scene.track_status(identity)
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn automation_streaming_metrics(&self) -> serde_json::Value {
        let metrics = self
            .scene
            .track_keys()
            .into_iter()
            .filter_map(|identity| self.scene.track_status(identity))
            .fold(scene::TrackMetrics::default(), |mut total, status| {
                total.decoded_frames =
                    total.decoded_frames.saturating_add(status.metrics.decoded_frames);
                total.discarded_late_frames = total
                    .discarded_late_frames
                    .saturating_add(status.metrics.discarded_late_frames);
                total.latency_keyframe_requests = total
                    .latency_keyframe_requests
                    .saturating_add(status.metrics.latency_keyframe_requests);
                total.audio_rebases =
                    total.audio_rebases.saturating_add(status.metrics.audio_rebases);
                total
            });
        serde_json::json!({
            "decoded_frames": metrics.decoded_frames,
            "discarded_late_frames": metrics.discarded_late_frames,
            "latency_keyframe_requests": metrics.latency_keyframe_requests,
            "audio_rebases": metrics.audio_rebases,
            "frame_wake_events": self.shared.frame_wake_events.load(Ordering::Acquire),
            "actor_timeout_services": self.shared.actor_timeout_services.load(Ordering::Acquire),
            "snapshot_rebuilds": self.scene.optimization_metrics().snapshot_rebuilds,
        })
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn automation_scene_status(
        &self,
        session: SessionIdentity,
        maximum_nodes: u64,
    ) -> SceneStatus {
        self.scene.scene_status(
            session,
            usize::try_from(maximum_nodes).unwrap_or(MAX_SCENE_NODES).min(MAX_SCENE_NODES),
        )
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn automation_evaluate_wait(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        condition: u64,
        value: Option<u64>,
    ) -> TrackWaitEvaluation {
        self.scene.evaluate_track_wait(identity, generation, condition, value)
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn automation_trace(
        &self,
        selection: trace::TraceSelection,
        limit: u16,
        filter: trace::TraceFilter,
    ) -> trace::TraceBatch {
        lock(&self.shared.trace).query(selection, limit, filter)
    }

    fn notify_anchor_events(&self, record_type: u16, anchors: &[AnchorIdentity]) {
        if anchors.is_empty() {
            return;
        }
        let sessions = lock(&self.shared.registry).sessions.clone();
        for identity in anchors {
            let Some(session) = sessions.get(&identity.context.session.session_id) else {
                continue;
            };
            let body = Envelope::new(
                0,
                vec![
                    (0, Value::Unsigned(identity.context.context_id)),
                    (1, Value::Unsigned(identity.anchor_id)),
                    (2, Value::Unsigned(2)),
                ],
            )
            .encode();
            if let Ok(body) = body {
                // Reached from the PTY parser on every scroll, clear, and screen swap.
                session.post_control(record_type, identity.anchor_id, body);
            }
        }
    }
}

impl SessionRuntime {
    fn supports(&self, profile: &str) -> bool {
        self.accepted_profiles.binary_search_by(|value| value.as_str().cmp(profile)).is_ok()
    }

    /// Queue one record on the session's control connection.
    ///
    /// This never blocks and never fails the caller's own work. `false` means the record was not
    /// admitted — the session has not begun serving yet, or its peer stopped draining and the
    /// egress overflowed, which closes the session on its own thread.
    fn post_control(&self, record_type: u16, object_id: u64, body: Vec<u8>) -> bool {
        let egress = lock(&self.egress).clone();
        egress.is_some_and(|egress| egress.send(record_type, object_id, body))
    }

    /// Queue one record on the session's interactive lane, with the same guarantees.
    fn post_lane(&self, record_type: u16, object_id: u64, body: Vec<u8>) -> bool {
        let egress = lock(&self.lane_egress).clone();
        egress.is_some_and(|egress| egress.send(record_type, object_id, body))
    }

    fn wake_actor(&self) {
        if let Some(ingress) = lock(&self.actor_ingress).as_ref() {
            let _ = ingress.try_send(ActorMessage::Wake);
        }
    }
}

impl ServiceShared {
    fn trace(
        &self,
        category: trace::TraceCategory,
        event: &'static str,
        track: Option<TrackIdentity>,
        data: serde_json::Value,
    ) {
        lock(&self.trace).push(category, event, track, data);
        self.request_frame_wake();
    }

    fn request_frame_wake(&self) {
        if self
            .wake_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.frame_wake_events.fetch_add(1, Ordering::Relaxed);
            (self.wake)();
        }
    }

    /// Build a complete track identity for a session this presenter owns.
    #[cfg(test)]
    fn presenter_track(
        &self,
        session_id: u64,
        context_id: u64,
        surface_id: u64,
        track_id: u64,
    ) -> TrackIdentity {
        SessionIdentity::new(self.presenter, session_id)
            .and_then(|session| session.context(context_id))
            .and_then(|context| context.surface(surface_id))
            .and_then(|surface| surface.track(track_id))
            .expect("a well-formed track identity")
    }
}

impl Drop for VividService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        wake_listener(&self.control_endpoint);
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }
    }
}

fn listener_loop(listener: LocalListener, shared: Arc<ServiceShared>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        let stream = match accept_stream(&listener) {
            Ok(stream) => stream,
            Err(_error) if shutdown.load(Ordering::Acquire) => break,
            Err(error) => {
                log::debug!("Vivid accept failed: {error}");
                continue;
            },
        };
        // Charge the pre-handshake budget before the connection budget, so unauthenticated peers
        // are bounded among themselves before they are counted against producers.
        let pending = match PendingConnection::admit(&shared, &stream) {
            Ok(pending) => pending,
            Err(error) => {
                log::debug!("Vivid connection refused before its handshake: {error}");
                continue;
            },
        };
        if shared.active_connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            shared.active_connections.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        shared.trace(
            trace::TraceCategory::Connection,
            "connection_accepted",
            None,
            serde_json::json!({
                "active_connections": shared.active_connections.load(Ordering::Acquire),
            }),
        );
        let slot = ConnectionSlot { shared: shared.clone() };
        let connection = shared.clone();
        let served = thread::Builder::new().name("vivid-1.5-connection".into()).spawn(move || {
            let _slot = slot;
            if let Err(error) = handle_connection(stream, &connection, &pending) {
                log::debug!("Vivid connection closed: {error}");
            }
        });
        if let Err(error) = served {
            // Dropping the failed spawn closure releases its `ConnectionSlot`.
            log::debug!("could not serve a Vivid connection: {error}");
        }
    }
}

fn handle_connection(
    stream: LocalStream,
    shared: &Arc<ServiceShared>,
    pending: &PendingConnection,
) -> io::Result<()> {
    let (mut reader, preface, preface_bytes) = Reader::new(stream)?;
    match preface.kind {
        ConnectionKind::Control => handle_control(&mut reader, &preface_bytes, shared, pending),
        ConnectionKind::Track => handle_track_channel(&mut reader, shared, pending),
        ConnectionKind::Lane => handle_lane(&mut reader, shared, pending),
        ConnectionKind::FileTransfer => file_drop::handle_connection(&mut reader, shared, pending),
    }
}

fn handle_control(
    reader: &mut Reader,
    preface: &[u8; 16],
    shared: &Arc<ServiceShared>,
    pending: &PendingConnection,
) -> io::Result<()> {
    let writer = Arc::new(reader.writer(ConnectionKind::Control)?);
    let first = reader.read_record(ConnectionKind::Control)?;
    let (hello_request, hello) = Hello::decode(&first.body)?;
    // Install the bounded egress before publishing the session. The registry lock held during
    // establishment keeps outside announcements behind WELCOME, and once it is released every
    // visible session is already able to queue them.
    let egress = Egress::start(writer.clone(), "vivid-control-egress")?;
    let session = match establish_root_session(
        shared,
        writer.clone(),
        egress.clone(),
        preface,
        &hello,
        hello_request,
    ) {
        Ok(session) => session,
        Err(error) => {
            egress.close();
            egress.join();
            return Err(error);
        },
    };
    let clean_goodbye = Arc::new(AtomicBool::new(false));
    let _cleanup = SessionCleanup {
        shared: shared.clone(),
        session: session.clone(),
        egress: egress.clone(),
        clean_goodbye: clean_goodbye.clone(),
    };
    // `HELLO` has been proved and answered: this is a session now, free to idle as long as it
    // likes, and no longer charged against the pre-handshake budget. `WELCOME` has already been
    // written by the time these can fail, so the session exists and has to be retired rather than
    // abandoned — a peer that closed between `WELCOME` and here is exactly how they fail.
    let established = reader
        .set_maximum(hello.maximum_control_body)
        .and_then(|()| writer.set_maximum(hello.maximum_control_body))
        .and_then(|()| pending.authenticated(reader));
    established?;

    // A session is a reader, an actor, and an egress. This thread is the reader: it parses and
    // enqueues, and never writes, so a peer that stops draining its replies cannot stall parsing.
    // An overflow here is caused by whichever thread posted the record that did not fit, which is
    // often not this session's own. Let the egress wake this reader so the session is reclaimed
    // rather than lingering until its peer decides to close.
    egress.set_shutdown(reader.shutdown_handle()?);
    let (records, incoming) = mpsc::sync_channel::<ActorMessage>(actor::INGRESS_CAPACITY);
    *lock(&session.actor_ingress) = Some(records.clone());
    let shutdown = reader.shutdown_handle()?;
    let actor = {
        let actor_shared = shared.clone();
        let actor_session = session.clone();
        let actor_egress = egress.clone();
        let panic_egress = egress.clone();
        let actor_clean_goodbye = clean_goodbye.clone();
        let panic_shutdown = shutdown.clone();
        let actor = thread::Builder::new().name("vivid-control-actor".into()).spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                actor_loop(
                    actor_shared,
                    actor_session,
                    incoming,
                    actor_egress,
                    actor_clean_goodbye,
                    shutdown,
                )
            }));
            if let Err(payload) = result {
                panic_egress.close();
                panic_shutdown.stop();
                panic::resume_unwind(payload);
            }
        });
        match actor {
            Ok(actor) => actor,
            Err(error) => {
                *lock(&session.actor_ingress) = None;
                drop(records);
                return Err(error);
            },
        }
    };

    loop {
        let record = match reader.read_record(ConnectionKind::Control) {
            Ok(record) => record,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => {
                *lock(&session.actor_ingress) = None;
                drop(records);
                let _ = actor.join();
                return Err(error);
            },
        };
        if records.send(ActorMessage::Record(record)).is_err() {
            break;
        }
    }
    *lock(&session.actor_ingress) = None;
    drop(records);
    let _ = actor.join();
    if egress.overflowed() {
        log::debug!(
            "Vivid session {} closed: the producer stopped draining its control replies",
            session.identity.session_id
        );
    }
    Ok(())
}

/// The session actor: owns mutable session state, applies mutations in receive order, and services
/// outstanding operations on a tick so a long one never blocks the next record.
fn actor_loop(
    shared: Arc<ServiceShared>,
    session: Arc<SessionRuntime>,
    incoming: mpsc::Receiver<ActorMessage>,
    egress: Arc<Egress>,
    clean_goodbye: Arc<AtomicBool>,
    shutdown: ReadShutdown,
) {
    let contract = presenter_contract();
    let mut pending = PendingSet::new(
        contract.get(Resource::RegisteredWaits),
        contract.get(Resource::PendingRequests),
    );
    let mut cancelled = HashSet::new();
    let mut admitted_post_hello = false;
    loop {
        let now = Instant::now();
        let timeout = actor_wait_timeout(&shared, &session, pending.observation_timeout(now), now);
        let received = match timeout {
            Some(timeout) => incoming.recv_timeout(timeout),
            None => incoming.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(ActorMessage::Record(record)) => {
                // Security §6.4 step 6: once any post-`HELLO` record is admitted, the activation
                // secret can no longer open a transport and recovery must use the resume proof.
                if !admitted_post_hello {
                    admitted_post_hello = true;
                    admit_post_hello(&shared, &session);
                }
                if !dispatch_and_reply(
                    &shared,
                    &session,
                    &record,
                    &egress,
                    &mut pending,
                    &mut cancelled,
                ) {
                    break;
                }
                if record.record_type == messages::GOODBYE {
                    clean_goodbye.store(true, Ordering::Release);
                    break;
                }
            },
            Ok(ActorMessage::Wake) => {},
            Err(mpsc::RecvTimeoutError::Timeout) => {
                shared.actor_timeout_services.fetch_add(1, Ordering::Relaxed);
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !pending.is_empty() {
            pending.service(&shared.scene, &egress, &mut cancelled, Instant::now());
        }
        expire_leases(&shared, &session);
        service_input_renewal(&session);
        drain_observations(&session, &egress);
        service_file_drop_timeouts(&shared, &session, &egress);
    }
    egress.close();
    // Flush the final reply (notably GOODBYE's OK) before closing the socket. On Windows a
    // receive-only shutdown does not reliably release another thread blocked in `recv`, so the
    // shutdown handle closes both halves only after egress has drained.
    egress.join();
    shutdown.stop();
}

/// Compute how long an actor may sleep before some time-based responsibility becomes due.
/// `None` means there is no deadline and the actor may block until a message arrives.
fn actor_wait_timeout(
    shared: &ServiceShared,
    session: &SessionRuntime,
    pending_timeout: Option<Duration>,
    now: Instant,
) -> Option<Duration> {
    let file_drop_timeout = lock(&shared.file_drops)
        .next_deadline(session.identity)
        .map(|deadline| deadline.saturating_duration_since(now));
    let pending_timeout = minimum_timeout(pending_timeout, file_drop_timeout);
    let lease_timeout = lock(&shared.registry)
        .leases
        .next_deadline(session.identity)
        .map(|deadline| deadline.saturating_duration_since(now));

    let monotonic_now = clock::from_instant(now);
    let renewal_timeout = lock(&session.grant).active().and_then(|active| {
        active.watchdog_deadline.checked_sub_micros(active.watchdog_timeout_us / 2).map(
            |deadline| {
                Duration::from_micros(
                    deadline.as_micros().saturating_sub(monotonic_now.as_micros()),
                )
            },
        )
    });
    select_actor_wait_timeout(pending_timeout, lease_timeout, renewal_timeout)
}

fn service_file_drop_timeouts(shared: &ServiceShared, session: &SessionRuntime, egress: &Egress) {
    let cancellations = lock(&shared.file_drops).service_timeouts(session.identity);
    for cancellation in cancellations {
        let Ok(payload) = cancellation.payload() else {
            continue;
        };
        let Ok(body) = Envelope::new(0, payload).encode() else {
            continue;
        };
        if !egress.send(messages::FILE_DROP_CANCELLED, cancellation.binding.drop_id, body) {
            break;
        }
    }
}

/// Pure deadline policy, separated from the actor's mutable registries for deterministic tests.
fn select_actor_wait_timeout(
    pending_timeout: Option<Duration>,
    lease_timeout: Option<Duration>,
    renewal_timeout: Option<Duration>,
) -> Option<Duration> {
    minimum_timeout(minimum_timeout(pending_timeout, lease_timeout), renewal_timeout)
}

fn minimum_timeout(left: Option<Duration>, right: Option<Duration>) -> Option<Duration> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(timeout), None) | (None, Some(timeout)) => Some(timeout),
        (None, None) => None,
    }
}

/// Dispatch one record and deliver whatever it produced. Returns false when the session must end.
fn dispatch_and_reply(
    shared: &Arc<ServiceShared>,
    session: &Arc<SessionRuntime>,
    record: &Record,
    egress: &Egress,
    pending: &mut PendingSet,
    cancelled: &mut HashSet<u64>,
) -> bool {
    match dispatch_control(shared, session, record, pending, cancelled) {
        Ok(Some(reply)) => actor::deliver(egress, reply).is_ok(),
        Ok(None) => true,
        Err(error) => {
            let request_id = messages::decode_control(&record.body)
                .map(|envelope| envelope.request_id)
                .unwrap_or(0);
            let fatal = request_id == 0;
            if error.trace_rejection
                && let Some((operation, category)) = traced_track_control(record.record_type)
            {
                shared.trace(
                    category,
                    "track_control_rejected",
                    error.track,
                    serde_json::json!({
                        "operation": operation,
                        "request_id": request_id,
                        "control_record_sequence": record.sequence,
                        "record_type": record.record_type,
                        "object_id": record.object_id,
                        "error_code": error.code,
                        "diagnostic": error.message,
                        "fatal": fatal,
                    }),
                );
            }
            let Ok(body) = protocol_error(request_id, error.code, fatal, error.message) else {
                return false;
            };
            if !egress.send(messages::ERROR, record.object_id, body) {
                return false;
            }
            !fatal
        },
    }
}

fn traced_track_control(record_type: u16) -> Option<(&'static str, trace::TraceCategory)> {
    let lifecycle = trace::TraceCategory::Lifecycle;
    let playback = trace::TraceCategory::Playback;
    match record_type {
        messages::CREATE_TRACK => Some(("create_track", lifecycle)),
        messages::DESTROY_TRACK => Some(("destroy_track", lifecycle)),
        messages::ADVANCE_CHANNEL => Some(("advance_channel", lifecycle)),
        messages::SET_AUDIO_GAIN => Some(("set_audio_gain", playback)),
        messages::PLAY => Some(("play", playback)),
        messages::PAUSE => Some(("pause", playback)),
        messages::FLUSH => Some(("flush", playback)),
        messages::DRAIN => Some(("drain", playback)),
        _ => None,
    }
}

/// Write whatever observations have accumulated, oldest first.
fn drain_observations(session: &Arc<SessionRuntime>, egress: &Egress) {
    loop {
        let next = lock(&session.observations).drain_next();
        let Some(observation) = next else {
            return;
        };
        let Ok(body) = Envelope::new(0, observation.payload).encode() else {
            continue;
        };
        if !egress.send(observation.record_type, observation.object_id, body) {
            return;
        }
    }
}

/// Queue one surface observation, core §10.
fn observe_surface(session: &Arc<SessionRuntime>, status: &SurfaceStatus, changed: u64) {
    lock(&session.observations).push(
        observation::class::SURFACE,
        ObservationKey {
            record_type: messages::SURFACE_CHANGED,
            context_id: status.identity.context.context_id,
            object_id: status.identity.surface_id,
        },
        vec![
            (0, Value::Unsigned(status.identity.context.context_id)),
            (1, Value::Unsigned(status.identity.surface_id)),
            (2, Value::Unsigned(status.revision.get())),
            (3, Value::Unsigned(status.generation.get())),
            (4, Value::Unsigned(changed)),
        ],
    );
}

/// Queue one track observation, core §10.
fn observe_track(session: &Arc<SessionRuntime>, status: &TrackStatus, changed: u64) {
    lock(&session.observations).push(
        observation::class::TRACK,
        ObservationKey {
            record_type: messages::TRACK_CHANGED,
            context_id: status.identity.surface.context.context_id,
            object_id: status.identity.track_id,
        },
        vec![
            (0, Value::Unsigned(status.identity.surface.context.context_id)),
            (1, Value::Unsigned(status.identity.surface.surface_id)),
            (2, Value::Unsigned(status.identity.track_id)),
            (3, Value::Unsigned(status.state.revision.get())),
            (4, Value::Unsigned(status.state.channel_generation.get())),
            (5, Value::Unsigned(changed)),
        ],
    );
}

/// Queue one scene observation, core §10.
fn observe_scene(session: &Arc<SessionRuntime>, revision: u64, reason: u64) {
    lock(&session.observations).push(
        observation::class::SCENE,
        ObservationKey { record_type: messages::SCENE_CHANGED, context_id: 0, object_id: 0 },
        vec![(0, Value::Unsigned(revision)), (1, Value::Unsigned(reason))],
    );
}

/// Close the activation-retry window for a leased session.
fn admit_post_hello(shared: &Arc<ServiceShared>, session: &Arc<SessionRuntime>) {
    let Some(key) = session.lease else {
        return;
    };
    if let Some(lease) = lock(&shared.registry).leases.get_mut(&key) {
        let _ = lease.machine.admit_post_hello();
    }
}

/// Suspend a leased child's state instead of destroying it, returning whether it took.
///
/// Security §7.1, in order: the lease and logical session become `SUSPENDED`, revisions advance,
/// input is revoked and held state released, transports close, media ingress and decoder state are
/// discarded, object metadata and scene nodes are retained, every track channel is marked detached
/// and needing a new generation, the grace deadline starts, and the reservation stays charged.
fn suspend_lease(
    shared: &Arc<ServiceShared>,
    session: &Arc<SessionRuntime>,
    key: LeaseKey,
) -> bool {
    let now = Instant::now();
    let (suspended, payload) = {
        let mut registry = lock(&shared.registry);
        let Some(lease) = registry.leases.get_mut(&key) else {
            return false;
        };
        if !lease.suspends_on_unclean_loss() {
            return false;
        }
        // The resume proof is checked against this generation's resume key, so it has to outlive
        // the connection that derived it.
        if !lease.suspend(Secret32::new(*session.resume_key.expose()), now) {
            return false;
        }
        (true, lease.changed_payload(key.1, key.2, reason::UNCLEAN_LOSS, now))
    };
    if !suspended {
        return false;
    }
    // Input is not implemented yet; when it is, its release belongs here, before anything else.
    shared.scene.suspend_session(session.identity);
    stop_session_audio(shared, session.identity);
    let issuer = lock(&shared.registry).sessions.get(&key.0.session_id).cloned();
    if let Some(issuer) = issuer {
        if let Ok(body) = Envelope::new(0, payload).encode() {
            // The issuer is a different session with a different reader. Writing its socket from
            // this one's thread makes one producer's backlog another producer's stall.
            issuer.post_control(messages::SESSION_LEASE_CHANGED, key.2, body);
        }
        issuer.wake_actor();
    }
    true
}

/// Silence and drop every audio output a session owns.
fn stop_session_audio(shared: &Arc<ServiceShared>, session: SessionIdentity) {
    let removed = {
        let mut outputs = lock(&shared.audio_outputs);
        let keys = outputs
            .keys()
            .filter(|identity| identity.surface.context.session == session)
            .copied()
            .collect::<Vec<_>>();
        keys.into_iter().filter_map(|identity| outputs.remove(&identity)).collect::<Vec<_>>()
    };
    for output in removed {
        output.stop();
    }
}

/// Drop leases whose activation deadline passed, releasing their reserved capacity.
fn expire_leases(shared: &Arc<ServiceShared>, issuer: &Arc<SessionRuntime>) {
    let now = Instant::now();
    let expired = {
        let registry = lock(&shared.registry);
        registry
            .leases
            .expired(now)
            .into_iter()
            .filter(|key| key.0 == issuer.identity)
            .collect::<Vec<_>>()
    };
    for key in expired {
        let suspended = lock(&shared.registry)
            .leases
            .get_mut(&key)
            .is_some_and(|lease| lease.grace_deadline.is_some());
        let cause = if suspended { reason::GRACE_EXPIRY } else { reason::ACTIVATION_EXPIRY };
        revoke_lease(shared, issuer, key, cause);
    }
}

/// Revoke one lease and tear down whatever it delegated, returning its final state.
///
/// Security §4.3: revocation is synchronous, closes the child's transports, releases the
/// subtree's reservations, and touches nothing outside it.
fn revoke_lease(
    shared: &Arc<ServiceShared>,
    issuer: &Arc<SessionRuntime>,
    key: LeaseKey,
    reason: u64,
) -> Option<()> {
    let mut registry = lock(&shared.registry);
    let mut lease = registry.leases.remove(&key)?;
    let _ = lease.machine.revoke();
    let child = lease.child.take();
    let payload = lease.changed_payload(key.1, key.2, reason, Instant::now());
    let child_runtime = child.and_then(|child| registry.remove_session(child.session_id));
    drop(registry);

    // Release the capacity the lease held back from its owning context.
    if let Ok(mut contexts) = issuer.contexts.lock()
        && let Some(context) = contexts.get_mut(&key.1)
    {
        let _ = context.release_child(&lease.contract);
    }
    if let Some(child) = child {
        shared.scene.remove_session(child);
    }
    drop(child_runtime);

    if let Ok(body) = Envelope::new(0, payload).encode() {
        issuer.post_control(messages::SESSION_LEASE_CHANGED, key.2, body);
    }
    shared.request_frame_wake();
    Some(())
}

fn finish_session(shared: &Arc<ServiceShared>, session: &Arc<SessionRuntime>, clean: bool) {
    // Parent cleanup: a session's leases and their children go with it (security §4.3).
    let issued = lock(&shared.registry).leases.issued_by(session.identity);
    for key in issued {
        revoke_lease(shared, session, key, reason::PARENT_CLEANUP);
    }
    // A leased child closing releases its own lease back to the issuer, unless an unclean loss
    // under cleanup policy one suspends it instead (security §7.1).
    if let Some(key) = session.lease {
        let issuer = lock(&shared.registry).sessions.get(&key.0.session_id).cloned();
        if let Some(issuer) = issuer {
            if !clean && suspend_lease(shared, session, key) {
                lock(&shared.registry).remove_session(session.identity.session_id);
                shared.request_frame_wake();
                return;
            }
            let reason = if clean { reason::CLEAN_CLOSE } else { reason::UNCLEAN_LOSS };
            revoke_lease(shared, &issuer, key, reason);
        }
    }
    lock(&shared.file_drops).remove_session(session.identity);
    lock(&shared.registry).remove_session(session.identity.session_id);
    let removed_audio = {
        let mut outputs = lock(&shared.audio_outputs);
        let keys = outputs
            .keys()
            .filter(|identity| identity.surface.context.session == session.identity)
            .copied()
            .collect::<Vec<_>>();
        keys.into_iter().filter_map(|identity| outputs.remove(&identity)).collect::<Vec<_>>()
    };
    for output in removed_audio {
        output.stop();
    }
    if clean {
        shared.scene.detach_session(session.identity);
    } else {
        shared.scene.remove_session(session.identity);
    }
    shared.request_frame_wake();
}

/// Who a control connection authenticated as.
enum Principal {
    /// The window's own root secret.
    Root,
    /// A controller-minted lease, which delegates one child logical session to one context.
    Lease { key: LeaseKey, secret: Secret32, attempt_id: [u8; 16] },
    /// A suspended lease resuming within its grace, authenticated by the prior resume key.
    Resume {
        key: LeaseKey,
        prior_resume_key: Secret32,
        attempt_id: [u8; 16],
        session_id: u64,
        resume_generation: u64,
    },
}

/// The session tag inside an encoded `WELCOME`.
///
/// An exact activation or resume retry returns the first attempt's bytes, so the tag the producer
/// will actually see is the one in those bytes and not the candidate this attempt generated.
fn session_tag_of(welcome: &[u8]) -> Option<[u8; messages::SESSION_TAG_BYTES]> {
    Welcome::decode(welcome).ok().map(|(_, welcome)| welcome.session_tag)
}

fn fail_authentication(writer: &Arc<Writer>, request_id: u64, diagnostic: &str) -> io::Error {
    // Uniform response and diagnostic: which of "no such lease" and "wrong secret" happened is
    // not something an unauthenticated caller gets to learn.
    if let Ok(body) = protocol_error(request_id, messages::ERROR_AUTH_FAILED, true, diagnostic) {
        let _ = writer.write_record(messages::ERROR, 0, &body);
    }
    io::Error::new(ErrorKind::PermissionDenied, diagnostic.to_owned())
}

fn establish_root_session(
    shared: &Arc<ServiceShared>,
    writer: Arc<Writer>,
    egress: Arc<Egress>,
    preface: &[u8; 16],
    hello: &Hello,
    request_id: u64,
) -> io::Result<Arc<SessionRuntime>> {
    // Root authentication proves possession of the window's secret; a lease activation proves
    // possession of a secret the controller minted and the presenter only ever saw hashed.
    let principal = match &hello.authentication {
        HelloAuthentication::Root { proof } => {
            let authless = hello.authless_payload()?;
            if !auth::verify_root_hello_proof(&shared.root_secret, preface, &authless, proof) {
                return Err(fail_authentication(&writer, request_id, "root authentication failed"));
            }
            Principal::Root
        },
        HelloAuthentication::LeaseActivation {
            context_id,
            lease_id,
            activation_secret,
            attempt_id,
            ..
        } => {
            let found = lock(&shared.registry).leases.find_activation(
                *context_id,
                *lease_id,
                activation_secret,
            );
            let Some(key) = found else {
                return Err(fail_authentication(&writer, request_id, "lease activation failed"));
            };
            Principal::Lease {
                key,
                secret: Secret32::new(*activation_secret.expose()),
                attempt_id: *attempt_id,
            }
        },
        HelloAuthentication::Resume {
            context_id,
            lease_id,
            session_id,
            resume_generation,
            attempt_id,
            proof,
        } => {
            let candidate =
                lock(&shared.registry).leases.find_resume(*context_id, *lease_id, *session_id);
            let Some(key) = candidate else {
                return Err(fail_authentication(&writer, request_id, "resume failed"));
            };
            let prior = {
                let mut registry = lock(&shared.registry);
                let lease = registry
                    .leases
                    .get_mut(&key)
                    .ok_or_else(|| io::Error::other("lease disappeared during resume"))?;
                if lease.machine.resume_generation().get() != *resume_generation {
                    return Err(fail_authentication(&writer, request_id, "resume failed"));
                }
                lease.resume_key().map(|key| Secret32::new(*key.expose()))
            };
            let Some(prior) = prior else {
                return Err(fail_authentication(&writer, request_id, "resume failed"));
            };
            // The proof binds the preface, the complete lease and session identity, the exact
            // generation, the attempt, and the whole `HELLO` minus the proof itself.
            let expected = auth::resume_hello_proof(
                prior.expose(),
                preface,
                *lease_id,
                *session_id,
                *resume_generation,
                attempt_id,
                &hello.authless_payload()?,
            );
            if !auth::verify_proof(&expected, proof) {
                return Err(fail_authentication(&writer, request_id, "resume failed"));
            }
            Principal::Resume {
                key,
                prior_resume_key: prior,
                attempt_id: *attempt_id,
                session_id: *session_id,
                resume_generation: *resume_generation,
            }
        },
    };
    let target = shared.scene.target().clone();
    if hello.target_profile != target.profile_name() {
        return Err(send_fatal(
            &writer,
            request_id,
            messages::ERROR_UNSUPPORTED_PROFILE,
            "this window presents a different target profile",
        ));
    }
    let supported = target.supported_profiles();
    if hello.required_profiles.iter().any(|profile| !supported.contains(&profile.as_str())) {
        return Err(send_fatal(
            &writer,
            request_id,
            messages::ERROR_UNSUPPORTED_PROFILE,
            "a required profile is not implemented by this target",
        ));
    }
    let mut accepted = hello.required_profiles.clone();
    accepted.extend(
        hello
            .optional_profiles
            .iter()
            .filter(|profile| supported.contains(&profile.as_str()))
            .cloned(),
    );
    accepted.sort();
    accepted.dedup();
    registry::validate_profile_set(accepted.iter().map(String::as_str))
        .map_err(io::Error::other)?;
    let mut registry = lock(&shared.registry);
    if registry.sessions.len() >= MAX_SESSIONS {
        return Err(send_fatal(
            &writer,
            request_id,
            messages::ERROR_LIMIT_EXCEEDED,
            "session capacity is exhausted",
        ));
    }
    let session_id = shared.next_session.fetch_add(1, Ordering::AcqRel);
    if session_id == 0 {
        return Err(io::Error::other("session ID exhausted"));
    }
    // A resume keeps the suspended logical session, so the candidate `WELCOME` has to carry that
    // ID rather than a freshly allocated one — the machine caches these exact bytes.
    let session_id = match &principal {
        Principal::Resume { session_id, .. } => *session_id,
        _ => session_id,
    };
    let identity = SessionIdentity::new(shared.presenter, session_id).map_err(io::Error::other)?;
    let root_context = identity.context(1).map_err(io::Error::other)?;
    let mut server_nonce = [0_u8; auth::NONCE_BYTES];
    let mut session_tag = [0_u8; messages::SESSION_TAG_BYTES];
    getrandom::fill(&mut server_nonce).map_err(io::Error::other)?;
    getrandom::fill(&mut session_tag).map_err(io::Error::other)?;

    // A leased child runs as the lease's context, with the capacity that lease reserved.
    let web_carrier = accepted.iter().any(|profile| profile == registry::WEB_CARRIER);
    let (session_secret, mut contract, classes, authentication_kind) = match &principal {
        Principal::Root => (
            Secret32::new(*shared.root_secret.expose()),
            presenter_contract(),
            target.root_operation_classes(),
            messages::AUTHENTICATION_ROOT,
        ),
        Principal::Lease { key, secret, .. } => {
            let (contract, classes) = {
                let lease = registry
                    .leases
                    .get_mut(key)
                    .ok_or_else(|| io::Error::other("lease disappeared during activation"))?;
                if !lease
                    .definition
                    .permitted_profiles
                    .iter()
                    .all(|permitted| supported.contains(&permitted.as_str()))
                {
                    return Err(fail_authentication(
                        &writer,
                        request_id,
                        "lease permits a profile this target does not implement",
                    ));
                }
                (lease.contract.clone(), lease.classes)
            };
            (
                Secret32::new(*secret.expose()),
                contract,
                classes,
                messages::AUTHENTICATION_LEASE_ACTIVATION,
            )
        },
        Principal::Resume { key, prior_resume_key, .. } => {
            // Security §7.2: the next generation's keys derive from the prior resume key, so the
            // resumed session shares no key material with the one that was lost.
            let lease = registry
                .leases
                .get_mut(key)
                .ok_or_else(|| io::Error::other("lease disappeared during resume"))?;
            (
                Secret32::new(*prior_resume_key.expose()),
                lease.contract.clone(),
                lease.classes,
                messages::AUTHENTICATION_RESUME,
            )
        },
    };
    if web_carrier {
        clamp_contract_for_web(&mut contract);
    }

    let prk =
        auth::extract_handshake_prk(&session_secret, &hello.client_nonce, &server_nonce, &[0; 32]);
    let (keys, anchor_key) = auth::derive_session_keys(&prk, session_id, 0, &session_tag);

    let observation_capacity = contract.get(Resource::ObservationQueueEntries);
    let mut welcome = Welcome {
        session_id,
        session_tag,
        root_context_id: root_context.context_id,
        target_generation: target.generation(),
        target_profile: target.profile_name().into(),
        target_descriptor: target.descriptor(),
        accepted_profiles: accepted,
        maximum_control_body: hello.maximum_control_body.min(if web_carrier {
            vivid_protocol::web::MAX_CONTROL_RECORD_BODY
        } else {
            vivid_protocol::CONTROL_MAX_RECORD_BODY
        }),
        server_nonce,
        authentication: WelcomeAuthentication {
            kind: authentication_kind,
            confirmation: [0; 32],
            // A leased child is `ACTIVE` by the time it reads its own `WELCOME`; a root session
            // has no lease state at all.
            lease_state: match &principal {
                Principal::Root => 0,
                Principal::Lease { .. } | Principal::Resume { .. } => {
                    vivid_protocol::lease::LeaseState::Active as u64
                },
            },
            activation_attempt_status: 0,
        },
        session_revision: 1,
        scene_revision: 0,
        resource_contract: contract.clone(),
        establishment_state: match &principal {
            Principal::Resume { .. } => 1,
            _ => 0,
        },
        resume_generation: match &principal {
            // The generation the resume advanced to, which the producer echoes on its next one.
            Principal::Resume { resume_generation, .. } => resume_generation.saturating_add(1),
            _ => 0,
        },
        extensions: vec![],
    };
    welcome.confirm(&prk)?;
    let welcome_body = welcome.encode(request_id)?;

    // Security §6.4: an exact retry of a lost `WELCOME` returns the same session ID, server
    // nonce, and bytes, and concurrent attempts have exactly one winner.
    let mut resumed_announcements: Vec<(u64, u64, messages::PayloadMap)> = Vec::new();
    let (session_id, session_tag, keys, anchor_key, welcome_body) = match &principal {
        Principal::Root => (session_id, session_tag, keys, anchor_key, welcome_body),
        Principal::Lease { key, attempt_id, .. } => {
            let fingerprint =
                profile_fingerprint(target.profile_name(), &welcome.accepted_profiles);
            let lease = registry
                .leases
                .get_mut(key)
                .ok_or_else(|| io::Error::other("lease disappeared during activation"))?;
            let decision = lease
                .machine
                .begin_activation(
                    *attempt_id,
                    hello.client_nonce,
                    &hello.authless_payload()?,
                    fingerprint,
                    session_id,
                    server_nonce,
                    welcome_body,
                )
                .map_err(|_| {
                    io::Error::new(ErrorKind::PermissionDenied, "lease activation was refused")
                })?;
            let (decided_session, decided_nonce, decided_welcome) = match decision {
                AttemptDecision::Fresh { session_id, server_nonce, welcome }
                | AttemptDecision::ExactReplay { session_id, server_nonce, welcome } => {
                    (session_id, server_nonce, welcome)
                },
            };
            lease.machine.commit_welcome().ok();
            lease.child = Some(
                SessionIdentity::new(shared.presenter, decided_session)
                    .map_err(io::Error::other)?,
            );
            // A replay reuses the original nonce, so the derived keys match the first attempt's.
            let prk = auth::extract_handshake_prk(
                &session_secret,
                &hello.client_nonce,
                &decided_nonce,
                &[0; 32],
            );
            let decided_tag = session_tag_of(&decided_welcome).unwrap_or(session_tag);
            let (keys, anchor_key) =
                auth::derive_session_keys(&prk, decided_session, 0, &decided_tag);
            (decided_session, decided_tag, keys, anchor_key, decided_welcome)
        },
        Principal::Resume { key, attempt_id, session_id: suspended, resume_generation, .. } => {
            let fingerprint =
                profile_fingerprint(target.profile_name(), &welcome.accepted_profiles);
            let lease = registry
                .leases
                .get_mut(key)
                .ok_or_else(|| io::Error::other("lease disappeared during resume"))?;
            // The logical session ID survives the loss; only its keys and generation change.
            let decision = lease
                .machine
                .begin_resume(
                    vivid_protocol::revision::ResumeGeneration::new(*resume_generation),
                    *attempt_id,
                    hello.client_nonce,
                    &hello.authless_payload()?,
                    fingerprint,
                    *suspended,
                    server_nonce,
                    welcome_body,
                )
                .map_err(|_| io::Error::new(ErrorKind::PermissionDenied, "resume was refused"))?;
            let (decided_session, decided_nonce, decided_welcome) = match decision {
                AttemptDecision::Fresh { session_id, server_nonce, welcome }
                | AttemptDecision::ExactReplay { session_id, server_nonce, welcome } => {
                    (session_id, server_nonce, welcome)
                },
            };
            lease.machine.commit_welcome().ok();
            // The prior resume key is erased once the new confirmation is committed.
            lease.resumed();
            let generation = lease.machine.resume_generation().get();
            let announcement = lease.changed_payload(key.1, key.2, reason::RESUMED, Instant::now());
            resumed_announcements.push((key.0.session_id, key.2, announcement));
            let prk = auth::extract_handshake_prk(
                &session_secret,
                &hello.client_nonce,
                &decided_nonce,
                &[0; 32],
            );
            let decided_tag = session_tag_of(&decided_welcome).unwrap_or(session_tag);
            let (keys, anchor_key) =
                auth::derive_session_keys(&prk, decided_session, generation, &decided_tag);
            (decided_session, decided_tag, keys, anchor_key, decided_welcome)
        },
    };
    let identity = SessionIdentity::new(shared.presenter, session_id).map_err(io::Error::other)?;
    let root_context = identity.context(root_context.context_id).map_err(io::Error::other)?;
    writer.write_record(messages::WELCOME, 0, &welcome_body)?;
    let runtime = Arc::new(SessionRuntime {
        identity,
        root_context,
        session_tag,
        channel_key: Secret32::new(*keys.channel_key()),
        anchor_key,
        accepted_profiles: welcome.accepted_profiles.clone(),
        egress: Mutex::new(Some(egress)),
        actor_ingress: Mutex::new(None),
        contexts: Mutex::new(HashMap::from([(
            root_context.context_id,
            ContextState::root(identity, root_context.context_id, classes, contract)
                .map_err(io::Error::other)?,
        )])),
        seen_anchors: Mutex::new(HashSet::new()),
        lease: match &principal {
            Principal::Root => None,
            Principal::Lease { key, .. } | Principal::Resume { key, .. } => Some(*key),
        },
        resume_key: Secret32::new(*keys.resume_key()),
        lane: Mutex::new(None),
        lane_writer: Mutex::new(None),
        lane_egress: Mutex::new(None),
        grant: Mutex::new(InputGrant::new()),
        observations: Mutex::new(ObservationQueue::new(observation_capacity)),
        markers: Mutex::new(MarkerAdmission::new(Instant::now())),
    });
    // Suspension retained this session's surfaces, tracks, and nodes, so a resume re-attaches to
    // them rather than registering a second time (security §7.1).
    if !shared.scene.is_registered(identity) {
        shared
            .scene
            .register_session(identity, TargetGeneration::new(target.generation()))
            .map_err(io::Error::other)?;
    }
    registry.insert_session(runtime.clone());
    for (issuer_session, lease_id, payload) in resumed_announcements {
        if let Some(issuer) = registry.sessions.get(&issuer_session)
            && let Ok(body) = Envelope::new(0, payload).encode()
        {
            issuer.post_control(messages::SESSION_LEASE_CHANGED, lease_id, body);
        }
    }
    Ok(runtime)
}

fn dispatch_control(
    shared: &Arc<ServiceShared>,
    session: &Arc<SessionRuntime>,
    record: &Record,
    pending: &mut PendingSet,
    cancelled: &mut HashSet<u64>,
) -> Result<Option<(u16, u64, Vec<u8>)>, ControlError> {
    if record.flags & !RECORD_OPTIONAL != 0 {
        return Err(ControlError::bad_message("unknown record flags"));
    }
    let envelope = messages::decode_control(&record.body)
        .map_err(|_| ControlError::bad_message("invalid strict control envelope"))?;
    envelope
        .validate_request()
        .map_err(|_| ControlError::bad_message("control request ID must be nonzero"))?;
    let request_id = envelope.request_id;
    let value = Value::Map(envelope.payload.clone());
    let reply = match record.record_type {
        messages::PING => (messages::PONG, 0, Envelope::new(request_id, envelope.payload).encode()),
        messages::GOODBYE => (messages::OK, 0, Ok(messages::ok(request_id))),
        messages::QUERY_SESSION => {
            // Core §10's schema, which is what a producer reconciles against after resume. The
            // previous payload described the session's identity rather than its revisions, so it
            // could not serve that purpose at all.
            let summaries =
                shared.scene.session_revision_summary(session.identity, MAX_STATUS_ENTRIES);
            let scene_revision = shared.scene.scene_revision(session.identity);
            (
                messages::SESSION_STATUS,
                0,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Unsigned(summaries.session_revision)),
                        (1, Value::Unsigned(scene_revision)),
                        (2, Value::Unsigned(shared.scene.target().generation())),
                        // Establishment state: active. Vivido does not suspend a session yet, so
                        // it is never `suspended` here.
                        (3, Value::Unsigned(1)),
                        (4, Value::Unsigned(session.lease.map(|_| 0).unwrap_or(0))),
                        // Input is disabled until `desktop-input-v1` lands.
                        (5, Value::Bool(true)),
                        (6, Value::Array(summaries.entries)),
                        (8, shared.scene.resource_usage(session.identity)),
                    ],
                )
                .encode(),
            )
        },
        messages::SET_FILE_DROP_BINDING => {
            if !session.supports(registry::FILE_DROP) {
                return Err(ControlError::unsupported("file-drop-v1 was not negotiated"));
            }
            let binding =
                vivid_protocol::file_drop::FileDropBinding::decode(record.object_id, &value)
                    .map_err(|_| ControlError::bad_message("invalid file-drop binding"))?;
            require_context_operation(session, binding.context_id, OP_RECEIVE_FILE_DROP)?;
            if !binding.disabled() && binding.surface_id != 0 {
                let identity = surface_identity(session, binding.context_id, binding.surface_id)?;
                let status = shared
                    .scene
                    .surface_status(identity)
                    .ok_or_else(|| ControlError::not_found("file-drop surface is absent"))?;
                if status.generation != binding.surface_generation {
                    return Err(ControlError::precondition(
                        "file-drop surface generation is stale",
                    ));
                }
            }
            let grant = lock(&shared.file_drops)
                .set_binding(session.identity, binding, true)
                .map_err(ControlError::state)?;
            (
                messages::FILE_DROP_BOUND,
                record.object_id,
                Envelope::new(request_id, grant.payload()).encode(),
            )
        },
        messages::ACCEPT_FILE_DROP => {
            if !session.supports(registry::FILE_DROP) {
                return Err(ControlError::unsupported("file-drop-v1 was not negotiated"));
            }
            let acceptance =
                vivid_protocol::file_drop::AcceptFileDrop::decode(record.object_id, &value)
                    .map_err(|_| ControlError::bad_message("invalid file-drop acceptance"))?;
            require_context_operation(
                session,
                acceptance.binding.context_id,
                OP_RECEIVE_FILE_DROP,
            )?;
            let accepted = lock(&shared.file_drops)
                .accept(session.identity, acceptance)
                .map_err(ControlError::state)?;
            (
                messages::FILE_DROP_ACCEPTED,
                record.object_id,
                Envelope::new(request_id, accepted.payload()).encode(),
            )
        },
        messages::CANCEL_FILE_DROP => {
            let cancellation = vivid_protocol::file_drop::CancelFileDrop::decode(
                "CANCEL_FILE_DROP",
                record.object_id,
                &value,
            )
            .map_err(|_| ControlError::bad_message("invalid file-drop cancellation"))?;
            require_context_operation(
                session,
                cancellation.binding.context_id,
                OP_RECEIVE_FILE_DROP,
            )?;
            lock(&shared.file_drops)
                .cancel(session.identity, cancellation)
                .map_err(ControlError::state)?;
            (
                messages::FILE_DROP_CANCELLED,
                record.object_id,
                Envelope::new(
                    request_id,
                    cancellation
                        .payload()
                        .map_err(|_| ControlError::bad_message("invalid file-drop cancellation"))?,
                )
                .encode(),
            )
        },
        messages::ADVANCE_FILE_TRANSFER => {
            let advance =
                vivid_protocol::file_drop::AdvanceFileTransfer::decode(record.object_id, &value)
                    .map_err(|_| ControlError::bad_message("invalid file-transfer advance"))?;
            require_context_operation(session, advance.context_id, OP_RECEIVE_FILE_DROP)?;
            let advanced = lock(&shared.file_drops)
                .advance(session.identity, advance)
                .map_err(ControlError::state)?;
            (
                messages::FILE_TRANSFER_ADVANCED,
                record.object_id,
                Envelope::new(
                    request_id,
                    advanced.payload().map_err(|_| {
                        ControlError::bad_message("invalid file-transfer advance reply")
                    })?,
                )
                .encode(),
            )
        },
        messages::QUERY_FILE_DROP => {
            let query = vivid_protocol::file_drop::QueryFileDrop::decode(record.object_id, &value)
                .map_err(|_| ControlError::bad_message("invalid file-drop query"))?;
            let status = lock(&shared.file_drops)
                .status(session.identity, query)
                .map_err(ControlError::not_found)?;
            (
                messages::FILE_DROP_STATUS,
                record.object_id,
                Envelope::new(
                    request_id,
                    status
                        .payload()
                        .map_err(|_| ControlError::bad_message("invalid file-drop status reply"))?,
                )
                .encode(),
            )
        },
        messages::CREATE_CONTEXT => {
            let definition = ContextDefinition::decode(record.object_id, &value)
                .map_err(|_| ControlError::bad_message("invalid context schema"))?;
            let mut contexts = lock(&session.contexts);
            if contexts.contains_key(&definition.context_id) {
                return Err(ControlError::bad_state("context ID is already live"));
            }
            let parent = contexts
                .get_mut(&definition.parent_context_id)
                .ok_or_else(|| ControlError::not_found("context parent is absent"))?;
            let (classes, contract) = parent
                .reserve_child(&definition, &presenter_contract())
                .map_err(|_| ControlError::limit("context capacity is exhausted"))?;
            let mut child = ContextState::root(
                session.identity,
                definition.context_id,
                classes,
                contract.clone(),
            )
            .map_err(|_| ControlError::bad_message("invalid effective context"))?;
            child.parent_context_id = Some(definition.parent_context_id);
            contexts.insert(definition.context_id, child);
            (
                messages::CONTEXT_READY,
                definition.context_id,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Unsigned(definition.context_id)),
                        (1, Value::Unsigned(classes)),
                        (2, contract.to_value()),
                        (3, Value::Unsigned(definition.lifetime_us)),
                        (4, Value::Unsigned(1)),
                    ],
                )
                .encode(),
            )
        },
        messages::CREATE_SESSION_LEASE => {
            let definition = SessionLeaseDefinition::decode(&value)
                .map_err(|_| ControlError::bad_message("invalid session lease schema"))?;
            if definition.lease_id != record.object_id {
                return Err(ControlError::bad_message(
                    "lease object ID does not match its payload",
                ));
            }
            require_context_operation(session, definition.context_id, OP_DELEGATE)?;
            // Permitted profiles form a closed set, and this target has to implement all of them.
            let supported = shared.scene.target().supported_profiles();
            if definition
                .permitted_profiles
                .iter()
                .any(|profile| !supported.contains(&profile.as_str()))
            {
                return Err(ControlError::unsupported(
                    "lease permits a profile this target does not implement",
                ));
            }
            let key = (session.identity, definition.context_id, definition.lease_id);
            let mut registry = lock(&shared.registry);
            if registry.leases.contains(&key) {
                return Err(ControlError::duplicate("lease identity is already live"));
            }
            if registry.leases.len() >= MAX_LEASES {
                return Err(ControlError::limit("lease capacity is exhausted"));
            }
            // Capacity delegated to a live child is reserved, not merely checked, so a sibling
            // cannot be handed the same capacity (security §4.3).
            let (classes, contract) = {
                let mut contexts = lock(&session.contexts);
                let parent = contexts
                    .get_mut(&definition.context_id)
                    .ok_or_else(|| ControlError::not_found("lease context is absent"))?;
                let request = ContextDefinition {
                    context_id: definition.lease_id,
                    parent_context_id: definition.context_id,
                    operation_classes: parent.operation_classes,
                    label: String::new(),
                    lifetime_us: 0,
                    requested_contract: definition.requested_contract.clone(),
                };
                parent
                    .reserve_child(&request, &presenter_contract())
                    .map_err(|_| ControlError::limit("context capacity is exhausted"))?
            };
            let lease = Lease::new(definition.clone(), contract, classes, Instant::now());
            let payload = lease.ready_payload(definition.context_id, definition.lease_id);
            registry.leases.insert(key, lease);
            drop(registry);
            (
                messages::SESSION_LEASE_READY,
                definition.lease_id,
                Envelope::new(request_id, payload).encode(),
            )
        },
        messages::REVOKE_SESSION_LEASE => {
            let map = StrictMap::new("REVOKE_SESSION_LEASE", &value, &[0, 1])
                .map_err(|_| ControlError::bad_message("invalid lease revoke"))?;
            let context_id =
                map.required_u64(0).map_err(|_| ControlError::bad_message("missing context ID"))?;
            let lease_id =
                map.required_u64(1).map_err(|_| ControlError::bad_message("missing lease ID"))?;
            require_context_operation(session, context_id, OP_DELEGATE)?;
            let key = (session.identity, context_id, lease_id);
            revoke_lease(shared, session, key, reason::EXPLICIT_REVOKE)
                .ok_or_else(|| ControlError::not_found("lease does not exist"))?;
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::REVOKE_CONTEXT => {
            let map = StrictMap::new("REVOKE_CONTEXT", &value, &[0, 1])
                .map_err(|_| ControlError::bad_message("invalid context revoke"))?;
            let context_id =
                map.required_u64(0).map_err(|_| ControlError::bad_message("missing context ID"))?;
            if context_id == session.root_context.context_id {
                return Err(ControlError::bad_state("context cannot be revoked"));
            }
            let mut contexts = lock(&session.contexts);
            if !contexts.contains_key(&context_id) {
                return Err(ControlError::not_found("context does not exist"));
            }
            let mut removed = HashSet::from([context_id]);
            loop {
                let before = removed.len();
                for state in contexts.values() {
                    if state.parent_context_id.is_some_and(|parent| removed.contains(&parent)) {
                        removed.insert(state.context_id());
                    }
                }
                if removed.len() == before {
                    break;
                }
            }
            let mut ordered = removed.iter().copied().collect::<Vec<_>>();
            ordered.sort_by_key(|candidate| {
                let mut depth = 0;
                let mut current = *candidate;
                while let Some(parent) =
                    contexts.get(&current).and_then(|state| state.parent_context_id)
                {
                    depth += 1;
                    current = parent;
                }
                std::cmp::Reverse(depth)
            });
            for removed_id in ordered {
                if let Some(mut removed_state) = contexts.remove(&removed_id) {
                    let _ = removed_state.revoke();
                    if let Some(parent_id) = removed_state.parent_context_id
                        && let Some(parent) = contexts.get_mut(&parent_id)
                    {
                        let _ = parent.release_child(&removed_state.contract);
                    }
                }
            }
            drop(contexts);
            shared.scene.remove_contexts(session.identity, &removed);
            lock(&shared.file_drops).remove_contexts(session.identity, &removed);
            (messages::OK, context_id, Ok(messages::ok(request_id)))
        },
        messages::CREATE_SURFACE => {
            let definition = SurfaceDefinition::decode_create(record.object_id, &value)
                .map_err(|_| ControlError::bad_message("invalid surface definition"))?;
            require_context_operation(session, definition.context_id, OP_SURFACE_TRACK_MEDIA)?;
            // Which semantic profiles are presentable is the target's decision, not the session's.
            if !matches!(
                definition.semantic_profile.as_str(),
                registry::GENERIC_CONTENT | registry::TERMINAL_CONTENT | registry::DESKTOP_CONTENT
            ) {
                return Err(ControlError::unsupported(
                    "Vivido presents generic, terminal, and desktop content surfaces",
                ));
            }
            let identity = surface_identity(session, definition.context_id, definition.surface_id)?;
            let status =
                shared.scene.create_surface(identity, definition).map_err(ControlError::state)?;
            observe_surface(session, &status, SURFACE_CHANGED_LIFECYCLE);
            (
                messages::SURFACE_READY,
                record.object_id,
                Envelope::new(request_id, surface_ready_payload(&status)).encode(),
            )
        },
        messages::UPDATE_SURFACE => {
            let map =
                StrictMap::new("UPDATE_SURFACE", &value, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11])
                    .map_err(|_| ControlError::bad_message("invalid surface update"))?;
            let context_id =
                map.required_u64(0).map_err(|_| ControlError::bad_message("context"))?;
            let surface_id =
                map.required_u64(1).map_err(|_| ControlError::bad_message("surface"))?;
            let identity = surface_identity(session, context_id, surface_id)?;
            let current = shared
                .scene
                .surface_status(identity)
                .ok_or_else(|| ControlError::not_found("surface does not exist"))?;
            let replacement = SurfaceDefinition {
                context_id,
                surface_id,
                semantic_profile: current.definition.semantic_profile.clone(),
                coordinate_model: current.definition.coordinate_model,
                logical_width: map
                    .required_u64(4)
                    .map_err(|_| ControlError::bad_message("width"))?,
                logical_height: map
                    .required_u64(5)
                    .map_err(|_| ControlError::bad_message("height"))?,
                scale_numerator: map
                    .required_u64(6)
                    .map_err(|_| ControlError::bad_message("scale"))?,
                scale_denominator: map
                    .required_u64(7)
                    .map_err(|_| ControlError::bad_message("scale"))?,
                rotation: u16::try_from(
                    map.required_u64(8).map_err(|_| ControlError::bad_message("rotation"))?,
                )
                .map_err(|_| ControlError::bad_message("rotation"))?,
                descriptor: SurfaceDescriptor::from_value(
                    map.required(9).map_err(|_| ControlError::bad_message("descriptor"))?,
                )
                .map_err(|_| ControlError::bad_message("descriptor"))?,
                policy: map.required_u64(10).map_err(|_| ControlError::bad_message("policy"))?,
                profile_parameters: map
                    .required_map(11)
                    .map_err(|_| ControlError::bad_message("profile parameters"))?
                    .to_vec(),
            };
            let status = shared
                .scene
                .update_surface(
                    identity,
                    SurfaceRevision::new(
                        map.required_u64(2).map_err(|_| ControlError::bad_message("revision"))?,
                    ),
                    SurfaceGeneration::new(
                        map.required_u64(3).map_err(|_| ControlError::bad_message("generation"))?,
                    ),
                    replacement,
                )
                .map_err(ControlError::state)?;
            if status.generation != current.generation {
                lock(&shared.file_drops).remove_surface(
                    session.identity,
                    identity.context.context_id,
                    identity.surface_id,
                );
            }
            observe_surface(session, &status, SURFACE_CHANGED_GEOMETRY);
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::DESTROY_SURFACE => {
            let identity = payload_surface_identity(session, &value)?;
            if let Some(status) = shared.scene.surface_status(identity) {
                observe_surface(session, &status, SURFACE_CHANGED_LIFECYCLE);
            }
            shared.scene.destroy_surface(identity).map_err(ControlError::state)?;
            lock(&shared.file_drops).remove_surface(
                session.identity,
                identity.context.context_id,
                identity.surface_id,
            );
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::QUERY_SURFACE => {
            let identity = payload_surface_identity(session, &value)?;
            let status = shared
                .scene
                .surface_status(identity)
                .ok_or_else(|| ControlError::not_found("surface does not exist"))?;
            (
                messages::SURFACE_STATUS,
                record.object_id,
                Envelope::new(request_id, surface_status_payload(&status)).encode(),
            )
        },
        messages::PROBE_TRACK_CONFIG => {
            let configuration = TrackConfiguration::decode(0, &value, true)
                .map_err(|_| ControlError::bad_message("invalid track probe"))?;
            let supported = supports_track(&configuration);
            (
                messages::TRACK_SUPPORT,
                0,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Bool(supported)),
                        (
                            1,
                            Value::Text(if supported {
                                "vivido-native".into()
                            } else {
                                "unsupported".into()
                            }),
                        ),
                        (2, Value::Unsigned(1)),
                        (3, Value::Map(configuration.payload(true).unwrap_or_default())),
                    ],
                )
                .encode(),
            )
        },
        messages::CREATE_TRACK => {
            let configuration = TrackConfiguration::decode(record.object_id, &value, false)
                .map_err(|_| ControlError::bad_message("invalid track configuration"))?;
            require_context_operation(session, configuration.context_id, OP_SURFACE_TRACK_MEDIA)?;
            let identity = track_identity(
                session,
                configuration.context_id,
                configuration.surface_id,
                configuration.track_id,
            )?;
            if !supports_track(&configuration) {
                return Err(ControlError::unsupported("track configuration is unsupported")
                    .with_track(identity));
            }
            let status = shared
                .scene
                .create_track(identity, configuration)
                .map_err(|message| ControlError::state(message).with_track(identity))?;
            observe_track(session, &status, TRACK_CHANGED_LIFECYCLE);
            shared.trace(
                trace::TraceCategory::Lifecycle,
                "track_created",
                Some(identity),
                serde_json::json!({
                    "operation": "create_track",
                    "request_id": request_id,
                    "control_record_sequence": record.sequence,
                    "record_type": record.record_type,
                    "object_id": record.object_id,
                    "track_revision_before": 0,
                    "track_revision_after": status.state.revision.get(),
                    "channel_generation": status.state.channel_generation.get(),
                    "kind": scene::track_kind_name(&status.configuration),
                    "slot": status.configuration.slot,
                    "mode": status.configuration.mode as u64,
                    "lane": status.configuration.lane as u64,
                }),
            );
            let mut payload = vec![
                (0, Value::Unsigned(identity.surface.context.context_id)),
                (1, Value::Unsigned(identity.surface.surface_id)),
                (2, Value::Unsigned(identity.track_id)),
                (3, Value::Unsigned(status.state.revision.get())),
                (4, Value::Unsigned(status.state.channel_generation.get())),
                (5, Value::Unsigned(CHANNEL_OPEN_DEADLINE_US)),
                (6, Value::Unsigned(u64::from(status.configuration.maximum_record_body))),
                (7, Value::Map(status.configuration.payload(false).unwrap_or_default())),
                (8, Value::Bool(true)),
            ];
            // A producer plans deltas against the granted limit, so a raster ingest path that
            // accepts deltas has to say so here. Omitting key 9 grants zero operations, which
            // reads as "full frames only" and silently retires the delta path for every producer.
            if let KindConfiguration::Raster(raster) = &status.configuration.kind
                && raster.delta_enabled
            {
                payload.push((9, Value::Unsigned(u64::from(raster.maximum_delta_operations))));
            }
            (messages::TRACK_READY, record.object_id, Envelope::new(request_id, payload).encode())
        },
        messages::DESTROY_TRACK => {
            let identity = payload_track_identity(session, &value)?;
            let status = shared.scene.track_status(identity).ok_or_else(|| {
                ControlError::not_found("track does not exist").with_track(identity)
            })?;
            shared
                .scene
                .destroy_track(identity)
                .map_err(|message| ControlError::state(message).with_track(identity))?;
            let audio_output_stopped =
                if let Some(output) = lock(&shared.audio_outputs).remove(&identity) {
                    output.stop();
                    true
                } else {
                    false
                };
            shared.trace(
                trace::TraceCategory::Lifecycle,
                "track_destroyed",
                Some(identity),
                serde_json::json!({
                    "operation": "destroy_track",
                    "request_id": request_id,
                    "control_record_sequence": record.sequence,
                    "record_type": record.record_type,
                    "object_id": record.object_id,
                    "track_revision_before": status.state.revision.get(),
                    "track_revision_after": null,
                    "channel_generation": status.state.channel_generation.get(),
                    "lifecycle": status.lifecycle,
                    "audio_output_stopped": audio_output_stopped,
                }),
            );
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::QUERY_TRACK => {
            let identity = payload_track_identity(session, &value)?;
            let status = shared
                .scene
                .track_status(identity)
                .ok_or_else(|| ControlError::not_found("track does not exist"))?;
            (
                messages::TRACK_STATUS,
                record.object_id,
                Envelope::new(
                    request_id,
                    track_status_payload(&status, session.supports(registry::AUDIO_GAIN)),
                )
                .encode(),
            )
        },
        messages::ADVANCE_CHANNEL => {
            let identity = payload_track_identity(session, &value)?;
            let map =
                StrictMap::new("ADVANCE_CHANNEL", &value, &[0, 1, 2, 3, 4, 5]).map_err(|_| {
                    ControlError::bad_message("invalid channel advance").with_track(identity)
                })?;
            let current = map.required_u64(3).map_err(|_| {
                ControlError::bad_message("current generation").with_track(identity)
            })?;
            let next = map
                .required_u64(4)
                .map_err(|_| ControlError::bad_message("next generation").with_track(identity))?;
            let before = shared.scene.track_status(identity).ok_or_else(|| {
                ControlError::not_found("track does not exist").with_track(identity)
            })?;
            if before.state.channel_generation.get() != current
                || current.checked_add(1) != Some(next)
            {
                return Err(
                    ControlError::state("channel advance is not exact").with_track(identity)
                );
            }
            let status = shared
                .scene
                .advance_channel(identity)
                .map_err(|message| ControlError::state(message).with_track(identity))?;
            let audio_output_stopped =
                if let Some(output) = lock(&shared.audio_outputs).remove(&identity) {
                    output.stop();
                    true
                } else {
                    false
                };
            observe_track(session, &status, TRACK_CHANGED_CHANNEL);
            shared.trace(
                trace::TraceCategory::Lifecycle,
                "channel_advanced",
                Some(identity),
                serde_json::json!({
                    "operation": "advance_channel",
                    "request_id": request_id,
                    "control_record_sequence": record.sequence,
                    "record_type": record.record_type,
                    "object_id": record.object_id,
                    "track_revision_before": before.state.revision.get(),
                    "track_revision_after": status.state.revision.get(),
                    "previous_channel_generation": current,
                    "channel_generation": status.state.channel_generation.get(),
                    "audio_output_stopped": audio_output_stopped,
                }),
            );
            (
                messages::CHANNEL_ADVANCED,
                record.object_id,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Unsigned(identity.surface.context.context_id)),
                        (1, Value::Unsigned(identity.surface.surface_id)),
                        (2, Value::Unsigned(identity.track_id)),
                        (3, Value::Unsigned(status.state.channel_generation.get())),
                        (4, Value::Unsigned(CHANNEL_OPEN_DEADLINE_US)),
                        (5, Value::Unsigned(status.state.revision.get())),
                    ],
                )
                .encode(),
            )
        },
        messages::SET_AUDIO_GAIN => {
            if !session.supports(registry::AUDIO_GAIN) {
                return Err(ControlError::unsupported_profile("audio-gain-v1 was not negotiated"));
            }
            let identity = payload_track_identity(session, &value)?;
            let map = StrictMap::new("SET_AUDIO_GAIN", &value, &[0, 1, 2, 3]).map_err(|_| {
                ControlError::bad_message("invalid audio gain").with_track(identity)
            })?;
            let raw = map
                .required_u64(3)
                .map_err(|_| ControlError::bad_message("audio gain").with_track(identity))?;
            let gain = vivid_protocol::track::AudioGain::new(raw).ok_or_else(|| {
                ControlError::bad_message("audio gain exceeds 200 percent").with_track(identity)
            })?;
            let before = shared.scene.track_status(identity).ok_or_else(|| {
                ControlError::not_found("track does not exist").with_track(identity)
            })?;
            let status = shared
                .scene
                .set_audio_gain(identity, gain)
                .map_err(|message| ControlError::state(message).with_track(identity))?;
            let output_updated = if let Some(output) = lock(&shared.audio_outputs).get(&identity) {
                output.set_gain(gain);
                true
            } else {
                false
            };
            observe_track(session, &status, TRACK_CHANGED_AUDIO_GAIN);
            shared.trace(
                trace::TraceCategory::Playback,
                "audio_gain_changed",
                Some(identity),
                serde_json::json!({
                    "operation": "set_audio_gain",
                    "request_id": request_id,
                    "control_record_sequence": record.sequence,
                    "record_type": record.record_type,
                    "object_id": record.object_id,
                    "track_revision_before": before.state.revision.get(),
                    "track_revision_after": status.state.revision.get(),
                    "channel_generation": status.state.channel_generation.get(),
                    "previous_gain": before.audio_gain.raw(),
                    "gain": status.audio_gain.raw(),
                    "output_updated": output_updated,
                }),
            );
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::ACTIVATE_TRACK => {
            let map = StrictMap::new("ACTIVATE_TRACK", &value, &[0, 1, 2, 3])
                .map_err(|_| ControlError::bad_message("invalid slot activation"))?;
            let context_id =
                map.required_u64(0).map_err(|_| ControlError::bad_message("context"))?;
            let surface_id =
                map.required_u64(1).map_err(|_| ControlError::bad_message("surface"))?;
            let bindings = map
                .required(2)
                .map_err(|_| ControlError::bad_message("bindings"))?
                .as_array()
                .ok_or_else(|| ControlError::bad_message("bindings are not an array"))?
                .iter()
                .map(|value| {
                    let binding = StrictMap::new("slot binding", value, &[0, 1, 2, 3])
                        .map_err(|_| ControlError::bad_message("invalid slot binding"))?;
                    Ok((
                        binding.required_u64(0).map_err(|_| ControlError::bad_message("slot"))?,
                        binding.required_u64(1).map_err(|_| ControlError::bad_message("track"))?,
                        ChannelGeneration::new(
                            binding
                                .required_u64(2)
                                .map_err(|_| ControlError::bad_message("generation"))?,
                        ),
                        binding
                            .required_u64(3)
                            .map_err(|_| ControlError::bad_message("required milestone"))?,
                    ))
                })
                .collect::<Result<Vec<_>, ControlError>>()?;
            let identity = surface_identity(session, context_id, surface_id)?;
            let status = shared
                .scene
                .activate_tracks(
                    identity,
                    SurfaceRevision::new(
                        map.required_u64(3).map_err(|_| ControlError::bad_message("revision"))?,
                    ),
                    &bindings,
                )
                .map_err(ControlError::state)?;
            observe_surface(session, &status, SURFACE_CHANGED_SLOTS);
            for (_slot, track_id, _generation, _milestone) in &bindings {
                if let Ok(track) = identity.track(*track_id)
                    && let Some(track_status) = shared.scene.track_status(track)
                {
                    observe_track(session, &track_status, TRACK_CHANGED_ACTIVATION);
                }
            }
            shared.request_frame_wake();
            (
                messages::TRACK_ACTIVATED,
                surface_id,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Unsigned(context_id)),
                        (1, Value::Unsigned(surface_id)),
                        (
                            2,
                            Value::Map(
                                status
                                    .active_slots
                                    .iter()
                                    .map(|(slot, track)| (*slot, Value::Unsigned(*track)))
                                    .collect(),
                            ),
                        ),
                        (3, Value::Unsigned(status.revision.get())),
                        (4, Value::Unsigned(status.revision.get())),
                    ],
                )
                .encode(),
            )
        },
        messages::BEGIN_TXN => {
            let map = StrictMap::new("BEGIN_TXN", &value, &[0, 1])
                .map_err(|_| ControlError::bad_message("invalid transaction begin"))?;
            let context = context_identity(
                session,
                map.required_u64(0).map_err(|_| ControlError::bad_message("context"))?,
            )?;
            let transaction =
                map.required_u64(1).map_err(|_| ControlError::bad_message("transaction"))?;
            shared.scene.begin_transaction(context, transaction).map_err(ControlError::state)?;
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::CREATE_NODE | messages::UPDATE_NODE => {
            let transaction = envelope
                .transaction_id
                .ok_or_else(|| ControlError::bad_message("node mutation omits transaction ID"))?;
            let node = SceneNode::decode(record.object_id, &value)
                .map_err(|_| ControlError::bad_message("invalid terminal scene node"))?;
            let context = context_identity(session, node.owning_context_id)?;
            if record.record_type == messages::CREATE_NODE {
                shared.scene.queue_node_create(context, transaction, node)
            } else {
                shared.scene.queue_node_update(context, transaction, node)
            }
            .map_err(ControlError::state)?;
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::DELETE_NODE => {
            let transaction = envelope
                .transaction_id
                .ok_or_else(|| ControlError::bad_message("node mutation omits transaction ID"))?;
            let map = StrictMap::new("DELETE_NODE", &value, &[0, 1])
                .map_err(|_| ControlError::bad_message("invalid node deletion"))?;
            let context = context_identity(
                session,
                map.required_u64(0).map_err(|_| ControlError::bad_message("context"))?,
            )?;
            let node = context
                .node(map.required_u64(1).map_err(|_| ControlError::bad_message("node"))?)
                .map_err(|_| ControlError::bad_message("node ID is zero"))?;
            shared
                .scene
                .queue_node_delete(context, transaction, node)
                .map_err(ControlError::state)?;
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::ABORT_TXN => {
            let transaction = envelope.transaction_id.unwrap_or(record.object_id);
            let context = shared
                .scene
                .transaction_context(session.identity, transaction)
                .map_err(ControlError::state)?;
            if !shared.scene.abort_transaction(context, transaction) {
                return Err(ControlError::not_found("transaction does not exist"));
            }
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::COMMIT_TXN => {
            let transaction = envelope.transaction_id.unwrap_or(record.object_id);
            let map = StrictMap::new("COMMIT_TXN", &value, &[0])
                .map_err(|_| ControlError::bad_message("invalid transaction commit"))?;
            if map.required_u64(0).map_err(|_| ControlError::bad_message("presentation mode"))? != 0
            {
                return Err(ControlError::unsupported("unsupported presentation mode"));
            }
            let context = shared
                .scene
                .transaction_context(session.identity, transaction)
                .map_err(ControlError::state)?;
            let expected_target = envelope
                .expected_target_generation
                .map(TargetGeneration::new)
                .ok_or_else(|| ControlError::bad_message("commit omits target generation"))?;
            let expected_scene = envelope
                .preconditions
                .iter()
                .find(|entry| entry.0 == 0)
                .and_then(|entry| entry.1.as_u64())
                .map(SceneRevision::new);
            let revision = match shared.scene.commit_transaction(
                context,
                transaction,
                expected_target,
                expected_scene,
            ) {
                Ok(revision) => revision,
                Err(CommitRejection::StaleTarget) => {
                    // The producer planned this commit against the target it last saw, so the
                    // rejection has to say what the target is now. The announcement precedes the
                    // error on this ordered connection, which is what lets the producer re-plan
                    // and commit again instead of failing.
                    let target = shared.scene.target();
                    let current = crate::vivid::target::TargetChange {
                        descriptor: target.descriptor(),
                        generation: target.generation(),
                        reason: 0,
                    };
                    if !session.post_control(
                        messages::TARGET_CHANGED,
                        0,
                        target_change_body(&current),
                    ) {
                        log::debug!("could not re-announce the target for a stale commit");
                    }
                    return Err(ControlError::stale_target());
                },
                Err(CommitRejection::StaleRevision) => {
                    return Err(ControlError::precondition("stale scene revision"));
                },
                Err(CommitRejection::Failed(message)) => return Err(ControlError::state(message)),
            };
            observe_scene(session, revision.get(), SCENE_CHANGED_PRODUCER_COMMIT);
            shared.request_frame_wake();
            (
                messages::SCENE_PRESENTED,
                record.object_id,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Unsigned(revision.get())),
                        (1, Value::Unsigned(expected_target.get())),
                    ],
                )
                .encode(),
            )
        },
        messages::QUERY_SCENE => {
            let status = shared.scene.scene_status(session.identity, MAX_SCENE_NODES);
            (
                messages::SCENE_STATUS,
                0,
                Envelope::new(request_id, scene_status_payload(&status)).encode(),
            )
        },
        messages::QUERY_ANCHOR => {
            let map = StrictMap::new("QUERY_ANCHOR", &value, &[0, 1])
                .map_err(|_| ControlError::bad_message("invalid anchor query"))?;
            let context_id =
                map.required_u64(0).map_err(|_| ControlError::bad_message("anchor context"))?;
            let anchor_id =
                map.required_u64(1).map_err(|_| ControlError::bad_message("anchor ID"))?;
            let identity = context_identity(session, context_id)?
                .anchor(anchor_id)
                .map_err(|_| ControlError::bad_message("anchor ID is zero"))?;
            let (state, position) = shared.scene.anchor_status(identity);
            let mut payload = vec![
                (0, Value::Unsigned(context_id)),
                (1, Value::Unsigned(anchor_id)),
                (2, Value::Unsigned(state)),
            ];
            if let Some((column, line, _)) = position {
                payload.push((3, Value::Unsigned(column as u64)));
                if line >= 0 {
                    payload.push((4, Value::Unsigned(line as u64)));
                }
                payload.push((5, Value::Bool(true)));
            }
            payload.push((6, Value::Unsigned(shared.scene.target().generation())));
            (messages::ANCHOR_STATUS, anchor_id, Envelope::new(request_id, payload).encode())
        },
        messages::WAIT_TRACK => {
            let map = StrictMap::new("WAIT_TRACK", &value, &[0, 1, 2, 3, 4, 5, 6])
                .map_err(|_| ControlError::bad_message("invalid WAIT_TRACK schema"))?;
            let identity = payload_track_identity(session, &value)?;
            let condition = map
                .required_u64(3)
                .map_err(|_| ControlError::bad_message("WAIT_TRACK condition"))?;
            let condition_value =
                map.optional_u64(4).map_err(|_| ControlError::bad_message("WAIT_TRACK value"))?;
            let timeout_us =
                map.required_u64(5).map_err(|_| ControlError::bad_message("WAIT_TRACK timeout"))?;
            let generation = ChannelGeneration::new(
                map.required_u64(6)
                    .map_err(|_| ControlError::bad_message("WAIT_TRACK generation"))?,
            );
            let value_is_valid = match condition {
                1..=4 => condition_value.is_some(),
                5..=9 => condition_value.is_none(),
                _ => false,
            };
            if timeout_us == 0
                || timeout_us > vivid_protocol::MAX_TRACK_WAIT_TIMEOUT_US
                || generation.get() == 0
                || !value_is_valid
            {
                return Err(ControlError::bad_message(
                    "WAIT_TRACK has an invalid condition, value, generation, or timeout",
                ));
            }
            match shared.scene.evaluate_track_wait(identity, generation, condition, condition_value)
            {
                TrackWaitEvaluation::Satisfied(satisfied) => (
                    messages::WAIT_SATISFIED,
                    record.object_id,
                    Envelope::new(
                        request_id,
                        wait_satisfied_payload(identity, condition, satisfied),
                    )
                    .encode(),
                ),
                TrackWaitEvaluation::NotFound => {
                    return Err(ControlError::not_found("track does not exist"));
                },
                TrackWaitEvaluation::Lost => {
                    return Err(ControlError::bad_state("track was lost while waiting"));
                },
                TrackWaitEvaluation::StaleGeneration => {
                    return Err(ControlError {
                        code: messages::ERROR_STALE_CHANNEL_GENERATION,
                        message: "track wait names a stale channel generation",
                        track: Some(identity),
                        trace_rejection: true,
                    });
                },
                TrackWaitEvaluation::NotVisible => {
                    return Err(ControlError {
                        code: messages::ERROR_NOT_VISIBLE,
                        message: "track has no eligible visible placement",
                        track: Some(identity),
                        trace_rejection: true,
                    });
                },
                TrackWaitEvaluation::Pending => {
                    let entry = Pending::TrackWait {
                        request_id,
                        object_id: record.object_id,
                        identity,
                        generation,
                        condition,
                        value: condition_value,
                        deadline: Instant::now()
                            .checked_add(Duration::from_micros(timeout_us))
                            .unwrap_or_else(Instant::now),
                    };
                    pending.register(entry).map_err(|error| match error {
                        AdmissionError::Waits => ControlError {
                            code: messages::ERROR_LIMIT_EXCEEDED,
                            message: "registered wait capacity is exhausted",
                            track: Some(identity),
                            trace_rejection: true,
                        },
                        AdmissionError::Requests => ControlError {
                            code: messages::ERROR_LIMIT_EXCEEDED,
                            message: "pending request capacity is exhausted",
                            track: Some(identity),
                            trace_rejection: true,
                        },
                    })?;
                    return Ok(None);
                },
            }
        },
        messages::CANCEL_WAIT => {
            let map = StrictMap::new("CANCEL_WAIT", &value, &[0])
                .map_err(|_| ControlError::bad_message("invalid CANCEL_WAIT schema"))?;
            let target = map
                .required_u64(0)
                .map_err(|_| ControlError::bad_message("CANCEL_WAIT request ID"))?;
            cancelled.insert(target);
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::PLAY | messages::PAUSE | messages::FLUSH | messages::DRAIN => {
            let identity = payload_track_identity(session, &value)?;
            let before = shared.scene.track_status(identity).ok_or_else(|| {
                ControlError::not_found("track does not exist").with_track(identity)
            })?;
            let output = lock(&shared.audio_outputs).get(&identity).cloned();
            match record.record_type {
                messages::PLAY => {
                    let map = StrictMap::new("PLAY", &value, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
                        .map_err(|_| {
                        ControlError::bad_message("invalid PLAY schema").with_track(identity)
                    })?;
                    let start = map
                        .required(3)
                        .map_err(|_| {
                            ControlError::bad_message("PLAY start PTS").with_track(identity)
                        })?
                        .as_i64()
                        .ok_or_else(|| {
                            ControlError::bad_message("PLAY start PTS").with_track(identity)
                        })?;
                    let minimum = map.required_u64(4).map_err(|_| {
                        ControlError::bad_message("PLAY minimum buffer").with_track(identity)
                    })?;
                    let maximum = map.required_u64(5).map_err(|_| {
                        ControlError::bad_message("PLAY maximum latency").with_track(identity)
                    })?;
                    let rate = map
                        .required(6)
                        .map_err(|_| ControlError::bad_message("PLAY rate").with_track(identity))?
                        .as_i64()
                        .ok_or_else(|| {
                            ControlError::bad_message("PLAY rate").with_track(identity)
                        })?;
                    let generation = map.required_u64(10).map_err(|_| {
                        ControlError::bad_message("PLAY generation").with_track(identity)
                    })?;
                    if minimum > maximum
                        || rate != 1_i64 << 32
                        || map.required_u64(7).ok() != Some(1)
                        || map.required_u64(8).ok() != Some(0)
                        || map.required_u64(9).ok() != Some(1)
                        || generation != before.state.channel_generation.get()
                    {
                        return Err(ControlError::bad_state(
                            "PLAY policy, latency, rate, or generation is invalid",
                        )
                        .with_track(identity));
                    }
                    shared
                        .scene
                        .start_playback(identity, start)
                        .map_err(|message| ControlError::state(message).with_track(identity))?;
                    if let Some(output) = &output {
                        output.configure_play(start, minimum);
                        output.start();
                    }
                    let revision_after = shared
                        .scene
                        .track_status(identity)
                        .map_or(before.state.revision.get(), |status| status.state.revision.get());
                    shared.trace(
                        trace::TraceCategory::Playback,
                        "play_applied",
                        Some(identity),
                        serde_json::json!({
                            "operation": "play",
                            "request_id": request_id,
                            "control_record_sequence": record.sequence,
                            "record_type": record.record_type,
                            "object_id": record.object_id,
                            "track_revision_before": before.state.revision.get(),
                            "track_revision_after": revision_after,
                            "channel_generation": before.state.channel_generation.get(),
                            "start_pts_us": start,
                            "minimum_buffer_us": minimum,
                            "maximum_latency_us": maximum,
                            "rate_q32_32": rate,
                            "audio_output_updated": output.is_some(),
                        }),
                    );
                },
                messages::PAUSE => {
                    shared
                        .scene
                        .pause_playback(identity)
                        .map_err(|message| ControlError::state(message).with_track(identity))?;
                    if let Some(output) = &output {
                        output.pause();
                    }
                    let revision_after = shared
                        .scene
                        .track_status(identity)
                        .map_or(before.state.revision.get(), |status| status.state.revision.get());
                    shared.trace(
                        trace::TraceCategory::Playback,
                        "pause_applied",
                        Some(identity),
                        serde_json::json!({
                            "operation": "pause",
                            "request_id": request_id,
                            "control_record_sequence": record.sequence,
                            "record_type": record.record_type,
                            "object_id": record.object_id,
                            "track_revision_before": before.state.revision.get(),
                            "track_revision_after": revision_after,
                            "channel_generation": before.state.channel_generation.get(),
                            "audio_output_updated": output.is_some(),
                        }),
                    );
                },
                messages::FLUSH => {
                    let epoch = StrictMap::new("FLUSH", &value, &[0, 1, 2, 3])
                        .map_err(|_| {
                            ControlError::bad_message("invalid FLUSH schema").with_track(identity)
                        })?
                        .required_u64(3)
                        .ok()
                        .and_then(|value| u32::try_from(value).ok())
                        .ok_or_else(|| {
                            ControlError::bad_message("FLUSH epoch").with_track(identity)
                        })?;
                    shared
                        .scene
                        .flush_playback(identity, epoch)
                        .map_err(|message| ControlError::state(message).with_track(identity))?;
                    if let Some(output) = &output {
                        output.flush();
                    }
                    let revision_after = shared
                        .scene
                        .track_status(identity)
                        .map_or(before.state.revision.get(), |status| status.state.revision.get());
                    shared.trace(
                        trace::TraceCategory::Playback,
                        "flush_applied",
                        Some(identity),
                        serde_json::json!({
                            "operation": "flush",
                            "request_id": request_id,
                            "control_record_sequence": record.sequence,
                            "record_type": record.record_type,
                            "object_id": record.object_id,
                            "track_revision_before": before.state.revision.get(),
                            "track_revision_after": revision_after,
                            "channel_generation": before.state.channel_generation.get(),
                            "previous_media_epoch": before.state.media_epoch,
                            "media_epoch": epoch,
                            "audio_output_updated": output.is_some(),
                        }),
                    );
                },
                messages::DRAIN => {
                    if let Some(output) = output {
                        pending
                            .register(Pending::AudioDrain {
                                request_id,
                                object_id: record.object_id,
                                identity,
                                generation: before.state.channel_generation,
                                output: output.clone(),
                            })
                            .map_err(|_| ControlError {
                                code: messages::ERROR_LIMIT_EXCEEDED,
                                message: "pending request capacity is exhausted",
                                track: Some(identity),
                                trace_rejection: true,
                            })?;
                        output.signal_eos();
                        shared.trace(
                            trace::TraceCategory::Playback,
                            "drain_applied",
                            Some(identity),
                            serde_json::json!({
                                "operation": "drain",
                                "request_id": request_id,
                                "control_record_sequence": record.sequence,
                                "record_type": record.record_type,
                                "object_id": record.object_id,
                                "track_revision_before": before.state.revision.get(),
                                "track_revision_after": before.state.revision.get(),
                                "channel_generation": before.state.channel_generation.get(),
                                "completion_pending": true,
                            }),
                        );
                        return Ok(None);
                    }
                    shared
                        .scene
                        .mark_buffered_ended(identity, before.state.channel_generation)
                        .map_err(|message| ControlError::state(message).with_track(identity))?;
                    let revision_after = shared
                        .scene
                        .track_status(identity)
                        .map_or(before.state.revision.get(), |status| status.state.revision.get());
                    shared.trace(
                        trace::TraceCategory::Playback,
                        "drain_applied",
                        Some(identity),
                        serde_json::json!({
                            "operation": "drain",
                            "request_id": request_id,
                            "control_record_sequence": record.sequence,
                            "record_type": record.record_type,
                            "object_id": record.object_id,
                            "track_revision_before": before.state.revision.get(),
                            "track_revision_after": revision_after,
                            "channel_generation": before.state.channel_generation.get(),
                            "completion_pending": false,
                        }),
                    );
                },
                _ => unreachable!(),
            }
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::SET_OBSERVATION => {
            let map = StrictMap::new("SET_OBSERVATION", &value, &[0])
                .map_err(|_| ControlError::bad_message("invalid SET_OBSERVATION schema"))?;
            let mask = map
                .required_u64(0)
                .map_err(|_| ControlError::bad_message("SET_OBSERVATION mask"))?;
            lock(&session.observations)
                .subscribe(mask)
                .map_err(|_| ControlError::bad_message("unassigned observation class bits"))?;
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        _ if record.flags & RECORD_OPTIONAL != 0 => {
            return Ok(None);
        },
        _ => {
            return Err(ControlError::unsupported(
                "record is not implemented by the terminal presentation target",
            ));
        },
    };
    let body = reply.2.map_err(|_| {
        ControlError::bad_message("reply encoding failed").without_rejection_trace()
    })?;
    Ok(Some((reply.0, reply.1, body)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelFailureKind {
    ProtocolFraming,
    TransportRead,
    TransportWrite,
    RateLimit,
    FlowControl,
    RecordIdentity,
    RecordType,
    MediaParse,
    MediaAdmission,
    Decode,
    AudioOutput,
    SceneState,
    InternalState,
}

impl ChannelFailureKind {
    const fn name(self) -> &'static str {
        match self {
            Self::ProtocolFraming => "protocol_framing",
            Self::TransportRead => "transport_read",
            Self::TransportWrite => "transport_write",
            Self::RateLimit => "rate_limit",
            Self::FlowControl => "flow_control",
            Self::RecordIdentity => "record_identity",
            Self::RecordType => "record_type",
            Self::MediaParse => "media_parse",
            Self::MediaAdmission => "media_admission",
            Self::Decode => "decode",
            Self::AudioOutput => "audio_output",
            Self::SceneState => "scene_state",
            Self::InternalState => "internal_state",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ChannelRecordContext {
    record_type: Option<u16>,
    record_sequence: Option<u64>,
    body_bytes: Option<u64>,
    media_epoch: Option<u32>,
    media_id: Option<u64>,
    pts_us: Option<i64>,
}

impl ChannelRecordContext {
    fn begin_record(&mut self, record_type: u16, record_sequence: u64, body_bytes: usize) {
        *self = Self {
            record_type: Some(record_type),
            record_sequence: Some(record_sequence),
            body_bytes: Some(u64::try_from(body_bytes).unwrap_or(u64::MAX)),
            ..Self::default()
        };
    }

    fn media(&mut self, epoch: u32, media_id: u64, pts_us: i64) {
        self.media_epoch = Some(epoch);
        self.media_id = Some(media_id);
        self.pts_us = Some(pts_us);
    }

    fn json(self) -> serde_json::Value {
        serde_json::json!({
            "record_type": self.record_type,
            "record_sequence": self.record_sequence,
            "body_bytes": self.body_bytes,
            "media_epoch": self.media_epoch,
            "media_id": self.media_id,
            "pts_us": self.pts_us,
        })
    }
}

#[derive(Debug)]
struct ChannelFailure {
    kind: ChannelFailureKind,
    error: io::Error,
    context: ChannelRecordContext,
}

impl ChannelFailure {
    fn io(kind: ChannelFailureKind, error: io::Error, context: ChannelRecordContext) -> Self {
        Self { kind, error, context }
    }

    fn other(
        kind: ChannelFailureKind,
        error: impl std::fmt::Display,
        context: ChannelRecordContext,
    ) -> Self {
        Self::io(kind, io::Error::other(error.to_string()), context)
    }

    fn message(
        kind: ChannelFailureKind,
        error_kind: ErrorKind,
        message: &'static str,
        context: ChannelRecordContext,
    ) -> Self {
        Self::io(kind, io::Error::new(error_kind, message), context)
    }

    fn read(error: io::Error, context: ChannelRecordContext) -> Self {
        let kind = if error.kind() == ErrorKind::InvalidData {
            ChannelFailureKind::ProtocolFraming
        } else {
            ChannelFailureKind::TransportRead
        };
        Self::io(kind, error, context)
    }

    fn protocol_error_code(&self) -> u64 {
        match self.kind {
            ChannelFailureKind::RateLimit => messages::ERROR_RATE_LIMITED,
            ChannelFailureKind::FlowControl | ChannelFailureKind::MediaAdmission => {
                messages::ERROR_FLOW_CONTROL
            },
            ChannelFailureKind::AudioOutput => messages::ERROR_DEVICE_LOST,
            _ if matches!(
                self.error.kind(),
                ErrorKind::NotFound | ErrorKind::NotConnected | ErrorKind::BrokenPipe
            ) =>
            {
                messages::ERROR_DEVICE_LOST
            },
            _ => messages::ERROR_DECODER,
        }
    }

    fn diagnostic(&self) -> String {
        sanitize_trace_diagnostic(&self.error.to_string())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind.name(),
            "io_kind": io_error_kind_name(self.error.kind()),
            "error_code": self.protocol_error_code(),
            "diagnostic": self.diagnostic(),
        })
    }
}

fn sanitize_trace_diagnostic(input: &str) -> String {
    const MAXIMUM_BYTES: usize = 1_024;
    let mut output = String::new();
    for character in input.chars() {
        let character = if character.is_control() { ' ' } else { character };
        if output.len().saturating_add(character.len_utf8()) > MAXIMUM_BYTES {
            break;
        }
        output.push(character);
    }
    output
}

fn io_error_kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::ConnectionRefused => "connection_refused",
        ErrorKind::ConnectionReset => "connection_reset",
        ErrorKind::ConnectionAborted => "connection_aborted",
        ErrorKind::NotConnected => "not_connected",
        ErrorKind::AddrInUse => "address_in_use",
        ErrorKind::AddrNotAvailable => "address_not_available",
        ErrorKind::BrokenPipe => "broken_pipe",
        ErrorKind::AlreadyExists => "already_exists",
        ErrorKind::WouldBlock => "would_block",
        ErrorKind::InvalidInput => "invalid_input",
        ErrorKind::InvalidData => "invalid_data",
        ErrorKind::TimedOut => "timed_out",
        ErrorKind::WriteZero => "write_zero",
        ErrorKind::Interrupted => "interrupted",
        ErrorKind::Unsupported => "unsupported",
        ErrorKind::UnexpectedEof => "unexpected_eof",
        ErrorKind::OutOfMemory => "out_of_memory",
        ErrorKind::Other => "other",
        _ => "other",
    }
}

#[allow(clippy::too_many_arguments)]
fn trace_track_channel_rejected(
    shared: &ServiceShared,
    track: Option<TrackIdentity>,
    request_id: u64,
    record_sequence: u64,
    channel_generation: Option<u64>,
    error_code: u64,
    reason: &'static str,
    authenticated: bool,
) {
    shared.trace(
        trace::TraceCategory::Lifecycle,
        "track_channel_rejected",
        track,
        serde_json::json!({
            "request_id": request_id,
            "record_sequence": record_sequence,
            "channel_generation": channel_generation,
            "error_code": error_code,
            "reason": reason,
            "authenticated": authenticated,
        }),
    );
}

fn handle_track_channel(
    reader: &mut Reader,
    shared: &Arc<ServiceShared>,
    pending: &PendingConnection,
) -> io::Result<()> {
    let writer = Arc::new(reader.writer(ConnectionKind::Track)?);
    let first = reader.read_record(ConnectionKind::Track)?;
    let envelope = match messages::decode_control(&first.body) {
        Ok(envelope) => envelope,
        Err(error) => {
            trace_track_channel_rejected(
                shared,
                None,
                0,
                first.sequence,
                None,
                messages::ERROR_BAD_MESSAGE,
                "invalid_channel_open",
                false,
            );
            return Err(io::Error::other(error));
        },
    };
    let request_id = envelope.request_id;
    let open = match ChannelOpen::decode(first.object_id, &first.body) {
        Ok(open) => open,
        Err(error) => {
            trace_track_channel_rejected(
                shared,
                None,
                request_id,
                first.sequence,
                None,
                messages::ERROR_BAD_MESSAGE,
                "invalid_channel_open",
                false,
            );
            return Err(io::Error::other(error));
        },
    };
    let session = match lock(&shared.registry).sessions.get(&open.session_id).cloned() {
        Some(session) => session,
        None => {
            trace_track_channel_rejected(
                shared,
                None,
                request_id,
                first.sequence,
                None,
                messages::ERROR_NOT_FOUND,
                "session_not_found",
                false,
            );
            return Err(io::Error::new(ErrorKind::NotFound, "session does not exist"));
        },
    };
    let identity = match track_identity(&session, open.context_id, open.surface_id, open.track_id) {
        Ok(identity) => identity,
        Err(error) => {
            trace_track_channel_rejected(
                shared,
                None,
                request_id,
                first.sequence,
                None,
                error.code,
                "track_identity_rejected",
                false,
            );
            return Err(io::Error::new(ErrorKind::InvalidData, error.message));
        },
    };
    let status = match shared.scene.track_status(identity) {
        Some(status) => status,
        None => {
            trace_track_channel_rejected(
                shared,
                None,
                request_id,
                first.sequence,
                None,
                messages::ERROR_NOT_FOUND,
                "track_not_found",
                false,
            );
            return Err(io::Error::new(ErrorKind::NotFound, "track does not exist"));
        },
    };
    if status.state.channel_generation.get() != open.channel_generation
        || status.configuration.kind.kind() != open.track_kind
        || status.configuration.lane != open.lane
    {
        trace_track_channel_rejected(
            shared,
            None,
            request_id,
            first.sequence,
            None,
            messages::ERROR_STALE_CHANNEL_GENERATION,
            "immutable_track_mismatch",
            false,
        );
        writer.write_record(
            messages::ERROR,
            open.track_id,
            &protocol_error(
                request_id,
                messages::ERROR_STALE_CHANNEL_GENERATION,
                true,
                "CHANNEL_OPEN does not match the immutable track generation",
            )?,
        )?;
        return Err(io::Error::new(ErrorKind::InvalidData, "stale channel generation"));
    }
    let expected_tag = auth::channel_tag(
        session.channel_key.expose(),
        open.session_id,
        open.context_id,
        open.surface_id,
        open.track_id,
        open.channel_generation,
        open.track_kind as u32,
        open.lane as u32,
        &open.client_nonce,
    );
    if !auth::verify_tag(&expected_tag, &open.authentication_tag) {
        trace_track_channel_rejected(
            shared,
            None,
            request_id,
            first.sequence,
            None,
            messages::ERROR_AUTH_FAILED,
            "authentication_failed",
            false,
        );
        writer.write_record(
            messages::ERROR,
            open.track_id,
            &protocol_error(
                request_id,
                messages::ERROR_AUTH_FAILED,
                true,
                "channel authentication failed",
            )?,
        )?;
        return Err(io::Error::new(ErrorKind::PermissionDenied, "channel authentication failed"));
    }
    // The channel tag is proved, so this connection is a media channel that may then wait on its
    // producer for as long as the track lives.
    if let Err(error) = pending.authenticated(reader) {
        trace_track_channel_rejected(
            shared,
            Some(identity),
            request_id,
            first.sequence,
            Some(open.channel_generation),
            messages::ERROR_DEVICE_LOST,
            "connection_setup_failed",
            true,
        );
        return Err(error);
    }
    let generation = ChannelGeneration::new(open.channel_generation);
    let (maximum_bytes, maximum_records) = live_channel_flow(&status.configuration);
    let acceptance = vec![
        (0, Value::Unsigned(open.context_id)),
        (1, Value::Unsigned(open.surface_id)),
        (2, Value::Unsigned(open.track_id)),
        (3, Value::Unsigned(open.channel_generation)),
        (4, Value::Unsigned(maximum_bytes)),
        (5, Value::Unsigned(maximum_records)),
        (6, Value::Unsigned(u64::from(status.configuration.maximum_record_body))),
        (7, Value::Unsigned(status.state.revision.get().saturating_add(1))),
    ];
    let decision = lock(&shared.registry).channel_opens.entry(identity).or_default().open(
        status.state.channel_generation,
        generation,
        open.client_nonce,
        &first.body,
        acceptance,
    );
    let (acceptance, replayed) = match decision {
        ChannelOpenDecision::Fresh(acceptance) => (acceptance, false),
        ChannelOpenDecision::ExactReplay(acceptance) => (acceptance, true),
        ChannelOpenDecision::Busy => {
            trace_track_channel_rejected(
                shared,
                Some(identity),
                request_id,
                first.sequence,
                Some(generation.get()),
                messages::ERROR_CHANNEL_BUSY,
                "channel_busy",
                true,
            );
            return Err(send_fatal(
                &writer,
                request_id,
                messages::ERROR_CHANNEL_BUSY,
                "track channel generation is already attached",
            ));
        },
        ChannelOpenDecision::DifferentBytes | ChannelOpenDecision::StaleGeneration => {
            trace_track_channel_rejected(
                shared,
                Some(identity),
                request_id,
                first.sequence,
                Some(generation.get()),
                messages::ERROR_STALE_CHANNEL_GENERATION,
                "stale_or_different_open",
                true,
            );
            return Err(send_fatal(
                &writer,
                request_id,
                messages::ERROR_STALE_CHANNEL_GENERATION,
                "CHANNEL_OPEN retry is stale or differs from the accepted bytes",
            ));
        },
    };
    let status = if replayed {
        match shared.scene.track_status(identity) {
            Some(status) => status,
            None => {
                trace_track_channel_rejected(
                    shared,
                    Some(identity),
                    request_id,
                    first.sequence,
                    Some(generation.get()),
                    messages::ERROR_NOT_FOUND,
                    "track_disappeared",
                    true,
                );
                return Err(io::Error::new(ErrorKind::NotFound, "track disappeared"));
            },
        }
    } else {
        match shared.scene.accept_channel(identity, generation, maximum_bytes, maximum_records) {
            Ok(status) => status,
            Err(error) => {
                trace_track_channel_rejected(
                    shared,
                    Some(identity),
                    request_id,
                    first.sequence,
                    Some(generation.get()),
                    messages::ERROR_BAD_STATE,
                    "channel_accept_failed",
                    true,
                );
                return Err(io::Error::other(error));
            },
        }
    };
    let mut attachment =
        TrackAttachmentCleanup { shared: shared.clone(), identity, generation, armed: true };
    let acceptance = match Envelope::new(request_id, acceptance).encode() {
        Ok(acceptance) => acceptance,
        Err(error) => {
            trace_track_channel_rejected(
                shared,
                Some(identity),
                request_id,
                first.sequence,
                Some(generation.get()),
                messages::ERROR_BAD_MESSAGE,
                "channel_accept_reply_failed",
                true,
            );
            return Err(io::Error::other(error));
        },
    };
    if let Err(error) = writer.write_record(messages::CHANNEL_ACCEPTED, open.track_id, &acceptance)
    {
        trace_track_channel_rejected(
            shared,
            Some(identity),
            request_id,
            first.sequence,
            Some(generation.get()),
            messages::ERROR_DEVICE_LOST,
            "channel_accept_write_failed",
            true,
        );
        return Err(error);
    }
    shared.trace(
        trace::TraceCategory::Lifecycle,
        "track_channel_accepted",
        Some(identity),
        serde_json::json!({
            "channel_generation": generation.get(),
            "maximum_body_bytes": maximum_bytes,
            "maximum_media_records": maximum_records,
            "request_id": request_id,
            "record_sequence": first.sequence,
            "replayed": replayed,
        }),
    );
    let result = reader
        .set_maximum(status.configuration.maximum_record_body)
        .map_err(|error| {
            ChannelFailure::io(
                ChannelFailureKind::InternalState,
                error,
                ChannelRecordContext::default(),
            )
        })
        .and_then(|()| channel_loop(reader, &writer, shared, identity, generation));
    attachment.detach();
    let clean = result.is_ok();
    let context = match &result {
        Ok(context) => *context,
        Err(failure) => failure.context,
    };
    let lost_status = if result.is_err() {
        shared.scene.lose_track(identity, generation).ok().flatten()
    } else {
        None
    };
    let current_status = lost_status.clone().or_else(|| shared.scene.track_status(identity));
    let disposition = if lost_status.is_some() {
        "track_lost"
    } else {
        match current_status.as_ref() {
            None => "owner_removed",
            Some(status) if status.state.channel_generation != generation => "superseded",
            Some(_) => "detached",
        }
    };
    let current_generation =
        current_status.as_ref().map(|status| status.state.channel_generation.get());
    let failure_json = result.as_ref().err().map(ChannelFailure::json);
    shared.trace(
        trace::TraceCategory::Lifecycle,
        "track_channel_detached",
        Some(identity),
        serde_json::json!({
            "channel_generation": generation.get(),
            "current_channel_generation": current_generation,
            "clean": clean,
            "outcome": if clean { "clean" } else { "failed" },
            "disposition": disposition,
            "last_record": context.json(),
            "failure": failure_json,
        }),
    );
    if let (Err(failure), Some(status)) = (&result, lost_status) {
        stop_failed_audio_output(&shared.audio_outputs, identity);
        let error_code = failure.protocol_error_code();
        let diagnostic = failure.diagnostic();
        shared.trace(
            trace::TraceCategory::Lifecycle,
            "track_lost",
            Some(identity),
            serde_json::json!({
                "channel_generation": generation.get(),
                "current_channel_generation": status.state.channel_generation.get(),
                "last_record": failure.context.json(),
                "failure": failure.json(),
                "error_code": error_code,
            }),
        );
        shared.request_frame_wake();
        let body = Envelope::new(
            0,
            vec![
                (0, Value::Unsigned(identity.surface.context.context_id)),
                (1, Value::Unsigned(identity.surface.surface_id)),
                (2, Value::Unsigned(identity.track_id)),
                (3, Value::Unsigned(error_code)),
                (4, Value::Unsigned(status.state.revision.get())),
                (5, Value::Map(vec![])),
                (6, Value::Text(diagnostic)),
            ],
        )
        .encode()?;
        // This is the track channel's thread, not the session's. Queue so a stalled control peer
        // cannot hold a media connection open past its own failure.
        session.post_control(messages::TRACK_LOST, identity.track_id, body);
    }
    match result {
        Ok(_) => Ok(()),
        Err(failure) => Err(failure.error),
    }
}

/// Keep video flow within its declared live-latency horizon, but reserve two seconds for live
/// audio. Audio is low bandwidth and cannot be reconstructed after a producer-side drop, while
/// stale video can be discarded and recovered at a fresh keyframe.
fn live_channel_flow(configuration: &TrackConfiguration) -> (u64, u64) {
    let latency_us = if configuration.mode == TrackMode::Live
        && matches!(&configuration.kind, KindConfiguration::Audio(_))
    {
        LIVE_AUDIO_FLOW_RESERVE_US
    } else {
        configuration.maximum_latency_us.max(1)
    };
    let latency_bytes = configuration
        .maximum_encoded_bits_per_second
        .saturating_mul(latency_us)
        .div_ceil(8_000_000);
    let maximum_bytes = u64::from(configuration.maximum_record_body)
        .saturating_add(latency_bytes)
        .min(configuration.maximum_inflight_body_bytes)
        .max(u64::from(configuration.maximum_record_body));
    let maximum_records = configuration
        .maximum_records_per_second
        .saturating_mul(latency_us)
        .div_ceil(1_000_000)
        .max(2);
    (maximum_bytes, maximum_records)
}

/// The live audio delay after video arrived `headroom_us` *behind* the audio clock.
///
/// The clock is already retarded by the delay in force, so the shortfall is exactly what still has
/// to be added for sound and picture to be presented together.
fn grown_live_delay_us(current_us: u64, headroom_us: i64) -> u64 {
    let behind_us = u64::try_from(headroom_us.saturating_neg().max(0)).unwrap_or(0);
    current_us.saturating_add(behind_us)
}

/// The live audio delay after a review window in which video was never late.
///
/// Only the margin above [`LIVE_DELAY_HEADROOM_US`] is given back: shrinking to the exact observed
/// minimum would leave the next frame with nothing to absorb ordinary jitter.
fn shrunk_live_delay_us(current_us: u64, least_headroom_us: i64) -> u64 {
    let spare_us = least_headroom_us.saturating_sub(LIVE_DELAY_HEADROOM_US).max(0);
    current_us.saturating_sub(u64::try_from(spare_us).unwrap_or(0))
}

/// What a step in the live audio timeline means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioTimelineStep {
    /// Within jitter of where the previous packet ended: play it.
    Continuous,
    /// The producer dropped this much timeline. Bridging it with silence keeps the media clock
    /// aligned without discarding a single buffered sample.
    Gap(u64),
    /// The producer's clock went backwards, which a continuous timeline cannot express.
    Restart,
}

fn classify_audio_step(expected_pts_us: Option<i64>, pts_us: i64, live: bool) -> AudioTimelineStep {
    let Some(expected) = expected_pts_us.filter(|_| live) else {
        return AudioTimelineStep::Continuous;
    };
    let step_us = pts_us.saturating_sub(expected);
    if step_us >= AUDIO_GAP_US {
        AudioTimelineStep::Gap(u64::try_from(step_us).unwrap_or(0))
    } else if step_us <= -AUDIO_GAP_US {
        AudioTimelineStep::Restart
    } else {
        AudioTimelineStep::Continuous
    }
}

/// Ask the producer for a random-access unit on this channel, media §13.
fn request_keyframe(
    writer: &Writer,
    shared: &ServiceShared,
    identity: TrackIdentity,
    generation: ChannelGeneration,
    reason: u64,
) {
    let payload = vec![
        (0, Value::Unsigned(identity.surface.context.context_id)),
        (1, Value::Unsigned(identity.surface.surface_id)),
        (2, Value::Unsigned(identity.track_id)),
        (3, Value::Unsigned(generation.get())),
        (4, Value::Unsigned(0)),
        (5, Value::Unsigned(reason)),
    ];
    if let Ok(body) = Envelope::new(0, payload).encode() {
        shared.trace(
            trace::TraceCategory::Recovery,
            "need_keyframe_queued",
            Some(identity),
            serde_json::json!({
                "channel_generation": generation.get(),
                "minimum_epoch": 0,
                "reason": reason,
            }),
        );
        if writer.write_record(messages::NEED_KEYFRAME, identity.track_id, &body).is_ok() {
            shared.trace(
                trace::TraceCategory::Recovery,
                "need_keyframe_written",
                Some(identity),
                serde_json::json!({
                    "channel_generation": generation.get(),
                    "minimum_epoch": 0,
                    "reason": reason,
                }),
            );
        }
    }
}

/// Ask the producer for a full raster frame on this channel, media §13.
fn request_full_frame(
    writer: &Writer,
    identity: TrackIdentity,
    generation: ChannelGeneration,
    reason: u64,
) {
    let payload = vec![
        (0, Value::Unsigned(identity.surface.context.context_id)),
        (1, Value::Unsigned(identity.surface.surface_id)),
        (2, Value::Unsigned(identity.track_id)),
        (3, Value::Unsigned(generation.get())),
        (4, Value::Unsigned(reason)),
    ];
    if let Ok(body) = Envelope::new(0, payload).encode() {
        let _ = writer.write_record(messages::NEED_FULL_FRAME, identity.track_id, &body);
    }
}

fn channel_loop(
    reader: &mut Reader,
    writer: &Writer,
    shared: &Arc<ServiceShared>,
    identity: TrackIdentity,
    generation: ChannelGeneration,
) -> Result<ChannelRecordContext, ChannelFailure> {
    let mut context = ChannelRecordContext::default();
    macro_rules! channel_io {
        ($kind:expr, $expression:expr) => {
            $expression.map_err(|error| ChannelFailure::io($kind, error, context))?
        };
    }
    macro_rules! channel_other {
        ($kind:expr, $expression:expr) => {
            $expression.map_err(|error| ChannelFailure::other($kind, error, context))?
        };
    }

    let status = shared.scene.track_status(identity).ok_or_else(|| {
        ChannelFailure::message(
            ChannelFailureKind::InternalState,
            ErrorKind::NotFound,
            "track disappeared",
            context,
        )
    })?;
    let configuration = status.configuration;
    let mut video_decoder = match &configuration.kind {
        KindConfiguration::Video(video) => {
            Some(channel_io!(ChannelFailureKind::Decode, Decoder::new(video, configuration.mode)))
        },
        _ => None,
    };
    let mut audio = match &configuration.kind {
        KindConfiguration::Audio(audio_configuration) => {
            let output = channel_io!(ChannelFailureKind::AudioOutput, AudioOutput::open());
            output.set_gain(status.audio_gain);
            let decoder =
                channel_io!(ChannelFailureKind::Decode, output.decoder(audio_configuration));
            if configuration.mode == vivid_protocol::track::TrackMode::Live {
                output.start();
            }
            lock(&shared.audio_outputs).insert(identity, output.clone());
            Some((output, decoder))
        },
        _ => None,
    };
    let maximum_record_charge = u64::from(configuration.maximum_record_body);
    let byte_rate = configuration
        .maximum_encoded_bits_per_second
        .checked_add(7)
        .map(|bits| bits / 8)
        .unwrap_or(u64::MAX);
    let mut byte_bucket = TokenBucket::new(byte_rate, maximum_record_charge);
    let mut record_bucket = TokenBucket::new(configuration.maximum_records_per_second, 1);
    let mut last_rate_update = Instant::now();
    let mut latency_recovery_epoch = None;
    let mut expected_audio_pts_us = None;
    let mut last_latency_keyframe: Option<Instant> = None;
    let mut delay_review_started = Instant::now();
    let mut delay_window_headroom_us: Option<i64> = None;
    let mut last_flow_trace = Instant::now();
    // Every record here is parsed and finished with before the next one is read, so the channel
    // reads into one buffer for the life of the connection instead of allocating per record — on
    // the path that carries every video packet, every raster frame and every audio packet.
    let mut body = Vec::new();
    loop {
        let mut recovery_unit = false;
        let header = match reader.read_record_into(ConnectionKind::Track, &mut body) {
            Ok(header) => header,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(context),
            Err(error) => return Err(ChannelFailure::read(error, context)),
        };
        context.begin_record(header.record_type, header.sequence, body.len());
        if header.object_id != identity.track_id {
            return Err(ChannelFailure::message(
                ChannelFailureKind::RecordIdentity,
                ErrorKind::InvalidData,
                "media record object ID does not match the track",
                context,
            ));
        }
        if matches!(
            header.record_type,
            messages::VIDEO_PACKET
                | messages::AUDIO_PACKET
                | messages::RASTER_FRAME
                | messages::IMAGE_DATA
        ) {
            channel_io!(
                ChannelFailureKind::RateLimit,
                pace_ingress(
                    &mut byte_bucket,
                    &mut record_bucket,
                    &mut last_rate_update,
                    u64::try_from(body.len()).unwrap_or(u64::MAX),
                )
            );
            let mut registry = lock(&shared.registry);
            let channel = registry.channel_opens.get_mut(&identity).ok_or_else(|| {
                ChannelFailure::message(
                    ChannelFailureKind::InternalState,
                    ErrorKind::Other,
                    "channel-open state disappeared",
                    context,
                )
            })?;
            channel_other!(ChannelFailureKind::InternalState, channel.admit_media(generation));
        }
        match header.record_type {
            messages::RASTER_FRAME => {
                let KindConfiguration::Raster(raster) = &configuration.kind else {
                    return Err(ChannelFailure::message(
                        ChannelFailureKind::RecordType,
                        ErrorKind::InvalidData,
                        "RASTER_FRAME used a non-raster track",
                        context,
                    ));
                };
                let (frame, media_epoch) = if let Ok(parsed) = media::parse_full_raster_frame(&body)
                {
                    if parsed.width != raster.width
                        || parsed.height != raster.height
                        || (parsed.compressed && !raster.zstd_enabled)
                    {
                        return Err(ChannelFailure::message(
                            ChannelFailureKind::MediaParse,
                            ErrorKind::InvalidData,
                            "raster frame differs from immutable configuration",
                            context,
                        ));
                    }
                    let media_epoch = parsed.epoch;
                    let frame = Frame {
                        frame_id: parsed.frame_id,
                        pts_us: parsed.pts_us,
                        width: parsed.width,
                        height: parsed.height,
                        sar_num: 1,
                        sar_den: 1,
                        alpha_mode: raster.alpha_mode,
                        rgba: Arc::new(RgbaBuffer::new(channel_io!(
                            ChannelFailureKind::Decode,
                            media::decode_raster_pixels(parsed)
                        ))),
                        damage: None,
                    };
                    (frame, media_epoch)
                } else {
                    if !raster.delta_enabled {
                        return Err(ChannelFailure::message(
                            ChannelFailureKind::RecordType,
                            ErrorKind::InvalidData,
                            "raster delta was not negotiated",
                            context,
                        ));
                    }
                    let delta = channel_io!(
                        ChannelFailureKind::MediaParse,
                        media::parse_delta_raster_frame(
                            &body,
                            raster.width,
                            raster.height,
                            u32::from(raster.maximum_delta_operations),
                        )
                    );
                    let media_epoch = delta.epoch;
                    let Some(base) = shared.scene.latest_frame(identity) else {
                        // Media §13: no base, so ask for a full frame instead of losing the track.
                        request_full_frame(writer, identity, generation, NEED_FULL_FRAME_NO_BASE);
                        continue;
                    };
                    (
                        channel_io!(ChannelFailureKind::Decode, apply_raster_delta(&base, delta)),
                        media_epoch,
                    )
                };
                context.media(media_epoch, frame.frame_id, frame.pts_us);
                let body_length = u32::try_from(body.len()).map_err(|_| {
                    ChannelFailure::message(
                        ChannelFailureKind::MediaParse,
                        ErrorKind::InvalidData,
                        "raster record exceeds u32",
                        context,
                    )
                })?;
                channel_other!(
                    ChannelFailureKind::MediaAdmission,
                    shared.scene.publish_frame(
                        identity,
                        generation,
                        body_length,
                        media_epoch,
                        frame.frame_id,
                        frame.damage.is_none(),
                        header.sequence,
                        frame,
                    )
                );
                shared.request_frame_wake();
            },
            messages::IMAGE_DATA => {
                let status = shared.scene.track_status(identity).ok_or_else(|| {
                    ChannelFailure::message(
                        ChannelFailureKind::InternalState,
                        ErrorKind::NotFound,
                        "track disappeared",
                        context,
                    )
                })?;
                let KindConfiguration::EncodedImage(configuration) = status.configuration.kind
                else {
                    return Err(ChannelFailure::message(
                        ChannelFailureKind::RecordType,
                        ErrorKind::InvalidData,
                        "IMAGE_DATA used a non-image track",
                        context,
                    ));
                };
                if body.len() != configuration.encoded_length as usize
                    || configuration.sha256.is_some_and(|expected| {
                        let actual: [u8; 32] = Sha256::digest(&body).into();
                        actual != expected
                    })
                {
                    return Err(ChannelFailure::message(
                        ChannelFailureKind::MediaParse,
                        ErrorKind::InvalidData,
                        "encoded image length or hash differs from immutable configuration",
                        context,
                    ));
                }
                let format =
                    channel_io!(ChannelFailureKind::Decode, image_format(configuration.encoding));
                let image = channel_other!(
                    ChannelFailureKind::Decode,
                    image::load_from_memory_with_format(&body, format)
                )
                .to_rgba8();
                let (width, height) = image.dimensions();
                if width != configuration.width || height != configuration.height {
                    return Err(ChannelFailure::message(
                        ChannelFailureKind::Decode,
                        ErrorKind::InvalidData,
                        "decoded image dimensions differ from immutable configuration",
                        context,
                    ));
                }
                context.media(0, 1, 0);
                let body_length = u32::try_from(body.len()).map_err(|_| {
                    ChannelFailure::message(
                        ChannelFailureKind::MediaParse,
                        ErrorKind::InvalidData,
                        "image record exceeds u32",
                        context,
                    )
                })?;
                channel_other!(
                    ChannelFailureKind::MediaAdmission,
                    shared.scene.publish_frame(
                        identity,
                        generation,
                        body_length,
                        0,
                        1,
                        true,
                        header.sequence,
                        Frame {
                            frame_id: 1,
                            pts_us: 0,
                            width,
                            height,
                            sar_num: 1,
                            sar_den: 1,
                            alpha_mode: scene::ALPHA_STRAIGHT,
                            rgba: Arc::new(RgbaBuffer::new(image.into_raw())),
                            damage: None,
                        },
                    )
                );
                shared.request_frame_wake();
            },
            messages::VIDEO_PACKET => {
                let packet = channel_other!(
                    ChannelFailureKind::MediaParse,
                    media::parse_video_packet(&body)
                );
                let random_access = packet.flags & media::VIDEO_PACKET_KEY != 0;
                recovery_unit = random_access;
                let packet_epoch = packet.epoch;
                let packet_id = packet.packet_id;
                let packet_pts_us = packet.pts_us;
                context.media(packet_epoch, packet_id, packet_pts_us);
                if random_access {
                    shared.trace(
                        trace::TraceCategory::Recovery,
                        "keyframe_ingress",
                        Some(identity),
                        serde_json::json!({
                            "channel_generation": generation.get(),
                            "record_sequence": header.sequence,
                            "media_epoch": packet_epoch,
                            "media_id": packet_id,
                            "pts_us": packet_pts_us,
                            "body_bytes": body.len(),
                        }),
                    );
                }
                // A decoder may release multiple reordered frames for one encoded record. Treat
                // every output from the first output-bearing record as part of the same priming
                // unit: the producer cannot observe OUTPUT_READY or issue PLAY until its record
                // write completes. Later records remain paced against the playback clock.
                let priming_record = shared.scene.latest_frame(identity).is_none();
                let body_length = u32::try_from(body.len()).map_err(|_| {
                    ChannelFailure::message(
                        ChannelFailureKind::MediaParse,
                        ErrorKind::InvalidData,
                        "video record exceeds u32",
                        context,
                    )
                })?;
                channel_other!(
                    ChannelFailureKind::MediaAdmission,
                    shared.scene.admit_media(
                        identity,
                        generation,
                        body_length,
                        packet.epoch,
                        packet.packet_id,
                        random_access,
                        header.sequence,
                    )
                );
                let linked_audio =
                    shared.scene.active_track(identity.surface, scene::SLOT_AUDIO).and_then(
                        |audio_identity| lock(&shared.audio_outputs).get(&audio_identity).cloned(),
                    );
                // Keep sound and picture together by delaying the sound.
                //
                // Both are stamped on one capture clock but they do not arrive together: video is
                // roughly two orders of magnitude larger and queues behind itself in the transport,
                // so audio played on arrival runs ahead of the picture it belongs with by exactly
                // that difference. Measuring the residual skew here and holding audio back by it is
                // what makes a live pair presentable; the previous design instead declared the
                // video late and discarded it, which on any link whose video delay exceeded the
                // 100 ms budget discarded every frame there was.
                if configuration.mode == TrackMode::Live
                    && let Some(audio) = &linked_audio
                    && let Some(rendered_pts_us) = audio.rendered_pts()
                {
                    let headroom_us = packet.pts_us.saturating_sub(rendered_pts_us);
                    let least_headroom_us = delay_window_headroom_us
                        .map_or(headroom_us, |least| least.min(headroom_us));
                    delay_window_headroom_us = Some(least_headroom_us);
                    if headroom_us < 0 {
                        audio.request_live_delay(grown_live_delay_us(
                            audio.live_delay_us(),
                            headroom_us,
                        ));
                    } else if delay_review_started.elapsed() >= LIVE_DELAY_REVIEW {
                        // Video has been arriving early for a whole review window, so the delay is
                        // larger than the link now needs and can give some of it back.
                        audio.request_live_delay(shrunk_live_delay_us(
                            audio.live_delay_us(),
                            least_headroom_us,
                        ));
                        delay_review_started = Instant::now();
                        delay_window_headroom_us = None;
                    }
                }
                let discard_before = if priming_record {
                    None
                } else {
                    linked_audio.as_ref().and_then(|audio| {
                        audio.discard_video_before(configuration.maximum_latency_us)
                    })
                };
                let late = discard_before.is_some_and(|deadline| packet.pts_us < deadline);
                // A key frame recovers a *broken* stream. Late media is not broken, and on a
                // saturated link the recovery unit is the largest thing that could be added to the
                // queue that made it late, so the request is spaced as well as deduplicated.
                let requested_keyframe = late
                    && latency_recovery_epoch != Some(packet.epoch)
                    && last_latency_keyframe
                        .is_none_or(|at| at.elapsed() >= LATENCY_KEYFRAME_INTERVAL);
                if requested_keyframe {
                    request_keyframe(
                        writer,
                        shared,
                        identity,
                        generation,
                        NEED_KEYFRAME_DECODER_RESET,
                    );
                    latency_recovery_epoch = Some(packet.epoch);
                    last_latency_keyframe = Some(Instant::now());
                } else if random_access
                    && latency_recovery_epoch.is_some_and(|epoch| packet.epoch > epoch)
                {
                    latency_recovery_epoch = None;
                }
                let frames = video_decoder
                    .as_mut()
                    .ok_or_else(|| {
                        ChannelFailure::message(
                            ChannelFailureKind::RecordType,
                            ErrorKind::InvalidData,
                            "VIDEO_PACKET used a non-video track",
                            context,
                        )
                    })?
                    .push_discarding_before(packet, discard_before);
                let (frames, discarded) = match frames {
                    Ok(frames) => frames,
                    Err(error) => {
                        // Media §13 reason 2: the decoder reset, so a key unit in a greater epoch
                        // recovers the channel rather than losing the track.
                        request_keyframe(
                            writer,
                            shared,
                            identity,
                            generation,
                            NEED_KEYFRAME_DECODER_RESET,
                        );
                        log::debug!("video decode failed, asked for a key unit: {error}");
                        continue;
                    },
                };
                if random_access {
                    shared.trace(
                        trace::TraceCategory::Decode,
                        "keyframe_decoded",
                        Some(identity),
                        serde_json::json!({
                            "channel_generation": generation.get(),
                            "media_epoch": packet_epoch,
                            "media_id": packet_id,
                            "pts_us": packet_pts_us,
                            "decoded_frames": frames.len(),
                            "discarded_frames": discarded,
                        }),
                    );
                }
                if discarded != 0 || requested_keyframe {
                    channel_other!(
                        ChannelFailureKind::SceneState,
                        shared.scene.record_late_video_discard(
                            identity,
                            discarded,
                            requested_keyframe,
                        )
                    );
                }
                for decoded in frames {
                    let (sar_num, sar_den) = match &configuration.kind {
                        KindConfiguration::Video(configuration) => (
                            u32::try_from(configuration.aspect_numerator).unwrap_or(u32::MAX),
                            u32::try_from(configuration.aspect_denominator).unwrap_or(u32::MAX),
                        ),
                        _ => unreachable!(),
                    };
                    channel_io!(
                        ChannelFailureKind::SceneState,
                        wait_until_video_due(
                            shared,
                            identity,
                            decoded.pts_us,
                            priming_record,
                            configuration.mode == TrackMode::Live,
                        )
                    );
                    channel_other!(
                        ChannelFailureKind::SceneState,
                        shared.scene.publish_decoded_frame(
                            identity,
                            generation,
                            Frame {
                                frame_id: packet.packet_id,
                                pts_us: decoded.pts_us,
                                width: decoded.width,
                                height: decoded.height,
                                sar_num,
                                sar_den,
                                alpha_mode: scene::ALPHA_STRAIGHT,
                                rgba: Arc::new(decoded.rgba),
                                damage: None,
                            },
                        )
                    );
                    shared.request_frame_wake();
                }
            },
            messages::AUDIO_PACKET => {
                let packet = channel_other!(
                    ChannelFailureKind::MediaParse,
                    media::parse_audio_packet(&body)
                );
                context.media(packet.epoch, packet.packet_id, packet.pts_us);
                let body_length = u32::try_from(body.len()).map_err(|_| {
                    ChannelFailure::message(
                        ChannelFailureKind::MediaParse,
                        ErrorKind::InvalidData,
                        "audio record exceeds u32",
                        context,
                    )
                })?;
                channel_other!(
                    ChannelFailureKind::MediaAdmission,
                    shared.scene.admit_media(
                        identity,
                        generation,
                        body_length,
                        packet.epoch,
                        packet.packet_id,
                        true,
                        header.sequence,
                    )
                );
                let (output, decoder) = audio.as_mut().ok_or_else(|| {
                    ChannelFailure::message(
                        ChannelFailureKind::RecordType,
                        ErrorKind::InvalidData,
                        "AUDIO_PACKET used a non-audio track",
                        context,
                    )
                })?;
                let mut samples = channel_io!(ChannelFailureKind::Decode, decoder.push(packet));
                // Classify the timeline step before reacting to it. A forward gap is a producer
                // that dropped packets: the timeline is intact and silence of the gap's length
                // keeps the clock honest. Only a backward jump is a restarted clock, and only that
                // justifies discarding every buffered sample. The previous 2 ms tolerance treated
                // ordinary capture jitter as a restart and was audible each time.
                match classify_audio_step(
                    expected_audio_pts_us,
                    packet.pts_us,
                    configuration.mode == TrackMode::Live,
                ) {
                    AudioTimelineStep::Continuous => output.observe_audio_pts(packet.pts_us),
                    AudioTimelineStep::Gap(gap_us) => {
                        output.bridge_live_gap(gap_us);
                        output.observe_audio_pts(packet.pts_us);
                        channel_other!(
                            ChannelFailureKind::SceneState,
                            shared.scene.record_audio_rebase(identity)
                        );
                    },
                    AudioTimelineStep::Restart => {
                        output.rebase_live(packet.pts_us);
                        channel_other!(
                            ChannelFailureKind::SceneState,
                            shared.scene.record_audio_rebase(identity)
                        );
                    },
                }
                expected_audio_pts_us = Some(
                    packet
                        .pts_us
                        .saturating_add(i64::try_from(packet.duration_us).unwrap_or(i64::MAX)),
                );
                output.trim_before_start(packet.pts_us, packet.duration_us, &mut samples);
                channel_io!(ChannelFailureKind::AudioOutput, output.push(&samples));
                channel_other!(
                    ChannelFailureKind::SceneState,
                    shared.scene.mark_output_ready(identity, generation)
                );
            },
            messages::CHANNEL_EOS => {
                let envelope =
                    channel_other!(ChannelFailureKind::MediaParse, messages::decode_control(&body));
                if envelope.request_id != 0 {
                    return Err(ChannelFailure::message(
                        ChannelFailureKind::MediaParse,
                        ErrorKind::InvalidData,
                        "CHANNEL_EOS must be uncorrelated",
                        context,
                    ));
                }
                let value = Value::Map(envelope.payload);
                let eos = channel_other!(
                    ChannelFailureKind::MediaParse,
                    StrictMap::new("CHANNEL_EOS", &value, &[0, 1, 2, 3, 4, 5])
                );
                let eos_epoch = eos
                    .required_u64(4)
                    .ok()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| {
                        ChannelFailure::message(
                            ChannelFailureKind::MediaParse,
                            ErrorKind::InvalidData,
                            "invalid CHANNEL_EOS media epoch",
                            context,
                        )
                    })?;
                context.media(
                    eos_epoch,
                    shared
                        .scene
                        .track_status(identity)
                        .map_or(0, |status| status.state.last_media_id),
                    0,
                );
                if eos.required_u64(0).ok() != Some(identity.surface.context.context_id)
                    || eos.required_u64(1).ok() != Some(identity.surface.surface_id)
                    || eos.required_u64(2).ok() != Some(identity.track_id)
                    || eos.required_u64(3).ok() != Some(generation.get())
                {
                    return Err(ChannelFailure::message(
                        ChannelFailureKind::RecordIdentity,
                        ErrorKind::InvalidData,
                        "CHANNEL_EOS does not name this track generation",
                        context,
                    ));
                }
                let last_record_sequence =
                    channel_other!(ChannelFailureKind::MediaParse, eos.required_u64(5));
                channel_other!(
                    ChannelFailureKind::SceneState,
                    shared.scene.mark_eos(identity, generation, eos_epoch, last_record_sequence,)
                );
                if let Some(decoder) = video_decoder.as_mut() {
                    // CHANNEL_EOS is also one channel record. If draining it produces the first
                    // output, complete that bounded priming unit before waiting for PLAY.
                    let priming_record = shared.scene.latest_frame(identity).is_none();
                    for decoded in channel_io!(ChannelFailureKind::Decode, decoder.finish()) {
                        let (sar_num, sar_den) = match &configuration.kind {
                            KindConfiguration::Video(configuration) => (
                                u32::try_from(configuration.aspect_numerator).unwrap_or(u32::MAX),
                                u32::try_from(configuration.aspect_denominator).unwrap_or(u32::MAX),
                            ),
                            _ => unreachable!(),
                        };
                        channel_io!(
                            ChannelFailureKind::SceneState,
                            wait_until_video_due(
                                shared,
                                identity,
                                decoded.pts_us,
                                priming_record,
                                configuration.mode == TrackMode::Live,
                            )
                        );
                        channel_other!(
                            ChannelFailureKind::SceneState,
                            shared.scene.publish_decoded_frame(
                                identity,
                                generation,
                                Frame {
                                    frame_id: shared
                                        .scene
                                        .track_status(identity)
                                        .map(|status| status.state.last_media_id)
                                        .unwrap_or(0),
                                    pts_us: decoded.pts_us,
                                    width: decoded.width,
                                    height: decoded.height,
                                    sar_num,
                                    sar_den,
                                    alpha_mode: scene::ALPHA_STRAIGHT,
                                    rgba: Arc::new(decoded.rgba),
                                    damage: None,
                                },
                            )
                        );
                        shared.request_frame_wake();
                    }
                    channel_other!(
                        ChannelFailureKind::SceneState,
                        shared.scene.mark_buffered_ended(identity, generation)
                    );
                }
                if let Some((output, decoder)) = audio.as_mut() {
                    let samples = channel_io!(ChannelFailureKind::Decode, decoder.finish());
                    channel_io!(ChannelFailureKind::AudioOutput, output.push(&samples));
                    output.finish_decode();
                    output.signal_eos();
                }
                return Ok(context);
            },
            _ if header.flags & RECORD_OPTIONAL != 0 => {},
            _ => {
                return Err(ChannelFailure::message(
                    ChannelFailureKind::RecordType,
                    ErrorKind::InvalidData,
                    "record is not legal on a track channel",
                    context,
                ));
            },
        }
        if matches!(
            header.record_type,
            messages::VIDEO_PACKET
                | messages::AUDIO_PACKET
                | messages::RASTER_FRAME
                | messages::IMAGE_DATA
        ) {
            let (maximum_bytes, maximum_records) = channel_other!(
                ChannelFailureKind::FlowControl,
                shared.scene.return_channel_capacity(identity, generation, body.len() as u64, 1,)
            );
            let grant = channel_other!(
                ChannelFailureKind::InternalState,
                Envelope::new(
                    0,
                    vec![
                        (0, Value::Unsigned(identity.surface.context.context_id)),
                        (1, Value::Unsigned(identity.surface.surface_id)),
                        (2, Value::Unsigned(identity.track_id)),
                        (3, Value::Unsigned(generation.get())),
                        (4, Value::Unsigned(maximum_bytes)),
                        (5, Value::Unsigned(maximum_records)),
                    ],
                )
                .encode()
            );
            channel_io!(
                ChannelFailureKind::TransportWrite,
                writer.write_record(messages::MAX_CHANNEL_DATA, identity.track_id, &grant,)
            );
            if recovery_unit || last_flow_trace.elapsed() >= Duration::from_millis(250) {
                shared.trace(
                    trace::TraceCategory::Flow,
                    "flow_grant_written",
                    Some(identity),
                    serde_json::json!({
                        "channel_generation": generation.get(),
                        "record_sequence": header.sequence,
                        "maximum_body_bytes": maximum_bytes,
                        "maximum_media_records": maximum_records,
                        "recovery_unit": recovery_unit,
                    }),
                );
                last_flow_trace = Instant::now();
            }
        }
    }
}

fn pace_ingress(
    byte_bucket: &mut TokenBucket,
    record_bucket: &mut TokenBucket,
    last_update: &mut Instant,
    body_bytes: u64,
) -> io::Result<()> {
    loop {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(*last_update);
        *last_update = now;
        byte_bucket.replenish(elapsed).map_err(io::Error::other)?;
        record_bucket.replenish(elapsed).map_err(io::Error::other)?;

        // Transport scheduling can turn a correctly paced producer stream into an arrival burst
        // after SSH, WebTransport, or WebSocket buffering. Shape admission here instead of
        // destroying the track for that transport artifact. Absolute channel flow remains the
        // finite bound and no capacity is returned until this record is reusable.
        //
        // A record larger than a bucket can ever hold is an error rather than something to wait
        // for: sleeping on it would park this track's reader for good.
        let bytes_wait = byte_bucket.time_until(body_bytes).map_err(io::Error::other)?;
        let records_wait = record_bucket.time_until(1).map_err(io::Error::other)?;
        let Some(wait) = bytes_wait.into_iter().chain(records_wait).max() else {
            byte_bucket.charge(body_bytes).map_err(io::Error::other)?;
            record_bucket.charge(1).map_err(io::Error::other)?;
            return Ok(());
        };
        // Sleep the shortfall exactly rather than polling for it. The cap only bounds how long a
        // single sleep can be, so a track being torn down is not waited out in one go.
        thread::sleep(wait.min(MAXIMUM_PACING_SLEEP));
    }
}

fn wait_until_video_due(
    shared: &Arc<ServiceShared>,
    identity: TrackIdentity,
    pts_us: i64,
    priming_record: bool,
    live: bool,
) -> io::Result<()> {
    if priming_record {
        return shared.scene.wait_until_due(identity, pts_us, true).map_err(io::Error::other);
    }
    // Live video is already paced by the capture that produced it, so there is nothing to wait
    // for — and this runs on the thread that reads the channel. Sleeping here stops the socket
    // from being drained and stops channel capacity from being returned, which backs the producer
    // up until *its* queue overflows: the pacing wait manufactures the congestion it is reacting
    // to. Timed media, which can be delivered far faster than real time, still needs the clock.
    if live {
        return Ok(());
    }
    loop {
        let audio = shared
            .scene
            .active_track(identity.surface, scene::SLOT_AUDIO)
            .and_then(|identity| lock(&shared.audio_outputs).get(&identity).cloned());
        let Some(audio) = audio else {
            return shared.scene.wait_until_due(identity, pts_us, false).map_err(io::Error::other);
        };
        if audio.pts_reached(pts_us) {
            return Ok(());
        }
        if audio.video_gate_stalled() {
            return shared.scene.wait_until_due(identity, pts_us, false).map_err(io::Error::other);
        }
        // The audio device renders at a fixed rate, so how long this frame is early by is
        // arithmetic. Sleep that instead of polling: the alternative, waking this track's reader
        // every two milliseconds for the length of the wait, costs the same whether the frame is
        // due in one millisecond or one second. The cap keeps the stall check and a disappearing
        // audio track responsive.
        let wait = audio
            .time_until_pts(pts_us)
            .unwrap_or(LINKED_AUDIO_RECHECK)
            .clamp(MINIMUM_LINKED_AUDIO_WAIT, LINKED_AUDIO_RECHECK);
        thread::sleep(wait);
    }
}

/// Serve one interactive-lane connection.
///
/// Core §7: the lane is authenticated with a tag over the session channel key, carries only
/// input and liveness records, and its loss revokes input without disturbing the control session,
/// surfaces, or tracks.
fn handle_lane(
    reader: &mut Reader,
    shared: &Arc<ServiceShared>,
    pending: &PendingConnection,
) -> io::Result<()> {
    let writer = Arc::new(reader.writer(ConnectionKind::Lane)?);
    reader.set_maximum(LANE_MAX_RECORD_BODY)?;
    writer.set_maximum(LANE_MAX_RECORD_BODY)?;
    let first = reader.read_record(ConnectionKind::Lane)?;
    if first.record_type != messages::LANE_OPEN {
        return Err(send_fatal(
            &writer,
            0,
            messages::ERROR_BAD_MESSAGE,
            "an interactive lane opens with LANE_OPEN",
        ));
    }
    let request_id =
        messages::decode_control(&first.body).map(|envelope| envelope.request_id).unwrap_or(0);
    // The decoder enforces the interactive lane class and every nonzero and length rule.
    let Ok(open) = messages::LaneOpen::decode(&first.body) else {
        return Err(send_fatal(
            &writer,
            request_id,
            messages::ERROR_BAD_MESSAGE,
            "invalid LANE_OPEN",
        ));
    };

    let Some(session) = lock(&shared.registry).sessions.get(&open.session_id).cloned() else {
        return Err(send_fatal(
            &writer,
            request_id,
            messages::ERROR_AUTH_FAILED,
            "lane authentication failed",
        ));
    };
    let expected = auth::lane_tag(
        session.channel_key.expose(),
        open.session_id,
        LaneClass::Interactive as u32,
        open.lane_generation,
        &open.client_nonce,
    );
    if !auth::verify_tag(&expected, &open.authentication_tag) {
        return Err(send_fatal(
            &writer,
            request_id,
            messages::ERROR_AUTH_FAILED,
            "lane authentication failed",
        ));
    }
    // The lane tag is proved. An interactive lane is quiet by nature — it carries input the
    // presenter sends when the user acts — so it must not be deadlined from here.
    pending.authenticated(reader)?;

    let admission = {
        let mut slot = lock(&session.lane);
        lane::admit(&mut slot, open.lane_generation, open.client_nonce)
    };
    match admission {
        lane::Admission::Accept | lane::Admission::Replay => {},
        lane::Admission::Busy => {
            return Err(send_fatal(
                &writer,
                request_id,
                messages::ERROR_CHANNEL_BUSY,
                "another interactive transport is live for this lane generation",
            ));
        },
        lane::Admission::Refused => {
            return Err(send_fatal(
                &writer,
                request_id,
                messages::ERROR_BAD_STATE,
                "this lane generation cannot be reopened",
            ));
        },
    }

    let mut cleanup = LaneCleanup {
        session: session.clone(),
        generation: open.lane_generation,
        writer: writer.clone(),
        egress: None,
    };
    let egress = Egress::start(writer.clone(), "vivid-lane-egress")?;
    cleanup.egress = Some(egress.clone());

    let accepted = Envelope::new(
        request_id,
        vec![
            (0, Value::Unsigned(open.session_id)),
            (1, Value::Unsigned(LaneClass::Interactive as u64)),
            (2, Value::Unsigned(open.lane_generation)),
            (3, Value::Unsigned(u64::from(LANE_MAX_RECORD_BODY))),
        ],
    )
    .encode()
    .map_err(io::Error::other)?;
    writer.write_record(messages::LANE_ACCEPTED, 0, &accepted)?;

    // From here the lane is reader plus egress, exactly as the control connection is. Nothing that
    // originates outside this thread — an input event from the window, a revocation, a renewal —
    // touches the socket, and this thread's own replies do not queue behind them either.
    egress.set_shutdown(reader.shutdown_handle()?);
    *lock(&session.lane_writer) = Some(writer.clone());
    *lock(&session.lane_egress) = Some(egress.clone());

    let outcome = serve_lane(reader, &writer, &session, &shared.scene);
    drop(cleanup);
    outcome
}

/// Answer one `SET_INPUT_BINDING` under the presenter's current eligibility.
fn apply_input_binding(
    scene: &SharedScene,
    session: &Arc<SessionRuntime>,
    binding: &InputBinding,
) -> io::Result<Vec<u8>> {
    let eligibility = input_eligibility(scene, session, binding);
    let mut grant = lock(&session.grant);
    let outcome = grant.apply(binding, &eligibility, clock::now());
    if outcome.is_err() {
        // A lower epoch, or the same epoch with different bytes, is `BAD_STATE` (desktop §5.1).
        return protocol_error(
            0,
            messages::ERROR_BAD_STATE,
            false,
            "input binding epoch is stale or inconsistent",
        );
    }
    drop(grant);
    session.wake_actor();
    let grant = lock(&session.grant);
    Envelope::new(0, grant.bound_payload(binding.producer_epoch.get()))
        .encode()
        .map_err(io::Error::other)
}

/// Gather the presenter's eligibility for a binding, desktop §5.1.
fn input_eligibility(
    scene: &SharedScene,
    session: &Arc<SessionRuntime>,
    binding: &InputBinding,
) -> Eligibility {
    let identity = session
        .identity
        .context(binding.context_id)
        .ok()
        .and_then(|context| context.surface(binding.surface_id).ok());
    let status = identity.and_then(|identity| scene.surface_status(identity));
    let capability_mask =
        status.as_ref().and_then(|status| desktop_capability_mask(&status.definition)).unwrap_or(0);
    let presented = identity.is_some_and(|identity| scene.surface_has_presented(identity));
    let may_receive_input = lock(&session.contexts)
        .get(&binding.context_id)
        .is_some_and(|context| context.operation_classes & OP_DESKTOP_INPUT != 0);
    Eligibility {
        // Vivido has no separate consent UI yet, and a window that is presenting is the focus
        // signal it has; both become real inputs when the desktop window mode grows a UI.
        focused: true,
        consented: true,
        surface_present: status.as_ref().is_some_and(|status| status.lifecycle == 1),
        surface_generation: status
            .as_ref()
            .map(|status| status.generation)
            .unwrap_or(SurfaceGeneration::ZERO),
        capability_mask,
        presented,
        lane_live: lock(&session.lane).is_some_and(|state| state.live()),
        may_receive_input,
    }
}

/// The input capability mask a `desktop-content-v1` surface declared, desktop §2 key 4.
fn desktop_capability_mask(definition: &SurfaceDefinition) -> Option<u64> {
    if definition.semantic_profile != registry::DESKTOP_CONTENT {
        return None;
    }
    DesktopSurfaceParameters::decode(&definition.profile_parameters)
        .ok()
        .map(|parameters| parameters.input_capabilities)
}

/// Revoke a session's grant and tell the producer, if there was one to revoke.
fn revoke_input(session: &Arc<SessionRuntime>, reason: u64) {
    let payload = lock(&session.grant).revoke(reason);
    let Some(payload) = payload else {
        return;
    };
    let surface_id =
        payload.iter().find(|entry| entry.0 == 3).and_then(|entry| entry.1.as_u64()).unwrap_or(0);
    // Focus loss revokes from the winit UI thread; lane loss revokes from the session actor.
    // Neither may block, and the grant above is already revoked locally either way, so a producer
    // that is not reading cannot keep a stale grant alive.
    if let Ok(body) = Envelope::new(0, payload).encode() {
        session.post_lane(messages::INPUT_REVOKED, surface_id, body);
    }
}

/// Send any renewal that has fallen due, desktop §6.
fn service_input_renewal(session: &Arc<SessionRuntime>) {
    let renewal = {
        let mut grant = lock(&session.grant);
        grant.due_renewal(clock::now()).and_then(|renewal| grant.renewal_payload(renewal))
    };
    let Some(payload) = renewal else {
        return;
    };
    let surface_id =
        payload.iter().find(|entry| entry.0 == 3).and_then(|entry| entry.1.as_u64()).unwrap_or(0);
    // Runs on the session actor's tick; a blocking write here would stall control dispatch.
    if let Ok(body) = Envelope::new(0, payload).encode() {
        session.post_lane(messages::INPUT_LEASE_RENEW, surface_id, body);
    }
}

fn serve_lane(
    reader: &mut Reader,
    writer: &Arc<Writer>,
    session: &Arc<SessionRuntime>,
    scene: &SharedScene,
) -> io::Result<()> {
    loop {
        let record = match reader.read_record(ConnectionKind::Lane) {
            Ok(record) => record,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        if !lane::carries(record.record_type) {
            return Err(send_fatal(
                writer,
                0,
                messages::ERROR_BAD_MESSAGE,
                "the interactive lane carries only input and liveness records",
            ));
        }
        match record.record_type {
            messages::PING => {
                let envelope = messages::decode_control(&record.body)?;
                let body = Envelope::new(envelope.request_id, envelope.payload)
                    .encode()
                    .map_err(io::Error::other)?;
                // Core §7 wants liveness answered promptly. Queue it so a producer that has
                // stopped reading cannot stall the thread that would answer the next one.
                session.post_lane(messages::PONG, 0, body);
            },
            messages::PONG | messages::ERROR => {},
            messages::SET_INPUT_BINDING => {
                let envelope = messages::decode_control(&record.body)?;
                let Ok(binding) =
                    InputBinding::decode(record.object_id, &Value::Map(envelope.payload))
                else {
                    return Err(send_fatal(
                        writer,
                        envelope.request_id,
                        messages::ERROR_BAD_MESSAGE,
                        "invalid SET_INPUT_BINDING",
                    ));
                };
                let body = apply_input_binding(scene, session, &binding)?;
                session.post_lane(messages::INPUT_BOUND, binding.surface_id, body);
            },
            _ => {
                // Ordinary input events travel presenter-to-producer, so a producer sending one is
                // a protocol error rather than something to translate.
                return Err(send_fatal(
                    writer,
                    0,
                    messages::ERROR_BAD_MESSAGE,
                    "input events travel from the presenter",
                ));
            },
        }
    }
}

fn surface_ready_payload(status: &SurfaceStatus) -> Vec<(u64, Value)> {
    vec![
        (0, Value::Unsigned(status.identity.context.context_id)),
        (1, Value::Unsigned(status.identity.surface_id)),
        (2, Value::Unsigned(status.revision.get())),
        (3, Value::Unsigned(status.generation.get())),
        (4, Value::Unsigned(status.definition.policy)),
        (5, Value::Map(status.definition.profile_parameters.clone())),
    ]
}

fn surface_status_payload(status: &SurfaceStatus) -> Vec<(u64, Value)> {
    vec![
        (0, Value::Unsigned(status.identity.context.context_id)),
        (1, Value::Unsigned(status.identity.surface_id)),
        (2, Value::Unsigned(status.revision.get())),
        (3, Value::Unsigned(status.generation.get())),
        (4, Value::Text(status.definition.semantic_profile.clone())),
        (5, Value::Unsigned(status.definition.coordinate_model as u64)),
        (6, Value::Unsigned(status.definition.logical_width)),
        (7, Value::Unsigned(status.definition.logical_height)),
        (8, Value::Unsigned(status.definition.scale_numerator)),
        (9, Value::Unsigned(status.definition.scale_denominator)),
        (10, Value::Unsigned(u64::from(status.definition.rotation))),
        (11, status.definition.descriptor.to_value().unwrap_or(Value::Map(vec![]))),
        (12, Value::Unsigned(status.definition.policy)),
        (
            13,
            Value::Map(
                status
                    .active_slots
                    .iter()
                    .map(|(slot, track)| (*slot, Value::Unsigned(*track)))
                    .collect(),
            ),
        ),
        (14, Value::Unsigned(status.lifecycle)),
        (15, Value::Map(status.definition.profile_parameters.clone())),
    ]
}

fn track_status_payload(status: &TrackStatus, include_audio_gain: bool) -> Vec<(u64, Value)> {
    let mut payload = vec![
        (0, Value::Unsigned(status.identity.surface.context.context_id)),
        (1, Value::Unsigned(status.identity.surface.surface_id)),
        (2, Value::Unsigned(status.identity.track_id)),
        (3, Value::Unsigned(status.state.revision.get())),
        (4, Value::Unsigned(status.configuration.kind.kind() as u64)),
        (5, Value::Unsigned(status.configuration.mode as u64)),
        (6, Value::Unsigned(status.lifecycle)),
        (7, Value::Unsigned(status.state.channel_generation.get())),
        (
            8,
            Value::Unsigned(
                if status.lifecycle == 6
                    || status.state.milestones & vivid_protocol::track::MILESTONE_CHANNEL_DETACHED
                        != 0
                {
                    2
                } else if status.state.milestones
                    & vivid_protocol::track::MILESTONE_CHANNEL_ACCEPTED
                    != 0
                {
                    1
                } else {
                    0
                },
            ),
        ),
        (9, Value::Unsigned(status.state.milestones)),
        (10, Value::Unsigned(u64::from(status.state.media_epoch))),
        (11, Value::Unsigned(status.state.last_media_id)),
        (12, Value::Unsigned(status.last_media_record_sequence)),
        (13, signed(status.last_decoded_pts_us.unwrap_or(0))),
        (14, signed(status.last_presented_pts_us.unwrap_or(0))),
        (15, Value::Unsigned(status.last_presentation_id)),
        (16, Value::Unsigned(status.state.flow.sent_body_bytes)),
        (17, Value::Unsigned(status.state.flow.sent_media_records)),
        (18, Value::Unsigned(status.maximum_channel_bytes)),
        (19, Value::Unsigned(status.maximum_channel_records)),
        (20, Value::Unsigned(0)),
    ];
    if include_audio_gain && matches!(status.configuration.kind, KindConfiguration::Audio(_)) {
        payload.push((23, Value::Unsigned(status.audio_gain.raw())));
    }
    payload
}

fn scene_status_payload(status: &SceneStatus) -> Vec<(u64, Value)> {
    vec![
        (0, Value::Unsigned(status.revision.get())),
        (1, Value::Unsigned(status.target_generation.get())),
        (
            2,
            Value::Array(
                status
                    .nodes
                    .iter()
                    .filter_map(|node| node.node.payload().ok().map(Value::Map))
                    .collect(),
            ),
        ),
        (3, Value::Unsigned(status.nodes.len() as u64)),
    ]
}

pub(crate) fn wait_satisfied_body(
    request_id: u64,
    identity: TrackIdentity,
    condition: u64,
    satisfied: TrackWaitSatisfied,
) -> Vec<u8> {
    Envelope::new(request_id, wait_satisfied_payload(identity, condition, satisfied))
        .encode()
        .unwrap_or_else(|_| messages::ok(request_id))
}

fn wait_satisfied_payload(
    identity: TrackIdentity,
    condition: u64,
    satisfied: TrackWaitSatisfied,
) -> Vec<(u64, Value)> {
    vec![
        (0, Value::Unsigned(identity.surface.context.context_id)),
        (1, Value::Unsigned(identity.surface.surface_id)),
        (2, Value::Unsigned(identity.track_id)),
        (3, Value::Unsigned(satisfied.revision.get())),
        (4, Value::Unsigned(satisfied.channel_generation.get())),
        (5, Value::Unsigned(condition)),
        (6, Value::Unsigned(satisfied.observed_value)),
    ]
}

fn supports_track(configuration: &TrackConfiguration) -> bool {
    configuration.slot <= scene::SLOT_POSTER
        && configuration.slot != 0
        && match (&configuration.kind, configuration.slot) {
            (KindConfiguration::Video(video), scene::SLOT_PRIMARY_VIDEO) => {
                Decoder::new(video, configuration.mode).is_ok()
            },
            (KindConfiguration::Audio(audio), scene::SLOT_AUDIO) => supports_audio(audio),
            (KindConfiguration::Raster(_), scene::SLOT_RASTER | scene::SLOT_POSTER)
            | (KindConfiguration::EncodedImage(_), scene::SLOT_POSTER) => true,
            _ => false,
        }
}

fn payload_surface_identity(
    session: &SessionRuntime,
    value: &Value,
) -> Result<SurfaceIdentity, ControlError> {
    let map = StrictMap::new("surface identity", value, &[0, 1])
        .map_err(|_| ControlError::bad_message("invalid surface identity"))?;
    surface_identity(
        session,
        map.required_u64(0).map_err(|_| ControlError::bad_message("context ID"))?,
        map.required_u64(1).map_err(|_| ControlError::bad_message("surface ID"))?,
    )
}

fn payload_track_identity(
    session: &SessionRuntime,
    value: &Value,
) -> Result<TrackIdentity, ControlError> {
    let Value::Map(map) = value else {
        return Err(ControlError::bad_message("track identity is not a map"));
    };
    let required = |key| {
        map.iter()
            .find(|entry| entry.0 == key)
            .and_then(|entry| entry.1.as_u64())
            .ok_or_else(|| ControlError::bad_message("invalid track identity"))
    };
    track_identity(session, required(0)?, required(1)?, required(2)?)
}

fn context_identity(
    session: &SessionRuntime,
    context_id: u64,
) -> Result<ContextIdentity, ControlError> {
    require_context(session, context_id)?;
    session
        .identity
        .context(context_id)
        .map_err(|_| ControlError::bad_message("context ID is zero"))
}

fn surface_identity(
    session: &SessionRuntime,
    context_id: u64,
    surface_id: u64,
) -> Result<SurfaceIdentity, ControlError> {
    context_identity(session, context_id)?
        .surface(surface_id)
        .map_err(|_| ControlError::bad_message("surface ID is zero"))
}

fn track_identity(
    session: &SessionRuntime,
    context_id: u64,
    surface_id: u64,
    track_id: u64,
) -> Result<TrackIdentity, ControlError> {
    surface_identity(session, context_id, surface_id)?
        .track(track_id)
        .map_err(|_| ControlError::bad_message("track ID is zero"))
}

fn require_context(session: &SessionRuntime, context_id: u64) -> Result<(), ControlError> {
    if context_id != 0 && lock(&session.contexts).contains_key(&context_id) {
        Ok(())
    } else {
        Err(ControlError::not_found("context is outside this session authority"))
    }
}

fn require_context_operation(
    session: &SessionRuntime,
    context_id: u64,
    operation: u64,
) -> Result<(), ControlError> {
    if context_id != 0
        && lock(&session.contexts)
            .get(&context_id)
            .is_some_and(|context| context.operation_classes & operation != 0)
    {
        Ok(())
    } else {
        Err(ControlError::not_found("context is outside this session operation authority"))
    }
}

struct ControlError {
    code: u64,
    message: &'static str,
    track: Option<TrackIdentity>,
    trace_rejection: bool,
}

impl ControlError {
    const fn bad_message(message: &'static str) -> Self {
        Self { code: messages::ERROR_BAD_MESSAGE, message, track: None, trace_rejection: true }
    }

    const fn bad_state(message: &'static str) -> Self {
        Self { code: messages::ERROR_BAD_STATE, message, track: None, trace_rejection: true }
    }

    const fn state(message: &'static str) -> Self {
        Self::bad_state(message)
    }

    const fn not_found(message: &'static str) -> Self {
        Self { code: messages::ERROR_NOT_FOUND, message, track: None, trace_rejection: true }
    }

    const fn precondition(message: &'static str) -> Self {
        Self {
            code: messages::ERROR_PRECONDITION_FAILED,
            message,
            track: None,
            trace_rejection: true,
        }
    }

    const fn stale_target() -> Self {
        Self {
            code: registry::error::STALE_TARGET_GENERATION,
            message: "stale target generation",
            track: None,
            trace_rejection: true,
        }
    }

    const fn unsupported(message: &'static str) -> Self {
        Self {
            code: messages::ERROR_UNSUPPORTED_CONFIG,
            message,
            track: None,
            trace_rejection: true,
        }
    }

    const fn unsupported_profile(message: &'static str) -> Self {
        Self {
            code: messages::ERROR_UNSUPPORTED_PROFILE,
            message,
            track: None,
            trace_rejection: true,
        }
    }

    const fn duplicate(message: &'static str) -> Self {
        Self { code: messages::ERROR_DUPLICATE_ID, message, track: None, trace_rejection: true }
    }

    const fn limit(message: &'static str) -> Self {
        Self { code: messages::ERROR_LIMIT_EXCEEDED, message, track: None, trace_rejection: true }
    }

    fn with_track(mut self, track: TrackIdentity) -> Self {
        self.track = Some(track);
        self
    }

    fn without_rejection_trace(mut self) -> Self {
        self.trace_rejection = false;
        self
    }
}

fn send_fatal(writer: &Writer, request_id: u64, code: u64, diagnostic: &str) -> io::Error {
    let body = protocol_error(request_id, code, true, diagnostic);
    if let Ok(body) = body {
        let _ = writer.write_record(messages::ERROR, 0, &body);
    }
    io::Error::new(ErrorKind::InvalidData, diagnostic.to_owned())
}

pub(crate) fn protocol_error(
    request_id: u64,
    code: u64,
    fatal: bool,
    diagnostic: impl Into<String>,
) -> io::Result<Vec<u8>> {
    ErrorReply {
        code,
        request_id,
        detail: ErrorDetail::new(vec![]).map_err(io::Error::other)?,
        fatal,
        diagnostic: diagnostic.into(),
    }
    .encode()
    .map_err(io::Error::other)
}

/// Reduce a session contract to what a browser carrier can actually deliver, web §5.2.
///
/// Applied only when the session negotiated `web-carrier-v1`: the WELCOME then advertises the
/// web ceilings, and admission enforces them, so a bridge between this presenter and a browser
/// never has to close a track the presenter claimed it could carry.
fn clamp_contract_for_web(contract: &mut ResourceContract) {
    for (resource, ceiling) in [
        (Resource::ControlRecordBody, u64::from(vivid_protocol::web::MAX_CONTROL_RECORD_BODY)),
        (Resource::MediaRecordBody, u64::from(vivid_protocol::web::MAX_MEDIA_RECORD_BODY)),
        (Resource::InflightMediaBytes, vivid_protocol::web::MAX_AGGREGATE_REASSEMBLY),
    ] {
        contract.set(resource, contract.get(resource).min(ceiling));
    }
}

fn presenter_contract() -> ResourceContract {
    let mut contract = ResourceContract::denied();
    for (resource, value) in [
        (Resource::Surfaces, 64),
        (Resource::Tracks, 256),
        (Resource::Nodes, MAX_SCENE_NODES as u64),
        (Resource::VideoTracks, 32),
        (Resource::AudioTracks, 32),
        (Resource::RasterTracks, 64),
        (Resource::ImageTracks, 64),
        (Resource::DecoderInstances, 64),
        (Resource::CodedPixelsPerTrack, 8192 * 8192),
        (Resource::DecodedPixelsPerSecond, 8192 * 8192 * 60),
        (Resource::EncodedBitsPerSecond, 1_000_000_000),
        (Resource::MediaRecordsPerSecond, 4000),
        (Resource::AudioSampleRate, 192_000),
        (Resource::AudioChannelsPerTrack, 32),
        (Resource::InflightMediaBytes, 256 * 1024 * 1024),
        (Resource::TrackConnections, MAX_CONNECTIONS as u64),
        (Resource::RetainedPixels, 8192 * 8192 * 2),
        (Resource::MediaRecordBody, u64::from(vivid_protocol::HARD_MAX_RECORD_BODY)),
        (Resource::ControlRecordBody, u64::from(vivid_protocol::CONTROL_MAX_RECORD_BODY)),
        (Resource::PendingRequests, 256),
        (Resource::RegisteredWaits, 64),
        (Resource::IdempotencyEntries, 256),
        (Resource::ChildSessionLeases, 0),
        (Resource::DisconnectGraceUs, 0),
        (Resource::ObservationQueueEntries, 64),
        (Resource::ImageCacheBytes, 64 * 1024 * 1024),
        (Resource::OpenSceneTransactions, 64),
        (Resource::ChildContexts, 64),
        (Resource::PendingChannelOpenAttempts, 64),
        (Resource::ActiveTerminalAnchors, MAX_ACTIVE_ANCHORS as u64),
        (Resource::SeenTerminalAnchorIds, MAX_SEEN_ANCHORS as u64),
    ] {
        contract.set(resource, value);
    }
    contract
}

/// Encode one `TARGET_CHANGED` body for the given target, announcement or re-announcement alike.
fn target_change_body(change: &crate::vivid::target::TargetChange) -> Vec<u8> {
    let mut payload = change.descriptor.clone();
    payload.push((9, Value::Unsigned(change.generation)));
    payload.push((10, Value::Unsigned(change.reason)));
    Envelope::new(0, payload).encode().expect("target-change payload is valid")
}

fn image_format(encoding: u64) -> io::Result<image::ImageFormat> {
    match encoding {
        1 => Ok(image::ImageFormat::Png),
        2 => Ok(image::ImageFormat::Jpeg),
        _ => Err(io::Error::new(ErrorKind::Unsupported, "unsupported encoded-image format")),
    }
}

fn apply_raster_delta(base: &Frame, delta: media::ParsedRasterDeltaFrame<'_>) -> io::Result<Frame> {
    if base.frame_id != delta.base_frame_id {
        return Err(io::Error::new(ErrorKind::InvalidData, "raster delta base frame is stale"));
    }
    let mut rgba = base.rgba.to_vec();
    let mut damage = Vec::with_capacity(delta.operations.len());
    for operation in delta.operations {
        match operation {
            media::ParsedRasterDeltaOperation::Overwrite {
                x,
                y,
                width,
                height,
                rgba: replacement,
            } => {
                for row in 0..height {
                    let destination = ((y + row) as usize * base.width as usize + x as usize) * 4;
                    let source = row as usize * width as usize * 4;
                    let length = width as usize * 4;
                    rgba[destination..destination + length]
                        .copy_from_slice(&replacement[source..source + length]);
                }
                damage.push(scene::RasterDamageRect { x, y, width, height });
            },
            media::ParsedRasterDeltaOperation::Copy {
                destination_x,
                destination_y,
                width,
                height,
                source_x,
                source_y,
            } => {
                copy_raster_rect(
                    &mut rgba,
                    base.width,
                    source_x,
                    source_y,
                    destination_x,
                    destination_y,
                    width,
                    height,
                );
                damage.push(scene::RasterDamageRect {
                    x: destination_x,
                    y: destination_y,
                    width,
                    height,
                });
            },
        }
    }
    Ok(Frame {
        frame_id: delta.frame_id,
        pts_us: delta.pts_us,
        width: base.width,
        height: base.height,
        sar_num: base.sar_num,
        sar_den: base.sar_den,
        alpha_mode: base.alpha_mode,
        rgba: Arc::new(RgbaBuffer::new(rgba)),
        damage: Some(Arc::from(damage)),
    })
}

#[allow(clippy::too_many_arguments)]
fn copy_raster_rect(
    rgba: &mut [u8],
    frame_width: u32,
    source_x: u32,
    source_y: u32,
    destination_x: u32,
    destination_y: u32,
    width: u32,
    height: u32,
) {
    let stride = frame_width as usize * 4;
    let row_bytes = width as usize * 4;
    let copy_row = |rgba: &mut [u8], row: u32| {
        let source = (source_y + row) as usize * stride + source_x as usize * 4;
        let destination = (destination_y + row) as usize * stride + destination_x as usize * 4;
        rgba.copy_within(source..source + row_bytes, destination);
    };
    if destination_y > source_y {
        for row in (0..height).rev() {
            copy_row(rgba, row);
        }
    } else {
        for row in 0..height {
            copy_row(rgba, row);
        }
    }
}

fn signed(value: i64) -> Value {
    if value >= 0 { Value::Unsigned(value as u64) } else { Value::Negative(value) }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0xf) as usize] as char);
    }
    output
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn stop_failed_audio_output(
    outputs: &Mutex<HashMap<TrackIdentity, Arc<AudioOutput>>>,
    identity: TrackIdentity,
) {
    if let Some(output) = lock(outputs).remove(&identity) {
        output.stop();
    }
}

#[cfg(unix)]
fn bind_local_listener() -> io::Result<(LocalListener, String, Option<TempDir>)> {
    let directory = tempfile::Builder::new().prefix("vivido-vivid-1.5-").tempdir()?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
    let path = directory.path().join("control.sock");
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok((listener, format!("unix:{}", path.display()), Some(directory)))
}

#[cfg(windows)]
fn bind_local_listener() -> io::Result<(LocalListener, String, Option<TempDir>)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    Ok((listener, format!("tcp:{address}"), None))
}

fn accept_stream(listener: &LocalListener) -> io::Result<LocalStream> {
    listener.accept().map(|(stream, _)| stream)
}

#[cfg(unix)]
fn wake_listener(endpoint: &str) {
    if let Some(path) = endpoint.strip_prefix("unix:") {
        let _ = UnixStream::connect(path);
    }
}

#[cfg(windows)]
fn wake_listener(endpoint: &str) {
    if let Some(address) = endpoint.strip_prefix("tcp:") {
        let _ = TcpStream::connect(address);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::messages::LaneClass;
    use vivid_protocol::track::{KindConfiguration, TrackConfiguration, TrackMode};

    macro_rules! socket_service {
        ($service:expr) => {
            match $service {
                Ok(service) => service,
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    eprintln!(
                        "skipping socket integration test: this runner forbids local socket binds"
                    );
                    return;
                },
                Err(error) => panic!("could not start test presenter: {error}"),
            }
        };
    }
    use vivid_sdk::{
        CoordinateModel, Fit, MILESTONE_OUTPUT_READY, MILESTONE_PRESENTED, ProducerAuthentication,
        ProducerConfig, RasterConfiguration, RequestMetadata, SceneNode, SessionEvent, SlotBinding,
        SurfaceDefinition, SurfaceDescriptor, SurfaceRole, TrackWaitCondition,
    };

    fn test_geometry() -> DisplayGeometry {
        DisplayGeometry {
            viewport_width: 800,
            viewport_height: 600,
            columns: 80,
            rows: 24,
            cell_width: 10,
            cell_height: 25,
        }
    }

    fn connect(service: &VividService) -> vivid_sdk::Session {
        vivid_sdk::Session::connect(ProducerConfig {
            endpoint_control: Some(service.control_endpoint().to_owned()),
            authentication: ProducerAuthentication::root_hex(service.root_secret()).unwrap(),
            ..ProducerConfig::default()
        })
        .unwrap()
    }

    /// Read the columns, rows, and settled flag out of a terminal target descriptor.
    fn descriptor_summary(descriptor: &[(u64, Value)]) -> (u64, u64, bool) {
        (
            descriptor[2].1.as_u64().unwrap(),
            descriptor[3].1.as_u64().unwrap(),
            descriptor[6].1.as_bool().unwrap(),
        )
    }

    #[test]
    fn frame_wakes_are_coalesced_until_the_event_is_acknowledged() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counted = wakes.clone();
        let service = socket_service!(VividService::start_with_wake(
            test_geometry(),
            Arc::new(move || {
                counted.fetch_add(1, Ordering::AcqRel);
            }),
        ));
        service.shared.request_frame_wake();
        service.shared.request_frame_wake();
        assert_eq!(wakes.load(Ordering::Acquire), 1);
        service.acknowledge_frame_wake();
        service.shared.request_frame_wake();
        assert_eq!(wakes.load(Ordering::Acquire), 2);
    }

    #[test]
    fn an_idle_actor_has_no_periodic_timeout_service() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {}),));
        let session = connect(&service);
        thread::sleep(Duration::from_millis(25));
        assert_eq!(
            service.automation_streaming_metrics()["actor_timeout_services"],
            serde_json::json!(0)
        );
        drop(session);
    }

    #[test]
    fn actor_timeout_policy_selects_the_earliest_responsibility() {
        assert_eq!(
            select_actor_wait_timeout(
                Some(Duration::from_millis(2)),
                Some(Duration::from_millis(9)),
                Some(Duration::from_millis(5)),
            ),
            Some(Duration::from_millis(2))
        );
        assert_eq!(
            select_actor_wait_timeout(None, Some(Duration::from_millis(9)), None),
            Some(Duration::from_millis(9))
        );
        assert_eq!(select_actor_wait_timeout(None, None, None), None);
    }

    #[test]
    fn channel_failure_metadata_is_typed_sanitized_and_bounded() {
        let mut context = ChannelRecordContext::default();
        context.begin_record(messages::VIDEO_PACKET, 9, 4_096);
        context.media(3, 17, 42_000);
        let diagnostic = format!("stale\n{}", "é".repeat(1_024));
        let failure = ChannelFailure::io(
            ChannelFailureKind::MediaAdmission,
            io::Error::other(diagnostic),
            context,
        );
        let encoded = failure.json();
        let diagnostic = encoded["diagnostic"].as_str().unwrap();
        assert!(diagnostic.len() <= 1_024);
        assert!(!diagnostic.chars().any(char::is_control));
        assert_eq!(encoded["kind"], serde_json::json!("media_admission"));
        assert_eq!(encoded["error_code"], serde_json::json!(messages::ERROR_FLOW_CONTROL));
        assert_eq!(context.json()["record_sequence"], serde_json::json!(9));

        let device = ChannelFailure::io(
            ChannelFailureKind::AudioOutput,
            io::Error::other("device stopped"),
            context,
        );
        assert_eq!(device.protocol_error_code(), messages::ERROR_DEVICE_LOST);
    }

    #[test]
    fn raster_copies_match_snapshot_semantics_for_every_overlap_direction() {
        let cases = [(0, 0, 1, 0), (1, 0, 0, 0), (0, 0, 0, 1), (0, 1, 0, 0), (0, 0, 2, 2)];
        for (source_x, source_y, destination_x, destination_y) in cases {
            let mut actual = (0_u8..64).collect::<Vec<_>>();
            let original = actual.clone();
            copy_raster_rect(
                &mut actual,
                4,
                source_x,
                source_y,
                destination_x,
                destination_y,
                2,
                2,
            );
            let mut expected = original.clone();
            for row in 0..2 {
                let source = ((source_y + row) * 4 + source_x) as usize * 4;
                let destination = ((destination_y + row) * 4 + destination_x) as usize * 4;
                expected[destination..destination + 8]
                    .copy_from_slice(&original[source..source + 8]);
            }
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn live_delay_is_scoped_to_its_owner_when_two_sessions_reuse_a_track_id() {
        // Two surfaces streaming at once will normally both allocate track 7. The delay one
        // session needs for its link must not become the delay the other plays with, so the
        // outputs are keyed by complete identity and never by the numeric track ID.
        let identity = |presenter: u8| {
            SessionIdentity::new(PresenterInstanceId([presenter; 16]), 1)
                .unwrap()
                .context(1)
                .unwrap()
                .surface(1)
                .unwrap()
                .track(7)
                .unwrap()
        };
        let (first, second) = (identity(1), identity(2));
        assert_ne!(first, second, "identity must distinguish the two owners");

        let outputs = HashMap::from([
            (first, AudioOutput::test_output()),
            (second, AudioOutput::test_output()),
        ]);

        outputs[&first].observe_audio_pts(1_000_000);
        outputs[&second].observe_audio_pts(1_000_000);
        outputs[&first].request_live_delay(400_000);

        assert_eq!(outputs[&first].live_delay_us(), 400_000);
        assert_eq!(outputs[&second].live_delay_us(), 0);
    }

    #[test]
    fn a_live_delay_grows_by_exactly_what_video_arrived_behind_the_audio_clock() {
        // The audio clock is already retarded by the delay in force, so the shortfall measured
        // against it is exactly what still has to be added. Video that is early adds nothing.
        assert_eq!(grown_live_delay_us(0, -180_000), 180_000);
        assert_eq!(grown_live_delay_us(180_000, -40_000), 220_000);
        assert_eq!(grown_live_delay_us(220_000, 90_000), 220_000);
        assert_eq!(grown_live_delay_us(u64::MAX, -1), u64::MAX);
    }

    #[test]
    fn a_live_delay_only_gives_back_the_margin_above_its_headroom() {
        // A review window in which the closest frame still arrived 400 ms early can return 300 ms
        // and keep 100 ms to absorb jitter. One that arrived just in time returns nothing.
        assert_eq!(shrunk_live_delay_us(500_000, 400_000), 200_000);
        assert_eq!(shrunk_live_delay_us(500_000, LIVE_DELAY_HEADROOM_US), 500_000);
        assert_eq!(shrunk_live_delay_us(500_000, -50_000), 500_000);
        assert_eq!(shrunk_live_delay_us(10_000, 900_000), 0);
    }

    #[test]
    fn a_forward_audio_step_is_a_gap_and_only_a_backward_one_restarts_the_timeline() {
        // Regression: a 2 ms tolerance classified ordinary capture jitter as a restart, and every
        // restart discarded the whole device buffer. Only a clock that went backwards is a restart.
        use AudioTimelineStep::{Continuous, Gap, Restart};
        assert_eq!(classify_audio_step(Some(1_000_000), 1_000_020, true), Continuous);
        assert_eq!(
            classify_audio_step(Some(1_000_000), 1_000_000 - AUDIO_GAP_US + 1, true),
            Continuous
        );
        assert_eq!(
            classify_audio_step(Some(1_000_000), 1_000_000 + AUDIO_GAP_US, true),
            Gap(u64::try_from(AUDIO_GAP_US).unwrap())
        );
        assert_eq!(classify_audio_step(Some(1_000_000), 1_000_000 - AUDIO_GAP_US, true), Restart);
        // The first packet has nothing to compare against, and timed media keeps exact PTS.
        assert_eq!(classify_audio_step(None, 5_000_000, true), Continuous);
        assert_eq!(classify_audio_step(Some(1_000_000), 9_000_000, false), Continuous);
    }

    #[test]
    fn live_channel_allowance_matches_the_declared_latency_horizon() {
        let configuration = TrackConfiguration {
            context_id: 1,
            surface_id: 1,
            track_id: 1,
            slot: 1,
            mode: TrackMode::Live,
            lane: LaneClass::Bulk,
            maximum_record_body: 4 * 1024 * 1024,
            maximum_rate_millihertz: 30_000,
            maximum_encoded_bits_per_second: 8_000_000,
            maximum_records_per_second: 34,
            maximum_inflight_body_bytes: 64 * 1024 * 1024,
            kind: KindConfiguration::Raster(RasterConfiguration {
                width: 1,
                height: 1,
                alpha_mode: scene::ALPHA_STRAIGHT,
                delta_enabled: false,
                maximum_delta_operations: 1,
                zstd_enabled: false,
            }),
            target_latency_us: 33_000,
            maximum_latency_us: 100_000,
            retained_pixel_charge: 1,
        };

        let (bytes, records) = live_channel_flow(&configuration);
        assert_eq!(bytes, 4 * 1024 * 1024 + 100_000);
        assert_eq!(records, 4);

        let audio = TrackConfiguration {
            context_id: 1,
            surface_id: 1,
            track_id: 2,
            slot: scene::SLOT_AUDIO,
            mode: TrackMode::Live,
            lane: LaneClass::Realtime,
            maximum_record_body: 4_096,
            maximum_rate_millihertz: 50_000,
            maximum_encoded_bits_per_second: 128_000,
            maximum_records_per_second: 100,
            maximum_inflight_body_bytes: 1 << 20,
            kind: KindConfiguration::Audio(vivid_protocol::track::AudioConfiguration {
                codec: "opus".into(),
                packetization: "opus-packet-v1".into(),
                extradata: vec![],
                sample_rate: 48_000,
                channels: 2,
                channel_mask: 3,
                maximum_access_unit_bytes: 4_048,
                codec_string: Some("opus".into()),
            }),
            target_latency_us: 33_000,
            maximum_latency_us: 100_000,
            retained_pixel_charge: 0,
        };
        let (bytes, records) = live_channel_flow(&audio);
        assert_eq!(bytes, 36_096);
        assert_eq!(records, 200);
    }

    fn next_target_change(session: &vivid_sdk::Session) -> messages::PayloadMap {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match session.take_event().unwrap() {
                Some(SessionEvent::TargetChanged(payload)) => return payload,
                Some(_) => continue,
                None => thread::sleep(Duration::from_millis(1)),
            }
        }
        panic!("presenter never sent TARGET_CHANGED");
    }

    #[test]
    fn contract_is_finite_and_terminal_only() {
        let contract = presenter_contract();
        assert_eq!(contract.get(Resource::ChildSessionLeases), 0);
        assert_eq!(contract.get(Resource::InputEventsPerSecond), 0);
        assert!(contract.get(Resource::Surfaces) > 0);
        assert!(contract.get(Resource::Tracks) > 0);
    }

    #[test]
    fn secret_text_is_exact_lowercase_hex() {
        let value = encode_hex(&[0x00, 0xab, 0xff]);
        assert_eq!(value, "00abff");
    }

    #[test]
    fn target_descriptor_advertises_anchor_v3() {
        let target = TerminalTarget::new(test_geometry()).unwrap();
        assert_eq!(target.descriptor()[7].1.as_u64(), Some(3));
    }

    /// A window resize has to reach producers. The startup geometry is only correct until the
    /// first resize, so a producer started afterwards must be told the terminal's real size in
    /// `WELCOME`, and a producer that is already running must see it as `TARGET_CHANGED`.
    #[test]
    fn a_display_change_reaches_live_and_later_sessions() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let live = connect(&service);
        assert_eq!(descriptor_summary(&live.info().target_descriptor), (80, 24, true));

        let resized = DisplayGeometry {
            viewport_width: 1550,
            viewport_height: 1450,
            columns: 155,
            rows: 58,
            ..test_geometry()
        };
        let generation = service.update_metrics(resized).expect("a resize is a new generation");
        assert_eq!(generation, 2);

        service.flush_display_change(None);
        let announced = next_target_change(&live);
        assert_eq!(descriptor_summary(&announced), (155, 58, false));
        assert_eq!(announced[9].1.as_u64(), Some(generation));

        // The settle timer fires after the unsettled announcement was already consumed.
        service.flush_display_change(Some(generation));
        let settled = next_target_change(&live);
        assert_eq!(descriptor_summary(&settled), (155, 58, true));
        assert_eq!(settled[9].1.as_u64(), Some(generation));

        let later = connect(&service);
        assert_eq!(descriptor_summary(&later.info().target_descriptor), (155, 58, true));
        assert_eq!(later.info().target_generation.get(), generation);
    }

    fn grid_surface(session: &mut vivid_sdk::Session, surface_id: u64) -> vivid_sdk::Surface {
        let context_id = session.info().root_context_id;
        session
            .create_surface(
                SurfaceDefinition {
                    context_id,
                    surface_id,
                    semantic_profile: registry::TERMINAL_CONTENT.into(),
                    coordinate_model: CoordinateModel::TerminalContentCells,
                    logical_width: 2,
                    logical_height: 2,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                    descriptor: SurfaceDescriptor {
                        role: SurfaceRole::Figure,
                        title: "target follow".into(),
                        semantic_content_revision: 1,
                        semantic_availability: 0,
                        locator_hint: String::new(),
                    },
                    policy: 0,
                    profile_parameters: vec![],
                },
                &RequestMetadata::default(),
            )
            .unwrap()
    }

    fn grid_node(
        context_id: u64,
        node_id: u64,
        surface: &vivid_sdk::Surface,
        cols: u64,
    ) -> SceneNode {
        SceneNode {
            owning_context_id: context_id,
            node_id,
            surface_context_id: surface.context_id(),
            surface_id: surface.id(),
            geometry: vec![
                (0, Value::Unsigned(1)),
                (1, Value::Unsigned(0)),
                (2, Value::Unsigned(0)),
                (3, Value::Unsigned(cols << 32)),
                (4, Value::Unsigned(2_u64 << 32)),
                (5, Value::Unsigned(1)),
            ],
            fit: vivid_sdk::Fit::Contain,
            linear_sampling: true,
            z_index: 0,
            visible: true,
            opacity: u16::MAX,
            clip: None,
        }
    }

    #[test]
    fn track_control_trace_reconstructs_seek_order_and_rejections() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut session = connect(&service);
        let context_id = session.info().root_context_id;
        let surface = grid_surface(&mut session, 70);
        let track = session
            .create_track(
                TrackConfiguration {
                    context_id,
                    surface_id: surface.id(),
                    track_id: 71,
                    slot: scene::SLOT_RASTER,
                    mode: TrackMode::Timed,
                    lane: LaneClass::Bulk,
                    maximum_record_body: 128,
                    maximum_rate_millihertz: 60_000,
                    maximum_encoded_bits_per_second: 1_000_000,
                    maximum_records_per_second: 60,
                    maximum_inflight_body_bytes: 4_096,
                    kind: KindConfiguration::Raster(RasterConfiguration {
                        width: 2,
                        height: 2,
                        alpha_mode: scene::ALPHA_STRAIGHT,
                        delta_enabled: false,
                        maximum_delta_operations: 1,
                        zstd_enabled: false,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: 1_000_000,
                    retained_pixel_charge: 4,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let channel = session.open_track_channel(&track).unwrap();
        channel.send_raster(0, 1, &[0x11, 0x22, 0x33, 0xff].repeat(4), false).unwrap();
        session
            .wait_track(
                &track,
                TrackWaitCondition::MilestoneSet,
                Some(MILESTONE_OUTPUT_READY),
                1_000_000,
            )
            .unwrap();
        session
            .activate_tracks(
                &surface,
                &[SlotBinding {
                    slot: scene::SLOT_RASTER,
                    track_id: track.id(),
                    expected_channel_generation: track.channel_generation(),
                    required_milestone: MILESTONE_OUTPUT_READY,
                }],
                &RequestMetadata::default(),
            )
            .unwrap();

        session.play(&track, 0, 0, 1_000_000).unwrap();
        session.pause(&track).unwrap();
        session.flush(&track, 1).unwrap();
        session.drain(&track).unwrap();
        session.advance_channel(&track, 1, &RequestMetadata::default()).unwrap();

        let owner =
            SessionIdentity::new(service.shared.presenter, session.info().session_id).unwrap();
        let identity =
            owner.context(context_id).unwrap().surface(surface.id()).unwrap().track(71).unwrap();
        service.scene.advance_channel(identity).unwrap();
        assert!(
            session.advance_channel(&track, 1, &RequestMetadata::default()).is_err(),
            "a stale SDK generation is rejected"
        );
        session.destroy_track(&track, &RequestMetadata::default()).unwrap();
        drop(channel);

        let batch = service.automation_trace(
            trace::TraceSelection::Tail,
            128,
            trace::TraceFilter {
                session_id: Some(owner.session_id),
                context_id: Some(context_id),
                surface_id: Some(surface.id()),
                track_id: Some(71),
                ..trace::TraceFilter::default()
            },
        );
        let names = batch.events.iter().map(|event| event.event.as_str()).collect::<Vec<_>>();
        for expected in [
            "track_created",
            "track_channel_accepted",
            "play_applied",
            "pause_applied",
            "flush_applied",
            "drain_applied",
            "channel_advanced",
            "track_control_rejected",
            "track_destroyed",
        ] {
            assert!(names.contains(&expected), "missing {expected} from {names:?}");
        }
        let ordered = ["play_applied", "pause_applied", "flush_applied", "channel_advanced"]
            .map(|name| names.iter().position(|event| *event == name).unwrap());
        assert!(ordered.windows(2).all(|pair| pair[0] < pair[1]));

        let rejected =
            batch.events.iter().find(|event| event.event == "track_control_rejected").unwrap();
        assert_eq!(rejected.track, Some(identity.into()));
        assert_eq!(rejected.data["operation"], serde_json::json!("advance_channel"));
        assert!(rejected.data["control_record_sequence"].as_u64().unwrap() > 0);
    }

    fn open_raw_track_channel(
        service: &VividService,
        identity: TrackIdentity,
        generation: ChannelGeneration,
    ) -> io::Result<LocalStream> {
        let (channel_key, configuration) = {
            let registry = lock(&service.shared.registry);
            let session = registry
                .sessions
                .get(&identity.surface.context.session.session_id)
                .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "session disappeared"))?;
            let status = service
                .scene
                .track_status(identity)
                .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "track disappeared"))?;
            (*session.channel_key.expose(), status.configuration)
        };
        let client_nonce = [0x52; 16];
        let authentication_tag = auth::channel_tag(
            &channel_key,
            identity.surface.context.session.session_id,
            identity.surface.context.context_id,
            identity.surface.surface_id,
            identity.track_id,
            generation.get(),
            configuration.kind.kind() as u32,
            configuration.lane as u32,
            &client_nonce,
        );
        let open = ChannelOpen {
            session_id: identity.surface.context.session.session_id,
            context_id: identity.surface.context.context_id,
            surface_id: identity.surface.surface_id,
            track_id: identity.track_id,
            channel_generation: generation.get(),
            track_kind: configuration.kind.kind(),
            lane: configuration.lane,
            client_nonce,
            authentication_tag,
        };
        let envelope = Envelope::correlated(1, open.payload()).map_err(io::Error::other)?;
        let body = envelope.encode().map_err(io::Error::other)?;
        let mut stream = connect_endpoint(service.control_endpoint())?;
        stream.write_all(&vivid_protocol::wire::encode_preface(
            ConnectionKind::Track,
            vivid_protocol::HARD_MAX_RECORD_BODY,
        ))?;
        write_raw(&mut stream, 1, messages::CHANNEL_OPEN, identity.track_id, &body)?;
        let accepted = read_raw(&mut stream)?;
        if accepted.record_type != messages::CHANNEL_ACCEPTED {
            return Err(io::Error::other("track channel was not accepted"));
        }
        Ok(stream)
    }

    #[test]
    fn channel_failure_trace_is_typed_and_generation_scoped() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut session = connect(&service);
        let context_id = session.info().root_context_id;
        let surface = grid_surface(&mut session, 72);
        let track = session
            .create_track(
                TrackConfiguration {
                    context_id,
                    surface_id: surface.id(),
                    track_id: 73,
                    slot: scene::SLOT_RASTER,
                    mode: TrackMode::Timed,
                    lane: LaneClass::Bulk,
                    maximum_record_body: 128,
                    maximum_rate_millihertz: 60_000,
                    maximum_encoded_bits_per_second: 1_000_000,
                    maximum_records_per_second: 60,
                    maximum_inflight_body_bytes: 4_096,
                    kind: KindConfiguration::Raster(RasterConfiguration {
                        width: 2,
                        height: 2,
                        alpha_mode: scene::ALPHA_STRAIGHT,
                        delta_enabled: false,
                        maximum_delta_operations: 1,
                        zstd_enabled: false,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: 1_000_000,
                    retained_pixel_charge: 4,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let owner =
            SessionIdentity::new(service.shared.presenter, session.info().session_id).unwrap();
        let identity =
            owner.context(context_id).unwrap().surface(surface.id()).unwrap().track(73).unwrap();

        let first_generation = track.channel_generation();
        let mut superseded = open_raw_track_channel(&service, identity, first_generation).unwrap();
        session.advance_channel(&track, 1, &RequestMetadata::default()).unwrap();
        write_raw(&mut superseded, 2, messages::RASTER_FRAME, identity.track_id + 1, &[]).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let superseded_detach = loop {
            let batch = service.automation_trace(
                trace::TraceSelection::Tail,
                128,
                trace::TraceFilter {
                    session_id: Some(owner.session_id),
                    context_id: Some(context_id),
                    surface_id: Some(surface.id()),
                    track_id: Some(identity.track_id),
                    ..trace::TraceFilter::default()
                },
            );
            if let Some(event) = batch.events.into_iter().find(|event| {
                event.event == "track_channel_detached"
                    && event.data["channel_generation"] == serde_json::json!(first_generation.get())
            }) {
                break event;
            }
            assert!(Instant::now() < deadline, "superseded channel did not detach");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(superseded_detach.data["disposition"], "superseded");
        assert_eq!(superseded_detach.data["failure"]["kind"], "record_identity");
        assert_eq!(superseded_detach.data["last_record"]["record_sequence"], 2);

        let current_generation = track.channel_generation();
        let mut current = open_raw_track_channel(&service, identity, current_generation).unwrap();
        write_raw(&mut current, 2, messages::RASTER_FRAME, identity.track_id + 1, &[]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let lost = loop {
            let batch = service.automation_trace(
                trace::TraceSelection::Tail,
                128,
                trace::TraceFilter {
                    session_id: Some(owner.session_id),
                    context_id: Some(context_id),
                    surface_id: Some(surface.id()),
                    track_id: Some(identity.track_id),
                    ..trace::TraceFilter::default()
                },
            );
            if let Some(event) = batch.events.into_iter().find(|event| {
                event.event == "track_lost"
                    && event.data["channel_generation"]
                        == serde_json::json!(current_generation.get())
            }) {
                break event;
            }
            assert!(Instant::now() < deadline, "current channel failure did not lose the track");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(lost.data["failure"]["kind"], "record_identity");
        assert_eq!(lost.data["failure"]["error_code"], messages::ERROR_DECODER);
        assert_eq!(lost.data["last_record"]["record_type"], messages::RASTER_FRAME);
        assert_eq!(lost.data["last_record"]["body_bytes"], 0);
    }

    /// A seek replaces linked audio while retaining its video surface. Clearing the old slot is
    /// the producer-visible mutation that advances both copies of the surface revision; destroying
    /// the now-unbound track must then leave that revision alone so the replacement can activate.
    #[test]
    fn clear_then_destroy_keeps_repeated_track_replacements_revision_synchronized() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut seeking = connect(&service);
        let mut neighbor = connect(&service);
        let seeking_context = seeking.info().root_context_id;
        let neighbor_context = neighbor.info().root_context_id;
        // Both producers deliberately reuse every numeric object ID.
        let seeking_surface = grid_surface(&mut seeking, 90);
        let neighbor_surface = grid_surface(&mut neighbor, 90);
        seeking
            .create_node(
                &grid_node(seeking_context, 94, &seeking_surface, 2),
                &RequestMetadata::default(),
            )
            .unwrap();
        neighbor
            .create_node(
                &grid_node(neighbor_context, 94, &neighbor_surface, 2),
                &RequestMetadata::default(),
            )
            .unwrap();

        let track_configuration =
            |context_id: u64, surface_id: u64, track_id: u64| TrackConfiguration {
                context_id,
                surface_id,
                track_id,
                slot: scene::SLOT_RASTER,
                mode: TrackMode::Live,
                lane: LaneClass::Bulk,
                maximum_record_body: 88,
                maximum_rate_millihertz: 60_000,
                maximum_encoded_bits_per_second: 1_000_000,
                maximum_records_per_second: 60,
                maximum_inflight_body_bytes: 4_096,
                kind: KindConfiguration::Raster(RasterConfiguration {
                    width: 2,
                    height: 2,
                    alpha_mode: scene::ALPHA_STRAIGHT,
                    delta_enabled: false,
                    maximum_delta_operations: 1,
                    zstd_enabled: false,
                }),
                target_latency_us: 0,
                maximum_latency_us: 1_000_000,
                retained_pixel_charge: 0,
            };
        let ready_track = |session: &mut vivid_sdk::Session,
                           surface: &vivid_sdk::Surface,
                           track_id: u64| {
            let track = session
                .create_track(
                    track_configuration(surface.context_id(), surface.id(), track_id),
                    &RequestMetadata::default(),
                )
                .unwrap();
            let channel = session.open_track_channel(&track).unwrap();
            channel.send_raster(0, track_id, &[0x44, 0x88, 0xcc, 0xff].repeat(4), false).unwrap();
            session
                .wait_track(
                    &track,
                    TrackWaitCondition::MilestoneSet,
                    Some(MILESTONE_OUTPUT_READY),
                    1_000_000,
                )
                .unwrap();
            (track, channel)
        };
        let activate = |session: &mut vivid_sdk::Session,
                        surface: &vivid_sdk::Surface,
                        track: &vivid_sdk::Track| {
            session
                .activate_tracks(
                    surface,
                    &[SlotBinding {
                        slot: scene::SLOT_RASTER,
                        track_id: track.id(),
                        expected_channel_generation: track.channel_generation(),
                        required_milestone: MILESTONE_OUTPUT_READY,
                    }],
                    &RequestMetadata::default(),
                )
                .unwrap();
        };
        let clear = |session: &mut vivid_sdk::Session, surface: &vivid_sdk::Surface| {
            session
                .activate_tracks(
                    surface,
                    &[SlotBinding {
                        slot: scene::SLOT_RASTER,
                        track_id: 0,
                        expected_channel_generation: ChannelGeneration::ZERO,
                        required_milestone: 0,
                    }],
                    &RequestMetadata::default(),
                )
                .unwrap();
        };

        let (neighbor_track, neighbor_channel) = ready_track(&mut neighbor, &neighbor_surface, 91);
        activate(&mut neighbor, &neighbor_surface, &neighbor_track);
        let neighbor_identity =
            SessionIdentity::new(service.shared.presenter, neighbor.info().session_id).unwrap();
        let neighbor_surface_identity = neighbor_identity
            .context(neighbor_context)
            .unwrap()
            .surface(neighbor_surface.id())
            .unwrap();
        let neighbor_revision =
            service.scene.surface_status(neighbor_surface_identity).unwrap().revision;

        for track_id in [91, 92, 93] {
            let (track, channel) = ready_track(&mut seeking, &seeking_surface, track_id);
            activate(&mut seeking, &seeking_surface, &track);
            clear(&mut seeking, &seeking_surface);
            channel.close().unwrap();
            seeking.destroy_track(&track, &RequestMetadata::default()).unwrap();
        }

        let neighbor_status = service.scene.surface_status(neighbor_surface_identity).unwrap();
        assert_eq!(neighbor_status.revision, neighbor_revision);
        assert_eq!(neighbor_status.active_slots.get(&scene::SLOT_RASTER), Some(&91));
        neighbor_channel.send_raster(0, 95, &[0x11, 0x22, 0x33, 0xff].repeat(4), false).unwrap();
        let neighbor_track_status = neighbor.query_track(&neighbor_track).unwrap();
        assert_eq!(neighbor_track_status.lifecycle, 1);
        assert_eq!(neighbor_track_status.last_media_id, 95);

        // Both the next surface mutation and the next scene mutation remain valid for the owner
        // that did not seek.
        activate(&mut neighbor, &neighbor_surface, &neighbor_track);
        neighbor
            .update_node(
                &grid_node(neighbor_context, 94, &neighbor_surface, 3),
                &RequestMetadata::default(),
            )
            .unwrap();
        let neighbor_scene = service.scene.scene_status(neighbor_identity, 8);
        assert_eq!(neighbor_scene.nodes.len(), 1);
        assert!(neighbor_scene.nodes[0].node.visible);
        assert_eq!(neighbor_scene.nodes[0].node.geometry[3].1, Value::Unsigned(3_u64 << 32));
    }

    /// A scene commit names the target generation it was planned against, so an announced resize
    /// has to carry every live scene onto the new target. Leaving a scene behind rejects the
    /// commits a producer makes in response to the announcement it was just sent, which is fatal
    /// for a producer that re-places its node on every resize.
    #[test]
    fn an_announced_display_change_carries_every_live_scene_onto_the_new_target() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut first = connect(&service);
        let mut second = connect(&service);
        let mut unaware = connect(&service);
        // The three producers deliberately reuse the same numeric surface and node IDs.
        let surfaces: Vec<_> = [&mut first, &mut second, &mut unaware]
            .into_iter()
            .map(|session| grid_surface(session, 4))
            .collect();
        for (session, surface) in [&mut first, &mut second, &mut unaware].into_iter().zip(&surfaces)
        {
            let context_id = session.info().root_context_id;
            session
                .create_node(&grid_node(context_id, 5, surface, 80), &RequestMetadata::default())
                .unwrap();
        }

        let resized = DisplayGeometry { columns: 100, rows: 40, ..test_geometry() };
        let generation = service.update_metrics(resized).expect("a resize is a new generation");
        service.flush_display_change(Some(generation));

        for (session, surface) in [&mut first, &mut second].into_iter().zip(&surfaces) {
            let announced = next_target_change(session);
            assert_eq!(session.apply_target_changed(&announced).unwrap().get(), generation);
            let context_id = session.info().root_context_id;
            let commit = session
                .update_node(&grid_node(context_id, 5, surface, 100), &RequestMetadata::default())
                .expect("a commit naming the announced target must be accepted");
            assert_eq!(commit.target_generation.get(), generation);
        }

        // A producer that has not consumed the announcement is still planning against the target
        // it last knew, and that commit stays rejected rather than reaching the new target.
        let context_id = unaware.info().root_context_id;
        let error = unaware
            .update_node(&grid_node(context_id, 5, &surfaces[2], 100), &RequestMetadata::default())
            .expect_err("a commit naming the previous target must be rejected");
        let code = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<vivid_sdk::PresenterError>())
            .map(|error| error.code);
        assert_eq!(code, Some(registry::error::STALE_TARGET_GENERATION));

        // The rejection is actionable: the current target precedes it, so the producer can re-plan
        // against the target it was just told about rather than waiting for the next resize.
        let announced = next_target_change(&unaware);
        assert_eq!(announced[9].1.as_u64(), Some(generation));
        assert_eq!(descriptor_summary(&announced), (100, 40, true));

        // The rejection is scoped to the producer that earned it: the other scenes are unchanged.
        for (session, cols) in [(&first, 100_u64), (&second, 100), (&unaware, 80)] {
            let identity =
                SessionIdentity::new(service.shared.presenter, session.info().session_id).unwrap();
            let status = service.scene().scene_status(identity, 8);
            assert_eq!(status.target_generation.get(), generation);
            assert_eq!(status.nodes.len(), 1);
            assert_eq!(status.nodes[0].node.geometry[3].1, Value::Unsigned(cols << 32));
        }
    }

    #[test]
    fn an_unchanged_stale_or_degenerate_display_change_is_never_announced() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let live = connect(&service);

        assert_eq!(service.update_metrics(test_geometry()), None);
        assert_eq!(service.update_metrics(DisplayGeometry { rows: 0, ..test_geometry() }), None);
        service.flush_display_change(None);
        // A settle timer from a superseded generation must not re-announce the current target.
        service.flush_display_change(Some(0));

        thread::sleep(Duration::from_millis(50));
        assert_eq!(live.take_event().unwrap(), None);
        assert_eq!(live.info().target_generation.get(), 1);
    }

    #[test]
    fn live_resize_reaches_the_sdk_as_a_same_generation_final_settle() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let config = ProducerConfig {
            endpoint_control: Some(service.control_endpoint().to_owned()),
            authentication: ProducerAuthentication::root_hex(service.root_secret()).unwrap(),
            ..ProducerConfig::default()
        };
        let mut session = vivid_sdk::Session::connect(config).unwrap();
        let generation = service
            .update_metrics(DisplayGeometry {
                viewport_width: 1200,
                viewport_height: 800,
                columns: 120,
                rows: 40,
                cell_width: 10,
                cell_height: 20,
            })
            .unwrap();

        let take_target_change = |session: &vivid_sdk::Session| {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                if let Some(SessionEvent::TargetChanged(payload)) = session.take_event().unwrap() {
                    break payload;
                }
                assert!(Instant::now() < deadline, "presenter did not deliver TARGET_CHANGED");
                thread::sleep(Duration::from_millis(1));
            }
        };

        service.flush_display_change(None);
        let unsettled = take_target_change(&session);
        assert_eq!(session.apply_target_changed(&unsettled).unwrap().get(), generation);
        assert_eq!(session.info().target_descriptor[6].1.as_bool(), Some(false));

        service.flush_display_change(Some(generation));
        let settled = take_target_change(&session);
        assert_eq!(session.apply_target_changed(&settled).unwrap().get(), generation);
        assert_eq!(session.info().target_descriptor[6].1.as_bool(), Some(true));

        let context_id = session.info().root_context_id;
        let surface = session
            .create_surface(
                SurfaceDefinition {
                    context_id,
                    surface_id: 71,
                    semantic_profile: registry::GENERIC_CONTENT.into(),
                    coordinate_model: CoordinateModel::DesktopLogicalPixels,
                    logical_width: 1200,
                    logical_height: 780,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                    descriptor: SurfaceDescriptor {
                        role: SurfaceRole::Document,
                        title: "resized target".into(),
                        semantic_content_revision: 1,
                        semantic_availability: 0,
                        locator_hint: String::new(),
                    },
                    policy: 0,
                    profile_parameters: vec![],
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        session
            .create_node(
                &SceneNode {
                    owning_context_id: context_id,
                    node_id: 72,
                    surface_context_id: context_id,
                    surface_id: surface.id(),
                    geometry: vec![
                        (0, Value::Unsigned(1)),
                        (1, Value::Unsigned(0)),
                        (2, Value::Unsigned(0)),
                        (3, Value::Unsigned(120_u64 << 32)),
                        (4, Value::Unsigned(39_u64 << 32)),
                        (5, Value::Unsigned(1)),
                    ],
                    fit: Fit::Contain,
                    linear_sampling: true,
                    z_index: 0,
                    visible: true,
                    opacity: u16::MAX,
                    clip: None,
                },
                &RequestMetadata::default(),
            )
            .expect("a resize event and the presenter's scene precondition must agree");
        session.close().unwrap();
    }

    #[test]
    fn failed_audio_cleanup_is_scoped_by_complete_track_identity() {
        let first = SessionIdentity::new(PresenterInstanceId([1; 16]), 1)
            .unwrap()
            .context(1)
            .unwrap()
            .surface(1)
            .unwrap()
            .track(1)
            .unwrap();
        let second = SessionIdentity::new(PresenterInstanceId([2; 16]), 1)
            .unwrap()
            .context(1)
            .unwrap()
            .surface(1)
            .unwrap()
            .track(1)
            .unwrap();
        let outputs = Mutex::new(HashMap::from([
            (first, AudioOutput::test_output()),
            (second, AudioOutput::test_output()),
        ]));

        stop_failed_audio_output(&outputs, first);

        let outputs = lock(&outputs);
        assert!(!outputs.contains_key(&first));
        assert!(outputs.contains_key(&second));
    }

    #[test]
    fn migrated_sdk_submits_a_live_raster_over_an_authenticated_channel() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut session = connect(&service);
        let context_id = session.info().root_context_id;
        let surface = session
            .create_surface(
                SurfaceDefinition {
                    context_id,
                    surface_id: 7,
                    semantic_profile: registry::TERMINAL_CONTENT.into(),
                    coordinate_model: CoordinateModel::TerminalContentCells,
                    logical_width: 2,
                    logical_height: 2,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                    descriptor: SurfaceDescriptor {
                        role: SurfaceRole::Figure,
                        title: "SDK integration".into(),
                        semantic_content_revision: 1,
                        semantic_availability: 0,
                        locator_hint: String::new(),
                    },
                    policy: 0,
                    profile_parameters: vec![],
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let anchor_id = 11;
        let marker = session.anchor_marker(context_id, anchor_id).unwrap();
        service.handle_terminal_marker(&marker[2..marker.len() - 2], 4, 3, false);
        let anchor = session.query_anchor(context_id, anchor_id).unwrap();
        assert_eq!(anchor.state, 1);
        let commit = session
            .create_node(
                &SceneNode {
                    owning_context_id: context_id,
                    node_id: 8,
                    surface_context_id: context_id,
                    surface_id: surface.id(),
                    geometry: vec![
                        (0, Value::Unsigned(2)),
                        (1, Value::Unsigned(0)),
                        (2, Value::Unsigned(0)),
                        (3, Value::Unsigned(2_u64 << 32)),
                        (4, Value::Unsigned(2_u64 << 32)),
                        (5, Value::Unsigned(1)),
                        (6, Value::Unsigned(context_id)),
                        (7, Value::Unsigned(anchor_id)),
                    ],
                    fit: vivid_sdk::Fit::Contain,
                    linear_sampling: true,
                    z_index: 0,
                    visible: true,
                    opacity: u16::MAX,
                    clip: None,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        assert_eq!(commit.scene_revision, SceneRevision::ONE);
        let track = session
            .create_track(
                TrackConfiguration {
                    context_id: session.info().root_context_id,
                    surface_id: surface.id(),
                    track_id: 9,
                    slot: scene::SLOT_RASTER,
                    mode: TrackMode::Live,
                    lane: LaneClass::Bulk,
                    maximum_record_body: 88,
                    maximum_rate_millihertz: 60_000,
                    maximum_encoded_bits_per_second: 1_000_000,
                    maximum_records_per_second: 60,
                    maximum_inflight_body_bytes: 4096,
                    kind: KindConfiguration::Raster(RasterConfiguration {
                        width: 2,
                        height: 2,
                        alpha_mode: scene::ALPHA_STRAIGHT,
                        delta_enabled: false,
                        maximum_delta_operations: 1,
                        zstd_enabled: false,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: 1_000_000,
                    retained_pixel_charge: 0,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let channel = session.open_track_channel(&track).unwrap();
        channel.send_raster(0, 1, &[0x44, 0x88, 0xcc, 0xff].repeat(4), false).unwrap();
        session
            .wait_track(
                &track,
                TrackWaitCondition::MilestoneSet,
                Some(MILESTONE_OUTPUT_READY),
                1_000_000,
            )
            .unwrap();
        let status = session.query_track(&track).unwrap();
        assert_eq!(status.last_media_id, 1);
        session
            .activate_tracks(
                &surface,
                &[SlotBinding {
                    slot: scene::SLOT_RASTER,
                    track_id: track.id(),
                    expected_channel_generation: track.channel_generation(),
                    required_milestone: MILESTONE_OUTPUT_READY,
                }],
                &RequestMetadata::default(),
            )
            .unwrap();
        let item = service.scene.snapshot().items[0].clone();
        assert_eq!((item.x, item.y), (3_i64 << 32, 4_i64 << 32));
        drop(channel);
        session.close().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !service.scene.session_ids().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(service.scene.session_ids().is_empty());
        assert_eq!(
            service.scene.snapshot().items.len(),
            1,
            "clean GOODBYE must preserve an anchored, policy-permitted terminal poster"
        );
    }

    /// Media §4: `TRACK_READY` key 9 is how a producer learns it may send deltas at all.
    ///
    /// A presenter that applies deltas but never grants them reads to every SDK producer as
    /// full-frames-only. A nested producer whose inner presenter did grant deltas then relays a
    /// delta this hop refuses, which strands the relayed source on its last full frame.
    #[test]
    fn a_delta_capable_raster_track_grants_and_then_applies_delta_frames() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut session = connect(&service);
        let context_id = session.info().root_context_id;
        let surface = session
            .create_surface(
                SurfaceDefinition {
                    context_id,
                    surface_id: 3,
                    semantic_profile: registry::TERMINAL_CONTENT.into(),
                    coordinate_model: CoordinateModel::TerminalContentCells,
                    logical_width: 2,
                    logical_height: 2,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                    descriptor: SurfaceDescriptor {
                        role: SurfaceRole::Figure,
                        title: "raster delta".into(),
                        semantic_content_revision: 1,
                        semantic_availability: 0,
                        locator_hint: String::new(),
                    },
                    policy: 0,
                    profile_parameters: vec![],
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let track = session
            .create_track(
                TrackConfiguration {
                    context_id,
                    surface_id: surface.id(),
                    track_id: 4,
                    slot: scene::SLOT_RASTER,
                    mode: TrackMode::Live,
                    lane: LaneClass::Bulk,
                    maximum_record_body: 4096,
                    maximum_rate_millihertz: 60_000,
                    maximum_encoded_bits_per_second: 8_000_000,
                    maximum_records_per_second: 60,
                    maximum_inflight_body_bytes: 16_384,
                    kind: KindConfiguration::Raster(RasterConfiguration {
                        width: 2,
                        height: 2,
                        alpha_mode: scene::ALPHA_STRAIGHT,
                        delta_enabled: true,
                        maximum_delta_operations: 4,
                        zstd_enabled: false,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: 1_000_000,
                    retained_pixel_charge: 4,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        assert_eq!(
            track.delta_operation_limit().unwrap(),
            4,
            "a delta-enabled raster track has to report the granted operation limit"
        );

        let channel = session.open_track_channel(&track).unwrap();
        channel.send_raster(0, 1, &[0x11, 0x22, 0x33, 0xff].repeat(4), false).unwrap();
        session
            .wait_track(
                &track,
                TrackWaitCondition::MilestoneSet,
                Some(MILESTONE_OUTPUT_READY),
                1_000_000,
            )
            .unwrap();
        channel
            .send_raster_delta(
                0,
                2,
                1,
                0,
                0,
                &[vivid_sdk::RasterDeltaOperation::Overwrite {
                    x: 1,
                    y: 1,
                    width: 1,
                    height: 1,
                    rgba: &[0xaa, 0xbb, 0xcc, 0xff],
                }],
                false,
            )
            .unwrap();

        let identity = service
            .scene
            .track_keys()
            .into_iter()
            .find(|identity| identity.track_id == track.id())
            .expect("the raster track is registered");
        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = loop {
            let frame = service.scene.latest_frame(identity).expect("a published frame");
            if frame.frame_id == 2 || Instant::now() >= deadline {
                break frame;
            }
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(frame.frame_id, 2, "the delta frame was never applied");
        assert_eq!(
            &frame.rgba[12..16],
            &[0xaa, 0xbb, 0xcc, 0xff],
            "the delta overwrote the wrong pixel"
        );
        assert_eq!(
            &frame.rgba[..4],
            &[0x11, 0x22, 0x33, 0xff],
            "the delta disturbed a pixel it did not name"
        );
    }

    fn desktop_service() -> io::Result<VividService> {
        let target = Arc::new(DesktopTarget::new(test_geometry()).unwrap());
        VividService::start_with_target(target, Arc::new(|| {}))
    }

    fn connect_desktop(service: &VividService) -> vivid_sdk::Session {
        let mut required = vec![
            registry::CORE_CONTROL.to_owned(),
            registry::DESKTOP_SURFACE.to_owned(),
            registry::LIVE_MEDIA.to_owned(),
        ];
        required.sort();
        vivid_sdk::Session::connect(ProducerConfig {
            endpoint_control: Some(service.control_endpoint().to_owned()),
            authentication: ProducerAuthentication::root_hex(service.root_secret()).unwrap(),
            target_profile: registry::DESKTOP_SURFACE.to_owned(),
            required_profiles: required,
            optional_profiles: vec![registry::OBSERVABILITY.to_owned()],
            ..ProducerConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn a_web_carrier_session_advertises_the_web_ceilings() {
        // Web §5.2: a session that will ride a browser carrier must be offered ceilings the
        // carrier can deliver, or the bridge closes tracks the presenter claimed to support.
        let service = socket_service!(desktop_service());
        let mut required = vec![
            registry::CORE_CONTROL.to_owned(),
            registry::DESKTOP_SURFACE.to_owned(),
            registry::LIVE_MEDIA.to_owned(),
        ];
        required.sort();
        let mut optional =
            vec![registry::OBSERVABILITY.to_owned(), registry::WEB_CARRIER.to_owned()];
        optional.sort();
        let session = vivid_sdk::Session::connect(ProducerConfig {
            endpoint_control: Some(service.control_endpoint().to_owned()),
            authentication: ProducerAuthentication::root_hex(service.root_secret()).unwrap(),
            target_profile: registry::DESKTOP_SURFACE.to_owned(),
            required_profiles: required,
            optional_profiles: optional,
            ..ProducerConfig::default()
        })
        .unwrap();
        let info = session.info();
        assert!(
            info.accepted_profiles.iter().any(|profile| profile == registry::WEB_CARRIER),
            "an offered web-carrier profile is accepted"
        );
        let contract = &info.resource_contract;
        assert_eq!(
            contract.get(Resource::ControlRecordBody),
            u64::from(vivid_protocol::web::MAX_CONTROL_RECORD_BODY)
        );
        assert_eq!(
            contract.get(Resource::MediaRecordBody),
            u64::from(vivid_protocol::web::MAX_MEDIA_RECORD_BODY)
        );
        assert_eq!(
            contract.get(Resource::InflightMediaBytes),
            vivid_protocol::web::MAX_AGGREGATE_REASSEMBLY
        );
        session.close().unwrap();
    }

    #[test]
    fn a_native_session_keeps_the_native_ceilings() {
        // Without the web-carrier profile nothing about the offer changes.
        let service = socket_service!(desktop_service());
        let session = connect_desktop(&service);
        let info = session.info();
        assert!(!info.accepted_profiles.iter().any(|profile| profile == registry::WEB_CARRIER));
        let contract = &info.resource_contract;
        assert_eq!(
            contract.get(Resource::ControlRecordBody),
            u64::from(vivid_protocol::CONTROL_MAX_RECORD_BODY)
        );
        assert_eq!(
            contract.get(Resource::MediaRecordBody),
            u64::from(vivid_protocol::HARD_MAX_RECORD_BODY)
        );
        session.close().unwrap();
    }

    #[test]
    fn the_web_clamp_never_widens_a_contract() {
        let mut contract = presenter_contract();
        clamp_contract_for_web(&mut contract);
        assert_eq!(
            contract.get(Resource::ControlRecordBody),
            u64::from(vivid_protocol::web::MAX_CONTROL_RECORD_BODY)
        );
        // A ceiling already below the web's survives: clamping takes a minimum, never a raise.
        let mut tight = presenter_contract();
        tight.set(Resource::MediaRecordBody, 1024);
        clamp_contract_for_web(&mut tight);
        assert_eq!(tight.get(Resource::MediaRecordBody), 1024);
    }

    fn desktop_surface(context_id: u64, width: u64) -> SurfaceDefinition {
        SurfaceDefinition {
            context_id,
            surface_id: 5,
            semantic_profile: registry::DESKTOP_CONTENT.into(),
            coordinate_model: CoordinateModel::DesktopLogicalPixels,
            logical_width: width,
            logical_height: 1080,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
            descriptor: SurfaceDescriptor {
                role: SurfaceRole::Desktop,
                title: "desktop".into(),
                semantic_content_revision: 1,
                semantic_availability: 0,
                locator_hint: String::new(),
            },
            policy: 0,
            profile_parameters: vivid_protocol::surface::DesktopSurfaceParameters {
                captured_origin_x: 0,
                captured_origin_y: 0,
                topology: vec![],
                semantic_generation: 1,
                input_capabilities: 0,
            }
            .encode(),
        }
    }

    #[test]
    fn a_desktop_window_presents_the_desktop_target_profile() {
        let service = socket_service!(desktop_service());
        let session = connect_desktop(&service);
        assert_eq!(session.info().target_profile, registry::DESKTOP_SURFACE);

        // Desktop §1 keys 0 through 6, and nothing that could name a device.
        let descriptor = &session.info().target_descriptor;
        assert_eq!(descriptor.len(), 7);
        assert_eq!(descriptor[2].1.as_u64(), Some(800));
        assert_eq!(descriptor[3].1.as_u64(), Some(600));
        for (_, value) in descriptor {
            let leaves = match value {
                Value::Array(entries) => entries.clone(),
                other => vec![other.clone()],
            };
            for leaf in leaves {
                match leaf {
                    Value::Map(entries) => assert!(
                        entries
                            .iter()
                            .all(|(_, item)| !matches!(item, Value::Text(_) | Value::Bytes(_))),
                        "an output descriptor carried free-form data"
                    ),
                    Value::Text(_) | Value::Bytes(_) => {
                        panic!("the desktop descriptor carried text")
                    },
                    _ => {},
                }
            }
        }
    }

    #[test]
    fn a_terminal_producer_cannot_attach_to_a_desktop_window() {
        // Stage 1 D1: a window presents exactly one target profile.
        let service = socket_service!(desktop_service());
        assert!(
            vivid_sdk::Session::connect(ProducerConfig {
                endpoint_control: Some(service.control_endpoint().to_owned()),
                authentication: ProducerAuthentication::root_hex(service.root_secret()).unwrap(),
                ..ProducerConfig::default()
            })
            .is_err(),
            "a terminal-surface-v1 producer must be refused by a desktop window"
        );
    }

    #[test]
    fn each_target_refuses_the_other_semantic_surface_profile() {
        let desktop = socket_service!(desktop_service());
        let mut session = connect_desktop(&desktop);
        let context_id = session.info().root_context_id;
        let mut terminal_shaped = desktop_surface(context_id, 1920);
        terminal_shaped.semantic_profile = registry::TERMINAL_CONTENT.into();
        terminal_shaped.coordinate_model = CoordinateModel::TerminalContentCells;
        assert!(
            session.create_surface(terminal_shaped, &RequestMetadata::default()).is_err(),
            "a desktop target cannot present terminal content"
        );

        let terminal =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut session = connect(&terminal);
        let context_id = session.info().root_context_id;
        assert!(
            session
                .create_surface(desktop_surface(context_id, 1920), &RequestMetadata::default())
                .is_err(),
            "a terminal target cannot present desktop content"
        );
    }

    #[test]
    fn malformed_desktop_profile_parameters_are_refused() {
        let service = socket_service!(desktop_service());
        let mut session = connect_desktop(&service);
        let context_id = session.info().root_context_id;
        let mut broken = desktop_surface(context_id, 1920);
        // Semantic generation zero, which desktop §2 forbids.
        broken.profile_parameters = vivid_protocol::surface::DesktopSurfaceParameters {
            captured_origin_x: 0,
            captured_origin_y: 0,
            topology: vec![],
            semantic_generation: 0,
            input_capabilities: 0,
        }
        .encode();
        assert!(session.create_surface(broken, &RequestMetadata::default()).is_err());
    }

    #[test]
    fn a_desktop_dimension_change_advances_the_surface_generation() {
        // The W3 acceptance: a coordinate-mapping change advances the generation, and nothing
        // about media does. Desktop §2 and core §8.3.
        let service = socket_service!(desktop_service());
        let mut session = connect_desktop(&service);
        let context_id = session.info().root_context_id;
        let surface = session
            .create_surface(desktop_surface(context_id, 1920), &RequestMetadata::default())
            .unwrap();
        assert_eq!(surface.generation(), SurfaceGeneration::ONE);

        session
            .update_surface(
                &surface,
                desktop_surface(context_id, 2560),
                &RequestMetadata::default(),
            )
            .unwrap();
        assert_eq!(surface.generation().get(), 2, "a width change is a coordinate-mapping change");

        // A descriptor-only update is not a mapping change, so the generation holds.
        let mut retitled = desktop_surface(context_id, 2560);
        retitled.descriptor.title = "renamed".into();
        retitled.descriptor.semantic_content_revision = 2;
        session.update_surface(&surface, retitled, &RequestMetadata::default()).unwrap();
        assert_eq!(
            surface.generation().get(),
            2,
            "a descriptor change must not advance the surface generation"
        );
        assert_eq!(surface.revision().get(), 3, "but it is still a revision");
    }

    #[test]
    fn a_full_root_node_covers_the_whole_desktop_target() {
        let service = socket_service!(desktop_service());
        let mut session = connect_desktop(&service);
        let context_id = session.info().root_context_id;
        let surface = session
            .create_surface(desktop_surface(context_id, 1920), &RequestMetadata::default())
            .unwrap();
        session
            .create_node(
                &SceneNode {
                    owning_context_id: context_id,
                    node_id: 3,
                    surface_context_id: context_id,
                    surface_id: surface.id(),
                    geometry: vivid_protocol::geometry::NodeGeometry::full_target().encode(),
                    fit: vivid_sdk::Fit::Contain,
                    linear_sampling: true,
                    z_index: 0,
                    visible: true,
                    opacity: u16::MAX,
                    clip: None,
                },
                &RequestMetadata::default(),
            )
            .unwrap();

        // Normalized geometry projects against the current target extent, in logical pixels.
        let target = service.scene.target();
        let placement = target
            .placement(&SceneNode {
                owning_context_id: context_id,
                node_id: 3,
                surface_context_id: context_id,
                surface_id: surface.id(),
                geometry: vivid_protocol::geometry::NodeGeometry::full_target().encode(),
                fit: vivid_sdk::Fit::Contain,
                linear_sampling: true,
                z_index: 0,
                visible: true,
                opacity: u16::MAX,
                clip: None,
            })
            .unwrap();
        assert_eq!(placement.x, 0);
        assert_eq!(placement.width, 800_i64 << 32);
        assert_eq!(placement.height, 600_i64 << 32);
        assert!(!placement.text_anchored, "a desktop node is never text-anchored");
    }

    #[test]
    fn a_desktop_resize_announces_a_target_change_with_its_reason() {
        let service = socket_service!(desktop_service());
        let session = connect_desktop(&service);
        assert_eq!(session.info().target_generation.get(), 1);

        let resized =
            DisplayGeometry { viewport_width: 1280, viewport_height: 720, ..test_geometry() };
        let generation = service.update_metrics(resized).unwrap();
        assert_eq!(generation, 2);
        service.flush_display_change(None);

        let payload = next_target_change(&session);
        // Look up by key: a desktop descriptor is seven entries, not the terminal's nine.
        let field = |key: u64| {
            payload.iter().find(|entry| entry.0 == key).and_then(|entry| entry.1.as_u64())
        };
        assert_eq!(field(2), Some(1280));
        assert_eq!(field(3), Some(720));
        assert_eq!(field(9), Some(2), "the new target generation");
        let reason = field(10).unwrap();
        assert!(
            reason & vivid_protocol::target::reason::VIRTUAL_BOUNDS != 0,
            "a resize changes the virtual bounds"
        );
    }

    fn lease_definition(
        context_id: u64,
        lease_id: u64,
        secret: &Secret32,
    ) -> SessionLeaseDefinition {
        SessionLeaseDefinition {
            context_id,
            lease_id,
            activation_verifier: auth::activation_verifier(lease_id, secret),
            activation_timeout_us: 20_000_000,
            requested_disconnect_grace_us: 0,
            cleanup_policy: vivid_protocol::lease::CleanupPolicy::Immediate,
            permitted_profiles: {
                let mut profiles =
                    vec![registry::CORE_CONTROL.to_owned(), registry::TERMINAL_SURFACE.to_owned()];
                profiles.sort();
                profiles
            },
            requested_contract: presenter_contract(),
            client_public_key: None,
        }
    }

    fn activate(
        service: &VividService,
        context_id: u64,
        lease_id: u64,
        secret: &Secret32,
    ) -> io::Result<vivid_sdk::Session> {
        vivid_sdk::Session::connect(ProducerConfig {
            endpoint_control: Some(service.control_endpoint().to_owned()),
            authentication: ProducerAuthentication::LeaseActivation {
                context_id,
                lease_id,
                activation_secret: Secret32::new(*secret.expose()),
                attempt_id: [0x77; 16],
                proof_of_possession: None,
            },
            ..ProducerConfig::default()
        })
    }

    #[test]
    fn a_lease_reply_carries_no_secret_and_activates_a_child_session() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut controller = connect(&service);
        let context_id = controller.info().root_context_id;
        let secret = Secret32::new([0x5c; 32]);

        let ready = controller
            .create_session_lease(
                &lease_definition(context_id, 3, &secret),
                &RequestMetadata::default(),
            )
            .unwrap();
        assert_eq!(ready.lease_id, 3);
        assert_eq!(ready.state, vivid_protocol::lease::LeaseState::Issued as u64);

        // Security §6.2: the presenter only ever received a verifier, so the child authenticates
        // with a secret the presenter cannot have leaked back.
        let child = activate(&service, context_id, 3, &secret).unwrap();
        assert_eq!(child.info().target_profile, registry::TERMINAL_SURFACE);
        assert_ne!(
            child.info().session_id,
            controller.info().session_id,
            "a lease activates a distinct logical session"
        );
    }

    #[test]
    fn a_wrong_activation_secret_is_refused() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut controller = connect(&service);
        let context_id = controller.info().root_context_id;
        let secret = Secret32::new([0x11; 32]);
        controller
            .create_session_lease(
                &lease_definition(context_id, 4, &secret),
                &RequestMetadata::default(),
            )
            .unwrap();

        assert!(
            activate(&service, context_id, 4, &Secret32::new([0x12; 32])).is_err(),
            "a near-miss secret must not activate"
        );
        assert!(
            activate(&service, context_id, 9, &secret).is_err(),
            "the lease ID is bound into the verifier"
        );
        // The real secret still works afterwards: a failed attempt must not consume the lease.
        assert!(activate(&service, context_id, 4, &secret).is_ok());
    }

    #[test]
    fn revoking_a_lease_closes_only_its_child() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut controller = connect(&service);
        let context_id = controller.info().root_context_id;
        let first = Secret32::new([0x21; 32]);
        let second = Secret32::new([0x22; 32]);
        for (lease_id, secret) in [(5, &first), (6, &second)] {
            controller
                .create_session_lease(
                    &lease_definition(context_id, lease_id, secret),
                    &RequestMetadata::default(),
                )
                .unwrap();
        }
        let kept = activate(&service, context_id, 6, &second).unwrap();
        let _doomed = activate(&service, context_id, 5, &first).unwrap();

        controller.revoke_session_lease(context_id, 5, &RequestMetadata::default()).unwrap();

        // The surviving child still answers, and the revoked one's session is gone.
        assert!(kept.query_session().is_ok(), "an unrelated lease must be untouched");
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock(&service.shared.registry).sessions.len() > 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            lock(&service.shared.registry).sessions.len(),
            2,
            "only the controller and the surviving child remain"
        );
    }

    #[test]
    fn closing_the_issuer_takes_its_leases_with_it() {
        // Security §4.3: parent cleanup removes only that subtree.
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut controller = connect(&service);
        let context_id = controller.info().root_context_id;
        let secret = Secret32::new([0x31; 32]);
        controller
            .create_session_lease(
                &lease_definition(context_id, 7, &secret),
                &RequestMetadata::default(),
            )
            .unwrap();
        controller.close().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while lock(&service.shared.registry).leases.len() > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(lock(&service.shared.registry).leases.len(), 0);
        assert!(
            activate(&service, context_id, 7, &secret).is_err(),
            "a lease cannot outlive the session that issued it"
        );
    }

    #[test]
    fn session_status_reports_revisions_a_producer_can_reconcile_against() {
        // Core §10, and the reason the previous payload was unusable: it described identity
        // rather than revisions, so nothing could be compared after a resume.
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut session = connect(&service);
        let context_id = session.info().root_context_id;
        session
            .create_surface(
                SurfaceDefinition {
                    context_id,
                    surface_id: 21,
                    semantic_profile: registry::TERMINAL_CONTENT.into(),
                    coordinate_model: CoordinateModel::TerminalContentCells,
                    logical_width: 2,
                    logical_height: 2,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                    descriptor: SurfaceDescriptor {
                        role: SurfaceRole::Figure,
                        title: "status".into(),
                        semantic_content_revision: 1,
                        semantic_availability: 0,
                        locator_hint: String::new(),
                    },
                    policy: 0,
                    profile_parameters: vec![],
                },
                &RequestMetadata::default(),
            )
            .unwrap();

        let status = session.query_session().unwrap();
        let field = |key: u64| status.iter().find(|entry| entry.0 == key).map(|entry| &entry.1);
        assert!(field(0).and_then(|value| value.as_u64()).is_some(), "session revision");
        assert_eq!(field(2).and_then(|value| value.as_u64()), Some(1), "target generation");
        assert_eq!(field(3).and_then(|value| value.as_u64()), Some(1), "establishment: active");
        assert_eq!(field(5).and_then(|value| value.as_bool()), Some(true), "input disabled");
        let Some(Value::Array(entries)) = field(6) else {
            panic!("SESSION_STATUS must carry bounded object summaries")
        };
        assert_eq!(entries.len(), 1, "the one surface this session owns");
    }

    fn resumable_definition(
        context_id: u64,
        lease_id: u64,
        secret: &Secret32,
    ) -> SessionLeaseDefinition {
        SessionLeaseDefinition {
            requested_disconnect_grace_us: 10_000_000,
            cleanup_policy: vivid_protocol::lease::CleanupPolicy::SuspendOnUncleanLoss,
            ..lease_definition(context_id, lease_id, secret)
        }
    }

    use std::io::{Read, Write};

    /// A control connection driven at the wire, so a test can close the transport uncleanly.
    ///
    /// The SDK cannot do this today: dropping a `Session` marks its lifecycle closed but leaves
    /// the socket open, because neither connection half owns anything shutdown-able. Suspension is
    /// precisely the behavior that needs a real loss, so this drives it directly.
    struct RawClient {
        stream: LocalStream,
        sequence: u64,
        session_id: u64,
        session_tag: [u8; messages::SESSION_TAG_BYTES],
        resume_key: Secret32,
    }

    impl RawClient {
        fn activate(
            service: &VividService,
            context_id: u64,
            lease_id: u64,
            secret: &Secret32,
        ) -> io::Result<Self> {
            let authentication = HelloAuthentication::LeaseActivation {
                context_id,
                lease_id,
                activation_secret: Secret32::new(*secret.expose()),
                attempt_id: [0x5a; 16],
                proof_of_possession: None,
            };
            Self::open(service, authentication, |_, _| Ok(()))
        }

        fn resume(
            service: &VividService,
            context_id: u64,
            lease_id: u64,
            session_id: u64,
            resume_generation: u64,
            prior_resume_key: &Secret32,
        ) -> io::Result<Self> {
            let authentication = HelloAuthentication::Resume {
                context_id,
                lease_id,
                session_id,
                resume_generation,
                attempt_id: [0x6b; 16],
                proof: [0; 32],
            };
            let key = Secret32::new(*prior_resume_key.expose());
            Self::open(service, authentication, move |hello: &mut Hello, preface: &[u8; 16]| {
                hello.authenticate_resume(key.expose(), preface).map_err(io::Error::other)
            })
        }

        fn open(
            service: &VividService,
            authentication: HelloAuthentication,
            sign: impl FnOnce(&mut Hello, &[u8; 16]) -> io::Result<()>,
        ) -> io::Result<Self> {
            let mut stream = connect_endpoint(service.control_endpoint())?;
            let preface = vivid_protocol::wire::encode_preface(
                ConnectionKind::Control,
                vivid_protocol::CONTROL_MAX_RECORD_BODY,
            );
            stream.write_all(&preface)?;
            let mut required =
                vec![registry::CORE_CONTROL.to_owned(), registry::TERMINAL_SURFACE.to_owned()];
            required.sort();
            let mut hello = Hello {
                producer_name: "raw".into(),
                producer_version: "0".into(),
                required_profiles: required,
                optional_profiles: Vec::new(),
                maximum_control_body: vivid_protocol::CONTROL_MAX_RECORD_BODY,
                client_nonce: [0x3c; 32],
                authentication,
                target_profile: registry::TERMINAL_SURFACE.into(),
                extensions: Vec::new(),
            };
            sign(&mut hello, &preface)?;
            let body = hello.encode(1).map_err(io::Error::other)?;
            write_raw(&mut stream, 1, messages::HELLO, 0, &body)?;

            let record = read_raw(&mut stream)?;
            if record.record_type != messages::WELCOME {
                return Err(io::Error::other("establishment refused"));
            }
            let (_, welcome) = Welcome::decode(&record.body).map_err(io::Error::other)?;
            let session_secret = match &hello.authentication {
                HelloAuthentication::LeaseActivation { activation_secret, .. } => {
                    Secret32::new(*activation_secret.expose())
                },
                _ => Secret32::new([0; 32]),
            };
            let prk = auth::extract_handshake_prk(
                &session_secret,
                &hello.client_nonce,
                &welcome.server_nonce,
                &[0; 32],
            );
            let (keys, _) = auth::derive_session_keys(
                &prk,
                welcome.session_id,
                welcome.resume_generation,
                &welcome.session_tag,
            );
            Ok(Self {
                stream,
                sequence: 1,
                session_id: welcome.session_id,
                session_tag: welcome.session_tag,
                resume_key: Secret32::new(*keys.resume_key()),
            })
        }

        /// A root session that never reads its control replies.
        ///
        /// The point of the freeze regressions below: a producer is free to stop draining, and
        /// nothing about the presenter's terminal or window may depend on it not doing so.
        fn root(service: &VividService) -> io::Result<Self> {
            let secret = Secret32::new(
                <[u8; 32]>::try_from(
                    (0..32)
                        .map(|index| {
                            u8::from_str_radix(&service.root_secret()[index * 2..index * 2 + 2], 16)
                                .expect("root secret is hex")
                        })
                        .collect::<Vec<_>>()
                        .as_slice(),
                )
                .expect("root secret is 32 bytes"),
            );
            Self::open(
                service,
                HelloAuthentication::Root { proof: [0; 32] },
                move |hello: &mut Hello, preface: &[u8; 16]| {
                    hello.authenticate_root(&secret, preface).map_err(io::Error::other)
                },
            )
        }

        /// Send one correlated request, which closes the activation-retry window.
        fn query_session(&mut self) -> io::Result<messages::PayloadMap> {
            self.sequence += 1;
            let body = Envelope::new(2, Vec::new()).encode().map_err(io::Error::other)?;
            write_raw(&mut self.stream, self.sequence, messages::QUERY_SESSION, 0, &body)?;
            let record = read_raw(&mut self.stream)?;
            if record.record_type != messages::SESSION_STATUS {
                return Err(io::Error::other("expected SESSION_STATUS"));
            }
            Ok(messages::decode_control(&record.body).map_err(io::Error::other)?.payload)
        }
    }

    fn connect_endpoint(endpoint: &str) -> io::Result<LocalStream> {
        #[cfg(unix)]
        {
            LocalStream::connect(endpoint.trim_start_matches("unix:"))
        }
        #[cfg(windows)]
        {
            LocalStream::connect(endpoint.trim_start_matches("tcp:"))
        }
    }

    fn write_raw(
        stream: &mut LocalStream,
        sequence: u64,
        record_type: u16,
        object_id: u64,
        body: &[u8],
    ) -> io::Result<()> {
        let header = vivid_protocol::wire::RecordHeader {
            body_length: u32::try_from(body.len()).map_err(io::Error::other)?,
            record_type,
            flags: 0,
            object_id,
            sequence,
        };
        stream.write_all(&header.encode())?;
        stream.write_all(body)?;
        stream.flush()
    }

    fn read_raw(stream: &mut LocalStream) -> io::Result<vivid_protocol::wire::Record> {
        let mut header = [0_u8; vivid_protocol::wire::HEADER_SIZE];
        stream.read_exact(&mut header)?;
        let header = vivid_protocol::wire::RecordHeader::decode(header);
        let mut body = vec![0_u8; header.body_length as usize];
        stream.read_exact(&mut body)?;
        Ok(vivid_protocol::wire::Record {
            record_type: header.record_type,
            flags: header.flags,
            object_id: header.object_id,
            sequence: header.sequence,
            body,
        })
    }

    /// Has this end of the connection already been closed by the presenter?
    fn is_closed_now(stream: &LocalStream) -> bool {
        stream.set_nonblocking(true).unwrap();
        let mut stream = stream;
        match stream.read(&mut [0_u8; 1]) {
            Ok(0) => true,
            Ok(_) => false,
            Err(error) => !matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted),
        }
    }

    /// The body of an anchor marker, as the PTY parser hands it over.
    fn marker_body(key: &AnchorKey, tag: &[u8; 16], context_id: u64, anchor_id: u64) -> String {
        let marker = anchor::encode_marker(key, tag, context_id, anchor_id).unwrap();
        marker[2..marker.len() - 2].to_owned()
    }

    fn live_sessions(service: &VividService) -> Vec<Arc<SessionRuntime>> {
        lock(&service.shared.registry).sessions.values().cloned().collect()
    }

    fn anchors_owned_by(service: &VividService, session: &Arc<SessionRuntime>) -> usize {
        service
            .scene
            .anchor_positions()
            .iter()
            .filter(|(identity, ..)| identity.context.session == session.identity)
            .count()
    }

    /// A program printing marker-shaped APCs in a loop must not be able to spend the terminal's
    /// output thread on HMAC verification — its own session's or anybody else's.
    ///
    /// Verification happens before the `seen_anchors` dedup can help, and the tag a marker carries
    /// is visible to any program that has seen one, so a replayed marker with a wrong
    /// authenticator is the cheapest possible attack and the most expensive record to serve.
    #[test]
    fn an_anchor_marker_flood_is_bounded_and_costs_no_other_session_anything() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let flooded = connect(&service);
        let flooded_context = flooded.info().root_context_id;
        let [flooded_session] = live_sessions(&service).try_into().ok().expect("one session");
        let quiet = connect(&service);
        let quiet_context = quiet.info().root_context_id;
        let quiet_session = live_sessions(&service)
            .into_iter()
            .find(|session| session.identity != flooded_session.identity)
            .expect("the second session");

        // The flooded session's own tag, with an authenticator derived from somebody else's key:
        // the lookup succeeds, so every one of these reaches verification and fails it.
        let forged = marker_body(
            &AnchorKey::new([0x5a; 32]),
            &flooded_session.session_tag,
            flooded_context,
            1,
        );
        let attempts = 16 * MARKER_ADMISSION_RATE;
        for _ in 0..attempts {
            service.handle_terminal_marker(&forged, 0, 0, false);
        }

        let admitted = lock(&flooded_session.markers).admitted;
        assert!(
            admitted <= 4 * MARKER_ADMISSION_RATE,
            "{admitted} of {attempts} forged markers were verified"
        );
        assert_eq!(anchors_owned_by(&service, &flooded_session), 0, "no forgery became an anchor");

        // The other session's budget is its own, and its markers still work.
        assert_eq!(lock(&quiet_session.markers).admitted, 0, "a flood spends no other budget");
        let genuine =
            marker_body(&quiet_session.anchor_key, &quiet_session.session_tag, quiet_context, 7);
        service.handle_terminal_marker(&genuine, 3, 5, false);
        assert_eq!(
            anchors_owned_by(&service, &quiet_session),
            1,
            "the untouched session registers its anchor as usual"
        );
    }

    /// A session that leaves takes its marker tag with it and nothing else.
    #[test]
    fn a_departing_session_unpublishes_only_its_own_marker_tag() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let leaving = connect(&service);
        let [leaving_session] = live_sessions(&service).try_into().ok().expect("one session");
        let staying = connect(&service);
        let staying_context = staying.info().root_context_id;
        let staying_session = live_sessions(&service)
            .into_iter()
            .find(|session| session.identity != leaving_session.identity)
            .expect("the second session");

        {
            let registry = lock(&service.shared.registry);
            for session in [&leaving_session, &staying_session] {
                assert_eq!(
                    registry.session_by_tag(&session.session_tag).map(|found| found.identity),
                    Some(session.identity),
                    "a live session is reachable by the tag its markers carry"
                );
            }
        }

        leaving.close().expect("GOODBYE is answered");
        assert!(
            wait_until(Duration::from_secs(2), || {
                lock(&service.shared.registry).sessions.len() == 1
            }),
            "the departed session leaves the registry"
        );

        let registry = lock(&service.shared.registry);
        assert!(
            registry.session_by_tag(&leaving_session.session_tag).is_none(),
            "the departed tag resolves to nothing"
        );
        assert_eq!(
            registry.session_by_tag(&staying_session.session_tag).map(|found| found.identity),
            Some(staying_session.identity),
            "the surviving session's tag is untouched"
        );
        drop(registry);

        let genuine = marker_body(
            &staying_session.anchor_key,
            &staying_session.session_tag,
            staying_context,
            9,
        );
        service.handle_terminal_marker(&genuine, 1, 2, false);
        assert_eq!(anchors_owned_by(&service, &staying_session), 1);
    }

    /// The marker budget is generous to producers and finite to floods.
    #[test]
    fn marker_admission_allows_a_whole_anchor_set_at_once_and_then_bounds_the_rate() {
        let start = Instant::now();
        let mut admission = MarkerAdmission::new(start);

        // A producer registering its complete anchor set in one burst is never made to wait.
        for index in 0..MAX_ACTIVE_ANCHORS {
            assert!(admission.admit(start), "marker {index} of a full anchor set was refused");
        }
        assert!(!admission.admit(start), "the burst is the whole budget, not the start of it");
        assert_eq!(admission.admitted, MAX_ACTIVE_ANCHORS as u64);

        // And the budget returns at the stated rate rather than all at once.
        let quarter = start + Duration::from_millis(250);
        for _ in 0..MARKER_ADMISSION_RATE / 4 {
            assert!(admission.admit(quarter));
        }
        assert!(!admission.admit(quarter), "a quarter second buys a quarter of the rate");
    }

    /// A local process that opens connections and says nothing must not close the endpoint.
    ///
    /// Every accepted connection costs a slot and a thread from the moment it arrives — before its
    /// peer has proved anything at all, and on Windows the endpoint is a loopback listener any
    /// process can reach. Unauthenticated connections are therefore bounded among themselves, and
    /// the bound is enforced by evicting the oldest silent peer rather than by turning the newest
    /// arrival away, so the flood displaces itself and not a producer.
    #[test]
    fn silent_connections_cannot_lock_a_producer_out_of_the_endpoint() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let endpoint = service.control_endpoint().to_owned();

        // Fill the unauthenticated budget and add one more silent peer. This is the smallest flood
        // that must evict itself; filling the unrelated global connection budget only creates 64
        // competing OS threads and turns this synchronization test into a scheduler benchmark.
        let silent_count = MAX_PENDING_CONNECTIONS + 1;
        let mut silent = Vec::new();
        for _ in 0..silent_count {
            match connect_endpoint(&endpoint) {
                Ok(stream) => silent.push(stream),
                Err(error) => panic!("the endpoint stopped accepting connections: {error}"),
            }
        }

        assert!(
            wait_until(Duration::from_secs(5), || {
                let pending = lock(&service.shared.pending_handshakes);
                pending.next_id >= silent_count as u64
                    && pending.open.len() <= MAX_PENDING_CONNECTIONS
            }),
            "unauthenticated connections stay bounded among themselves"
        );
        assert!(
            wait_until(Duration::from_secs(5), || {
                service.shared.active_connections.load(Ordering::Acquire) <= MAX_PENDING_CONNECTIONS
            }),
            "a peer that has not spoken cannot hold a producer's connection slot or its thread"
        );

        let evicted = silent_count - MAX_PENDING_CONNECTIONS;
        assert!(
            wait_until(Duration::from_secs(2), || {
                silent.iter().filter(|stream| is_closed_now(stream)).count() >= evicted
            }),
            "all but the bounded pending set are shut down, not merely uncounted"
        );

        // The point of the bound: a real producer is served while the flood is still live.
        let session = connect(&service);
        assert!(
            session.query_session().is_ok(),
            "a legitimate producer connects and is served during a flood of silent sockets"
        );
        drop(silent);
    }

    fn wait_until(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    #[test]
    fn an_unclean_loss_suspends_a_resumable_lease_and_resume_restores_it() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut controller = connect(&service);
        let context_id = controller.info().root_context_id;
        let secret = Secret32::new([0x61; 32]);
        controller
            .create_session_lease(
                &resumable_definition(context_id, 11, &secret),
                &RequestMetadata::default(),
            )
            .unwrap();

        let mut child = RawClient::activate(&service, context_id, 11, &secret).unwrap();
        let child_session = child.session_id;
        let resume_key = Secret32::new(*child.resume_key.expose());
        let tag = child.session_tag;
        // A post-`HELLO` record closes the activation-retry window, so the only recovery left is
        // the resume proof (security §6.4 step 6).
        child.query_session().unwrap();
        drop(child);

        let identity = SessionIdentity::new(service.shared.presenter, child_session).unwrap();
        assert!(
            wait_until(Duration::from_secs(2), || {
                lock(&service.shared.registry).sessions.len() == 1
            }),
            "the lost child leaves the session registry"
        );
        assert!(
            service.scene.is_registered(identity),
            "security §7.1 retains the suspended session's object state"
        );
        assert_eq!(lock(&service.shared.registry).leases.len(), 1, "the lease is still charged");

        let mut resumed =
            RawClient::resume(&service, context_id, 11, child_session, 0, &resume_key).unwrap();
        assert_eq!(resumed.session_id, child_session, "the logical session survives the loss");
        assert_ne!(resumed.session_tag, tag, "resume derives fresh key material");
        assert!(resumed.query_session().is_ok(), "the resumed session serves control traffic");
    }

    #[test]
    fn a_consumed_resume_cannot_be_replayed() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut controller = connect(&service);
        let context_id = controller.info().root_context_id;
        let secret = Secret32::new([0x62; 32]);
        controller
            .create_session_lease(
                &resumable_definition(context_id, 12, &secret),
                &RequestMetadata::default(),
            )
            .unwrap();
        let mut child = RawClient::activate(&service, context_id, 12, &secret).unwrap();
        let child_session = child.session_id;
        let resume_key = Secret32::new(*child.resume_key.expose());
        child.query_session().unwrap();
        drop(child);
        assert!(wait_until(Duration::from_secs(2), || {
            lock(&service.shared.registry).sessions.len() == 1
        }));

        let first =
            RawClient::resume(&service, context_id, 11 + 1, child_session, 0, &resume_key).unwrap();
        assert_eq!(first.session_id, child_session);
        // Security §7.2: competing resume attempts cannot both advance the generation, so the
        // same proof at generation zero is now stale.
        assert!(
            RawClient::resume(&service, context_id, 12, child_session, 0, &resume_key).is_err(),
            "a consumed resume proof must not open a second session"
        );
    }

    #[test]
    fn a_wrong_resume_proof_is_refused() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut controller = connect(&service);
        let context_id = controller.info().root_context_id;
        let secret = Secret32::new([0x65; 32]);
        controller
            .create_session_lease(
                &resumable_definition(context_id, 15, &secret),
                &RequestMetadata::default(),
            )
            .unwrap();
        let mut child = RawClient::activate(&service, context_id, 15, &secret).unwrap();
        let child_session = child.session_id;
        child.query_session().unwrap();
        drop(child);
        assert!(wait_until(Duration::from_secs(2), || {
            lock(&service.shared.registry).sessions.len() == 1
        }));

        assert!(
            RawClient::resume(
                &service,
                context_id,
                15,
                child_session,
                0,
                &Secret32::new([0xff; 32]),
            )
            .is_err(),
            "a resume proof under the wrong key must be refused"
        );
    }

    #[test]
    fn a_lease_without_grace_closes_instead_of_suspending() {
        // Cleanup policy zero closes immediately, even on an unclean loss (security §7.1).
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut controller = connect(&service);
        let context_id = controller.info().root_context_id;
        let secret = Secret32::new([0x63; 32]);
        controller
            .create_session_lease(
                &lease_definition(context_id, 13, &secret),
                &RequestMetadata::default(),
            )
            .unwrap();
        let mut child = RawClient::activate(&service, context_id, 13, &secret).unwrap();
        let child_session = child.session_id;
        let resume_key = Secret32::new(*child.resume_key.expose());
        child.query_session().unwrap();
        drop(child);

        let identity = SessionIdentity::new(service.shared.presenter, child_session).unwrap();
        assert!(
            wait_until(Duration::from_secs(2), || {
                let lease_released = lock(&service.shared.registry).leases.len() == 0;
                lease_released && !service.scene.is_registered(identity)
            }),
            "an immediate cleanup policy releases the lease and its retained state"
        );
        assert!(
            RawClient::resume(&service, context_id, 13, child_session, 0, &resume_key).is_err(),
            "a closed lease cannot resume"
        );
    }

    #[test]
    fn a_suspended_lease_is_released_when_its_grace_expires() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut controller = connect(&service);
        let context_id = controller.info().root_context_id;
        let secret = Secret32::new([0x66; 32]);
        let mut definition = resumable_definition(context_id, 16, &secret);
        definition.requested_disconnect_grace_us = 50_000;
        controller.create_session_lease(&definition, &RequestMetadata::default()).unwrap();

        let mut child = RawClient::activate(&service, context_id, 16, &secret).unwrap();
        let child_session = child.session_id;
        child.query_session().unwrap();
        drop(child);

        // Grace expiry performs final owner-scoped cleanup, releasing the reservation.
        let identity = SessionIdentity::new(service.shared.presenter, child_session).unwrap();
        assert!(
            wait_until(Duration::from_secs(3), || {
                let lease_released = lock(&service.shared.registry).leases.len() == 0;
                lease_released && !service.scene.is_registered(identity)
            }),
            "the grace deadline releases a suspended lease and its retained state"
        );
    }

    #[test]
    fn an_authenticated_interactive_lane_opens_and_answers_ping() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let session = connect(&service);
        // The SDK opens a lane only when `desktop-input-v1` was accepted, which a terminal target
        // does not offer, so drive the wire directly.
        let mut lane = RawLane::open(&service, &session, 1).unwrap();
        assert!(lane.ping().is_ok(), "the lane is serviced independently of control");

        // Core §7: only one transport per generation.
        assert!(
            RawLane::open(&service, &session, 1).is_err(),
            "a duplicate open while the first is live must be refused"
        );

        drop(lane);
        // After confirmed loss the same generation may be reopened, because no input was admitted.
        assert!(
            wait_until(Duration::from_secs(2), || { RawLane::open(&service, &session, 1).is_ok() }),
            "an exact retry after loss is accepted"
        );
        // A lower generation is always refused.
        assert!(RawLane::open(&service, &session, 0).is_err());
    }

    #[test]
    fn a_lane_with_a_bad_tag_or_unknown_session_is_refused() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let session = connect(&service);
        assert!(
            RawLane::open_with_key(&service, session.info().session_id, 1, &Secret32::new([0; 32]))
                .is_err(),
            "a tag under the wrong key must not authenticate"
        );
        assert!(
            RawLane::open_with_key(&service, 9999, 1, &Secret32::new([0; 32])).is_err(),
            "an unknown session must not authenticate"
        );
    }

    #[test]
    fn the_lane_refuses_records_it_does_not_carry() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let session = connect(&service);
        let mut lane = RawLane::open(&service, &session, 1).unwrap();
        // A control record on the interactive lane is a framing error, not a mutation.
        assert!(
            lane.send(messages::CREATE_SURFACE, &Envelope::new(1, Vec::new()).encode().unwrap())
                .and_then(|()| lane.read())
                .is_ok_and(|record| record.record_type == messages::ERROR),
            "the lane rejects a record it does not carry"
        );
        // The control session is untouched by that.
        assert!(session.query_session().is_ok());
    }

    /// An interactive-lane connection driven at the wire.
    struct RawLane {
        stream: LocalStream,
        sequence: u64,
    }

    impl RawLane {
        fn open(
            service: &VividService,
            session: &vivid_sdk::Session,
            generation: u64,
        ) -> io::Result<Self> {
            // The SDK derives the same channel key from the handshake it completed.
            let key = session.channel_key();
            Self::open_with_key(service, session.info().session_id, generation, &key)
        }

        fn open_with_key(
            service: &VividService,
            session_id: u64,
            generation: u64,
            channel_key: &Secret32,
        ) -> io::Result<Self> {
            let mut stream = connect_endpoint(service.control_endpoint())?;
            stream.write_all(&vivid_protocol::wire::encode_preface(
                ConnectionKind::Lane,
                LANE_MAX_RECORD_BODY,
            ))?;
            let nonce = [0x2d_u8; 16];
            let tag = auth::lane_tag(
                channel_key.expose(),
                session_id,
                LaneClass::Interactive as u32,
                generation,
                &nonce,
            );
            let body = Envelope::new(
                1,
                vec![
                    (0, Value::Unsigned(session_id)),
                    (1, Value::Unsigned(LaneClass::Interactive as u64)),
                    (2, Value::Unsigned(generation)),
                    (3, Value::Bytes(nonce.to_vec())),
                    (4, Value::Bytes(tag.to_vec())),
                ],
            )
            .encode()
            .map_err(io::Error::other)?;
            write_raw(&mut stream, 1, messages::LANE_OPEN, 0, &body)?;
            let mut lane = Self { stream, sequence: 1 };
            let record = lane.read()?;
            if record.record_type != messages::LANE_ACCEPTED {
                return Err(io::Error::other("lane open refused"));
            }
            Ok(lane)
        }

        fn send(&mut self, record_type: u16, body: &[u8]) -> io::Result<()> {
            self.sequence += 1;
            write_raw(&mut self.stream, self.sequence, record_type, 0, body)
        }

        fn read(&mut self) -> io::Result<vivid_protocol::wire::Record> {
            read_raw(&mut self.stream)
        }

        fn bind(
            &mut self,
            context_id: u64,
            surface_id: u64,
            epoch: u64,
            surface_generation: u64,
        ) -> io::Result<messages::PayloadMap> {
            let body = Envelope::new(
                0,
                vec![
                    (0, Value::Unsigned(epoch)),
                    (1, Value::Unsigned(context_id)),
                    (2, Value::Unsigned(surface_id)),
                    (3, Value::Unsigned(surface_generation)),
                    (4, Value::Unsigned(vivid_protocol::input::INPUT_CLASS_KEYBOARD)),
                    (5, Value::Unsigned(6)),
                    (6, Value::Unsigned(vivid_protocol::grant::DEFAULT_WATCHDOG_US)),
                ],
            )
            .encode()
            .map_err(io::Error::other)?;
            self.sequence += 1;
            write_raw(
                &mut self.stream,
                self.sequence,
                messages::SET_INPUT_BINDING,
                surface_id,
                &body,
            )?;
            let record = self.read()?;
            if record.record_type != messages::INPUT_BOUND {
                return Err(io::Error::other("expected INPUT_BOUND"));
            }
            Ok(messages::decode_control(&record.body).map_err(io::Error::other)?.payload)
        }

        fn ping(&mut self) -> io::Result<()> {
            let body = Envelope::new(7, Vec::new()).encode().map_err(io::Error::other)?;
            self.send(messages::PING, &body)?;
            let record = self.read()?;
            if record.record_type != messages::PONG {
                return Err(io::Error::other("expected PONG"));
            }
            Ok(())
        }
    }

    #[test]
    fn input_is_denied_until_the_desktop_has_presented_and_revoked_when_the_lane_dies() {
        // Desktop §5.1: enabling requires milestone 5 on the active video track's current channel
        // generation. This is the ordering claim the whole vvdesk input story rests on.
        let service = socket_service!(desktop_service());
        let mut session = connect_desktop(&service);
        let context_id = session.info().root_context_id;
        let mut definition = desktop_surface(context_id, 1920);
        definition.profile_parameters = vivid_protocol::surface::DesktopSurfaceParameters {
            captured_origin_x: 0,
            captured_origin_y: 0,
            topology: vec![],
            semantic_generation: 1,
            input_capabilities: vivid_protocol::input::INPUT_CLASS_KEYBOARD,
        }
        .encode();
        let surface = session.create_surface(definition, &RequestMetadata::default()).unwrap();

        let mut lane = RawLane::open(&service, &session, 1).unwrap();
        let denied = lane.bind(context_id, surface.id(), 1, 1).unwrap();
        assert_eq!(
            denied.iter().find(|entry| entry.0 == 6).and_then(|entry| entry.1.as_u64()),
            Some(vivid_protocol::grant::STATE_DENIED),
            "nothing has been presented, so the binding is denied"
        );

        // Present something, then a strictly greater epoch is granted.
        present_desktop_video(&service, &mut session, &surface, context_id);
        let granted = lane.bind(context_id, surface.id(), 2, 1).unwrap();
        let field = |payload: &messages::PayloadMap, key: u64| {
            payload.iter().find(|entry| entry.0 == key).and_then(|entry| entry.1.as_u64())
        };
        assert_eq!(field(&granted, 6), Some(vivid_protocol::grant::STATE_ENABLED));
        assert_eq!(
            field(&granted, 5),
            Some(vivid_protocol::input::INPUT_CLASS_KEYBOARD),
            "the grant is narrowed to the surface capability mask"
        );
        assert_eq!(field(&granted, 8), Some(vivid_protocol::grant::DEFAULT_WATCHDOG_US));

        // An event now reaches the producer, carrying the complete binding tuple.
        assert!(service.send_input(vivid_protocol::input::InputEvent::Key {
            binding: zero_tuple(),
            usage: 0x04,
            pressed: true,
        }));
        let event = lane.read().unwrap();
        assert_eq!(event.record_type, messages::KEY_INPUT);
        let payload = messages::decode_control(&event.body).unwrap().payload;
        assert_eq!(field(&payload, 0), Some(2), "the producer epoch it was granted under");
        assert_eq!(field(&payload, 3), Some(surface.id()));
        assert_eq!(field(&payload, 5), Some(0x04), "the HID usage");

        // Losing the lane revokes the grant and leaves the control session alone.
        drop(lane);
        assert!(
            wait_until(Duration::from_secs(2), || {
                !service.send_input(vivid_protocol::input::InputEvent::Key {
                    binding: zero_tuple(),
                    usage: 0x05,
                    pressed: true,
                })
            }),
            "lane loss revokes the grant"
        );
        assert!(session.query_session().is_ok(), "the control session survives lane loss");
    }

    /// A producer that stops reading its control replies must not stop the terminal.
    ///
    /// `ANCHOR_READY` and `ANCHOR_GONE` are emitted from the PTY parser thread on every marker,
    /// scroll, clear, and screen swap, and `TARGET_CHANGED` from the thread that draws the window.
    /// All three used to be blocking socket writes, so one producer declining to read froze
    /// terminal output and redraw for every window.
    #[test]
    fn a_producer_that_stops_draining_control_cannot_stall_the_terminal_or_the_window() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        // A producer that completes HELLO and then never reads another byte.
        let stalled = RawClient::root(&service).expect("root session");
        assert!(
            wait_until(Duration::from_secs(2), || {
                lock(&service.shared.registry).sessions.contains_key(&stalled.session_id)
            }),
            "the stalled producer is registered"
        );

        // Far more announcements than either the egress queue or the socket buffer can hold.
        //
        // The geometry has to actually change on every iteration: `offer_geometry` returns `None`
        // for a repeat, so re-offering the same size would queue nothing and this would pass
        // whether or not the write blocks. `flush_display_change` is what the thread that draws
        // the window calls on every frame.
        let started = Instant::now();
        for step in 0..(actor::EGRESS_CAPACITY as u32 * 4) {
            let mut geometry = test_geometry();
            geometry.viewport_width = 400 + step % 400;
            geometry.columns = geometry.viewport_width / geometry.cell_width;
            assert!(
                service.update_metrics(geometry).is_some(),
                "each step must be a real target change, or this proves nothing"
            );
            service.flush_display_change(None);
            service.handle_grid_scroll(0, 24, 1, 0);
            service.handle_screen_swap(step % 2 == 1);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "the terminal and draw paths must never block on a producer; took {elapsed:?}"
        );
    }

    /// Overflowing one producer's egress closes that producer and touches nothing else.
    ///
    /// Paced deliberately: a presenter that generates announcements faster than *any* producer can
    /// consume them will close that producer too, which is the designed bound rather than a
    /// failure. The claim here is about a peer that will not read at all, so the rate stays one a
    /// healthy peer keeps up with while the stalled one's queue still fills.
    #[test]
    fn a_stalled_producer_is_closed_without_disturbing_a_healthy_one() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut healthy = connect(&service);
        let healthy_context = healthy.info().root_context_id;
        let healthy_surface = grid_surface(&mut healthy, 1);
        let stalled = RawClient::root(&service).expect("root session");
        assert!(
            wait_until(Duration::from_secs(2), || {
                lock(&service.shared.registry).sessions.contains_key(&stalled.session_id)
            }),
            "the stalled producer is registered"
        );

        // Make the stalled write deterministic. Kernel receive-buffer capacity differs greatly:
        // Windows loopback TCP can absorb this entire burst, while the Unix-domain socket used on
        // macOS fills. Pausing only this session's worker models the same blocked socket boundary
        // and leaves the production queue/overflow/reader-shutdown path under test.
        assert!(
            wait_until(Duration::from_secs(2), || {
                let registry = lock(&service.shared.registry);
                registry
                    .sessions
                    .get(&stalled.session_id)
                    .is_some_and(|session| lock(&session.egress).is_some())
            }),
            "the stalled producer egress is installed"
        );
        let stalled_egress = {
            let registry = lock(&service.shared.registry);
            let session = registry.sessions.get(&stalled.session_id).expect("stalled session");
            lock(&session.egress).clone().expect("stalled session egress")
        };
        stalled_egress.pause_worker_for_test();

        for step in 0..(actor::EGRESS_CAPACITY as u32 * 2) {
            let mut geometry = test_geometry();
            geometry.viewport_width = 400 + step % 400;
            geometry.columns = geometry.viewport_width / geometry.cell_width;
            service.update_metrics(geometry);
            service.flush_display_change(None);
            if step % 16 == 0 {
                thread::sleep(Duration::from_micros(200));
            }
        }

        assert!(stalled_egress.overflowed(), "the stalled egress queue must overflow");

        assert!(
            wait_until(Duration::from_secs(5), || {
                !lock(&service.shared.registry).sessions.contains_key(&stalled.session_id)
            }),
            "a producer that will not read its replies is closed"
        );

        // The healthy producer still answers, and keeps every piece of state it owns.
        assert!(healthy.query_session().is_ok(), "the healthy session still answers");
        let healthy_identity =
            SessionIdentity::new(service.shared.presenter, healthy.info().session_id).unwrap();
        let status = service
            .scene
            .surface_status(SurfaceIdentity {
                context: healthy_identity.context(healthy_context).unwrap(),
                surface_id: healthy_surface.id(),
            })
            .expect("the healthy producer keeps its surface");
        assert_eq!(status.lifecycle, 1);
    }

    /// The same claim for the interactive lane, which is driven from the winit UI thread.
    ///
    /// `send_input` runs on the event loop for every keystroke and pointer motion, and
    /// `revoke_all_input` runs on focus loss. A blocking lane write there froze the whole window.
    #[test]
    fn a_producer_that_stops_draining_its_lane_cannot_stall_the_ui_thread() {
        let service = socket_service!(desktop_service());
        let mut session = connect_desktop(&service);
        let context_id = session.info().root_context_id;
        let mut definition = desktop_surface(context_id, 1920);
        definition.profile_parameters = vivid_protocol::surface::DesktopSurfaceParameters {
            captured_origin_x: 0,
            captured_origin_y: 0,
            topology: vec![],
            semantic_generation: 1,
            input_capabilities: vivid_protocol::input::INPUT_CLASS_KEYBOARD,
        }
        .encode();
        let surface = session.create_surface(definition, &RequestMetadata::default()).unwrap();
        present_desktop_video(&service, &mut session, &surface, context_id);

        // Open a lane, take the grant, then never read from it again.
        let mut lane = RawLane::open(&service, &session, 1).unwrap();
        let granted = lane.bind(context_id, surface.id(), 2, 1).unwrap();
        assert_eq!(
            granted.iter().find(|entry| entry.0 == 6).and_then(|entry| entry.1.as_u64()),
            Some(vivid_protocol::grant::STATE_ENABLED),
        );

        // Many more events than the lane egress or the socket can hold, from the thread the winit
        // event loop would be on.
        let started = Instant::now();
        for usage in 0..(actor::EGRESS_CAPACITY as u32 * 4) {
            service.send_input(vivid_protocol::input::InputEvent::Key {
                binding: zero_tuple(),
                usage: 0x04 + (usage % 8) as u16,
                pressed: usage % 2 == 0,
            });
        }
        service.revoke_all_input(vivid_protocol::grant::reason::FOCUS_LOSS);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "input delivery and revocation must never block on a producer; took {elapsed:?}"
        );

        // Losing input is the correct outcome for a producer that will not read it. The control
        // session, which is a different connection, is untouched.
        assert!(session.query_session().is_ok(), "the control session survives a stalled lane");
    }

    #[test]
    fn a_terminal_window_never_grants_desktop_input() {
        // A terminal target masks off the input operation class, so the forwarding path in the
        // keyboard handler is inert for an ordinary window.
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let _session = connect(&service);
        assert!(!service.send_input(vivid_protocol::input::InputEvent::Key {
            binding: zero_tuple(),
            usage: 0x04,
            pressed: true,
        }));
    }

    fn zero_tuple() -> vivid_protocol::input::InputTuple {
        vivid_protocol::input::InputTuple {
            producer_epoch: vivid_protocol::revision::InputEpoch::ZERO,
            grant_generation: vivid_protocol::revision::GrantGeneration::ZERO,
            context_id: 0,
            surface_id: 0,
            surface_generation: SurfaceGeneration::ZERO,
        }
    }

    /// Activate a primary-video track and mark it presented.
    ///
    /// Desktop §5.1 names the *primary-video* slot specifically, so a raster track will not do.
    /// The decoder and compositor are stood in for with the scene's own milestone seams, which is
    /// what a renderer would call: a headless test has neither.
    fn present_desktop_video(
        service: &VividService,
        session: &mut vivid_sdk::Session,
        surface: &vivid_sdk::Surface,
        context_id: u64,
    ) {
        let track = session
            .create_track(
                TrackConfiguration {
                    context_id,
                    surface_id: surface.id(),
                    track_id: 41,
                    slot: scene::SLOT_PRIMARY_VIDEO,
                    mode: TrackMode::Live,
                    lane: LaneClass::Bulk,
                    maximum_record_body: 1 << 16,
                    maximum_rate_millihertz: 30_000,
                    maximum_encoded_bits_per_second: 1_000_000,
                    maximum_records_per_second: 60,
                    maximum_inflight_body_bytes: 1 << 20,
                    kind: KindConfiguration::Video(vivid_protocol::track::VideoConfiguration {
                        codec: "h264".into(),
                        packetization: "h264-annexb-au-v1".into(),
                        extradata: Vec::new(),
                        coded_width: 16,
                        coded_height: 16,
                        profile: 77,
                        level: 10,
                        maximum_reorder_depth: 0,
                        color_primaries: 1,
                        transfer: 1,
                        matrix: 1,
                        signal_range: 1,
                        aspect_numerator: 1,
                        aspect_denominator: 1,
                        maximum_access_unit_bytes: 4096,
                        codec_string: None,
                        decoder_configuration: None,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: 1_000_000,
                    retained_pixel_charge: 0,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let identity =
            service.shared.presenter_track(session.info().session_id, context_id, surface.id(), 41);
        // Stand in for the decoder: a presentation acknowledgement names a frame that exists.
        service
            .scene
            .publish_decoded_frame(
                identity,
                ChannelGeneration::ONE,
                scene::Frame {
                    frame_id: 1,
                    pts_us: 0,
                    width: 2,
                    height: 2,
                    sar_num: 1,
                    sar_den: 1,
                    alpha_mode: scene::ALPHA_STRAIGHT,
                    rgba: Arc::new(RgbaBuffer::new(vec![0xff_u8; 16])),
                    damage: None,
                },
            )
            .unwrap();
        service.scene.mark_output_ready(identity, ChannelGeneration::ONE).unwrap();
        session
            .create_node(
                &SceneNode {
                    owning_context_id: context_id,
                    node_id: 42,
                    surface_context_id: context_id,
                    surface_id: surface.id(),
                    geometry: vivid_protocol::geometry::NodeGeometry::full_target().encode(),
                    fit: vivid_sdk::Fit::Contain,
                    linear_sampling: true,
                    z_index: 0,
                    visible: true,
                    opacity: u16::MAX,
                    clip: None,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        session
            .activate_tracks(
                surface,
                &[SlotBinding {
                    slot: scene::SLOT_PRIMARY_VIDEO,
                    track_id: 41,
                    expected_channel_generation: ChannelGeneration::ONE,
                    required_milestone: MILESTONE_OUTPUT_READY,
                }],
                &RequestMetadata::default(),
            )
            .unwrap();
        service
            .scene
            .mark_presented(identity, ChannelGeneration::ONE, surface.generation(), 1, 0)
            .unwrap();
        drop(track);
    }

    #[test]
    fn a_subscribed_session_observes_its_own_mutations() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut session = connect(&service);
        let context_id = session.info().root_context_id;
        session.set_observation(observation::class::SURFACE).unwrap();

        let surface = session
            .create_surface(
                SurfaceDefinition {
                    context_id,
                    surface_id: 51,
                    semantic_profile: registry::TERMINAL_CONTENT.into(),
                    coordinate_model: CoordinateModel::TerminalContentCells,
                    logical_width: 2,
                    logical_height: 2,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                    descriptor: SurfaceDescriptor {
                        role: SurfaceRole::Figure,
                        title: "observed".into(),
                        semantic_content_revision: 1,
                        semantic_availability: 0,
                        locator_hint: String::new(),
                    },
                    policy: 0,
                    profile_parameters: vec![],
                },
                &RequestMetadata::default(),
            )
            .unwrap();

        let observed = wait_for_event(&session, messages::SURFACE_CHANGED)
            .expect("a subscribed surface mutation is observed");
        let field = |key: u64| observed.iter().find(|e| e.0 == key).and_then(|e| e.1.as_u64());
        assert_eq!(field(1), Some(surface.id()));
        assert_eq!(field(4), Some(SURFACE_CHANGED_LIFECYCLE));
        assert_eq!(
            field(vivid_protocol::observation::OBSERVATION_SEQUENCE_KEY),
            Some(1),
            "observations carry a strictly increasing sequence"
        );
    }

    #[test]
    fn an_unsubscribed_session_observes_nothing() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut session = connect(&service);
        let context_id = session.info().root_context_id;
        // No SET_OBSERVATION: the default mask is zero, so nothing is queued at all.
        session
            .create_surface(
                SurfaceDefinition {
                    context_id,
                    surface_id: 52,
                    semantic_profile: registry::TERMINAL_CONTENT.into(),
                    coordinate_model: CoordinateModel::TerminalContentCells,
                    logical_width: 2,
                    logical_height: 2,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                    descriptor: SurfaceDescriptor {
                        role: SurfaceRole::Figure,
                        title: "quiet".into(),
                        semantic_content_revision: 1,
                        semantic_availability: 0,
                        locator_hint: String::new(),
                    },
                    policy: 0,
                    profile_parameters: vec![],
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(60));
        assert!(
            wait_for_event(&session, messages::SURFACE_CHANGED).is_none(),
            "an unsubscribed session receives no observations"
        );
    }

    #[test]
    fn an_unassigned_observation_class_is_refused() {
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let session = connect(&service);
        assert!(session.set_observation(1 << 9).is_err());
    }

    /// Drain session events looking for one record type, within a bounded wait.
    fn wait_for_event(
        session: &vivid_sdk::Session,
        record_type: u16,
    ) -> Option<messages::PayloadMap> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            while let Ok(Some(event)) = session.take_event() {
                if let SessionEvent::Other { record_type: seen, payload, .. } = event
                    && seen == record_type
                {
                    return Some(payload);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn an_outstanding_wait_does_not_block_the_control_reader() {
        // Core §5.1: a long operation becomes bounded pending state and must not stall the
        // parser. Registering a wait that cannot be satisfied used to be harmless only because
        // it spawned a thread; the actor now holds it, so prove the session still answers.
        let service =
            socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|| {})));
        let mut session = connect(&service);
        let context_id = session.info().root_context_id;
        let surface = session
            .create_surface(
                SurfaceDefinition {
                    context_id,
                    surface_id: 11,
                    semantic_profile: registry::TERMINAL_CONTENT.into(),
                    coordinate_model: CoordinateModel::TerminalContentCells,
                    logical_width: 2,
                    logical_height: 2,
                    scale_numerator: 1,
                    scale_denominator: 1,
                    rotation: 0,
                    descriptor: SurfaceDescriptor {
                        role: SurfaceRole::Figure,
                        title: "pending wait".into(),
                        semantic_content_revision: 1,
                        semantic_availability: 0,
                        locator_hint: String::new(),
                    },
                    policy: 0,
                    profile_parameters: vec![],
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        let track = session
            .create_track(
                TrackConfiguration {
                    context_id,
                    surface_id: surface.id(),
                    track_id: 12,
                    slot: scene::SLOT_RASTER,
                    mode: TrackMode::Live,
                    lane: LaneClass::Bulk,
                    maximum_record_body: 88,
                    maximum_rate_millihertz: 60_000,
                    maximum_encoded_bits_per_second: 1_000_000,
                    maximum_records_per_second: 60,
                    maximum_inflight_body_bytes: 4096,
                    kind: KindConfiguration::Raster(RasterConfiguration {
                        width: 2,
                        height: 2,
                        alpha_mode: scene::ALPHA_STRAIGHT,
                        delta_enabled: false,
                        maximum_delta_operations: 1,
                        zstd_enabled: false,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: 1_000_000,
                    retained_pixel_charge: 0,
                },
                &RequestMetadata::default(),
            )
            .unwrap();

        // Nothing has ever been presented, so this milestone cannot be reached and the wait sits
        // in the actor's pending set until it times out.
        let session = Arc::new(session);
        let waiter = {
            let session = session.clone();
            let track = track.clone();
            thread::spawn(move || {
                session.wait_track(
                    &track,
                    TrackWaitCondition::MilestoneSet,
                    Some(MILESTONE_PRESENTED),
                    2_000_000,
                )
            })
        };

        // Give the wait time to reach the presenter, then keep asking unrelated questions.
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        for _ in 0..20 {
            session.query_surface(&surface).unwrap();
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "20 queries took {elapsed:?} while a wait was outstanding"
        );

        // The wait still resolves on its own deadline rather than being starved.
        let outcome = waiter.join().unwrap();
        assert!(outcome.is_err(), "an unreachable milestone must time out");
    }
}
