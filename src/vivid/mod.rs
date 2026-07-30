//! Per-window Vivid Protocol 1.5 presenter.

mod actor;
mod audio;
mod decoder;
pub mod scene;
pub mod target;
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
    ContextDefinition, ContextState, OP_SURFACE_TRACK_MEDIA, OP_TERMINAL_ANCHOR,
};
use vivid_protocol::identity::{
    AnchorIdentity, ContextIdentity, PresenterInstanceId, SessionIdentity, SurfaceIdentity,
    TrackIdentity,
};
use vivid_protocol::media;
use vivid_protocol::messages::{
    self, ChannelOpen, Envelope, ErrorDetail, ErrorReply, Hello, HelloAuthentication, StrictMap,
    Welcome, WelcomeAuthentication,
};
use vivid_protocol::registry;
use vivid_protocol::resource::{Resource, ResourceContract, TokenBucket};
use vivid_protocol::revision::{
    ChannelGeneration, SceneRevision, SurfaceGeneration, SurfaceRevision, TargetGeneration,
};
use vivid_protocol::scene::SceneNode;
use vivid_protocol::surface::{SurfaceDefinition, SurfaceDescriptor};
use vivid_protocol::track::{
    ChannelOpenDecision, ChannelOpenState, KindConfiguration, TrackConfiguration,
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
use crate::vivid::scene::{
    CommitRejection, Frame, SceneStatus, SharedScene, SurfaceStatus, TrackStatus,
    TrackWaitEvaluation, TrackWaitSatisfied,
};
pub use crate::vivid::target::DisplayGeometry;
use crate::vivid::target::{DesktopTarget, PresentationTarget, TerminalTarget};
use crate::vivid::transport::{ReadShutdown, Reader, Writer};

#[cfg(windows)]
type LocalListener = TcpListener;
#[cfg(windows)]
type LocalStream = TcpStream;
#[cfg(unix)]
type LocalListener = UnixListener;
#[cfg(unix)]
type LocalStream = UnixStream;

const MAX_SESSIONS: usize = 16;
const MAX_CONNECTIONS: usize = 64;
const MAX_ACTIVE_ANCHORS: usize = 4096;
const MAX_SEEN_ANCHORS: usize = 8192;
const CHANNEL_FLOW_BYTES: u64 = 8 * 1024 * 1024;
const CHANNEL_FLOW_RECORDS: u64 = 128;
const CHANNEL_OPEN_DEADLINE_US: u64 = 30_000_000;
const MAX_SCENE_NODES: usize = 256;

struct SessionRuntime {
    identity: SessionIdentity,
    root_context: ContextIdentity,
    session_tag: [u8; 16],
    channel_key: Secret32,
    anchor_key: AnchorKey,
    writer: Arc<Writer>,
    contexts: Mutex<HashMap<u64, ContextState>>,
    seen_anchors: Mutex<HashSet<(u64, u64)>>,
}

#[derive(Default)]
struct Registry {
    sessions: HashMap<u64, Arc<SessionRuntime>>,
    channel_opens: HashMap<TrackIdentity, ChannelOpenState>,
}

struct ServiceShared {
    root_secret: Secret32,
    presenter: PresenterInstanceId,
    scene: SharedScene,
    registry: Mutex<Registry>,
    audio_outputs: Mutex<HashMap<TrackIdentity, Arc<AudioOutput>>>,
    next_session: AtomicU64,
    active_connections: AtomicUsize,
    wake: Arc<dyn Fn() + Send + Sync>,
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
            wake,
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
        let writers = lock(&self.shared.registry)
            .sessions
            .values()
            .map(|session| session.writer.clone())
            .collect::<Vec<_>>();
        for writer in writers {
            if let Err(error) = writer.write_record(messages::TARGET_CHANGED, 0, &body) {
                log::debug!("could not send TARGET_CHANGED: {error}");
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
        let session = lock(&self.shared.registry)
            .sessions
            .values()
            .find(|session| session.session_tag == marker.session_tag)
            .cloned();
        let Some(session) = session else {
            return;
        };
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
        let _ = session.writer.write_record(messages::ANCHOR_READY, marker.anchor_id, &body);
        (self.shared.wake)();
    }

    pub fn handle_grid_scroll(&self, origin: i32, end: i32, lines: i32, history_size: usize) {
        let removed = self.scene.scroll_anchors(origin, end, lines, history_size);
        self.notify_anchor_events(messages::ANCHOR_GONE, &removed);
        (self.shared.wake)();
    }

    pub fn handle_terminal_clear(&self) {
        let removed = self.scene.clear_terminal();
        self.notify_anchor_events(messages::ANCHOR_GONE, &removed);
        (self.shared.wake)();
    }

    pub fn handle_screen_swap(&self, alternate: bool) {
        let removed = self.scene.set_alternate_screen(alternate);
        self.notify_anchor_events(messages::ANCHOR_GONE, &removed);
        (self.shared.wake)();
    }

    pub fn update_visibility(&self, _visible: bool, _display_offset: usize) {}

    #[cfg(unix)]
    pub(crate) fn automation_sessions(&self) -> Vec<SessionIdentity> {
        self.scene.session_ids()
    }

    #[cfg(unix)]
    pub(crate) fn automation_surface_keys(&self) -> Vec<SurfaceIdentity> {
        self.scene.surface_keys()
    }

    #[cfg(unix)]
    pub(crate) fn automation_surface_status(
        &self,
        identity: SurfaceIdentity,
    ) -> Option<SurfaceStatus> {
        self.scene.surface_status(identity)
    }

    #[cfg(unix)]
    pub(crate) fn automation_track_keys(&self) -> Vec<TrackIdentity> {
        self.scene.track_keys()
    }

    #[cfg(unix)]
    pub(crate) fn automation_track_status(&self, identity: TrackIdentity) -> Option<TrackStatus> {
        self.scene.track_status(identity)
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
    pub(crate) fn automation_evaluate_wait(
        &self,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        condition: u64,
        value: Option<u64>,
    ) -> TrackWaitEvaluation {
        self.scene.evaluate_track_wait(identity, generation, condition, value)
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
                let _ = session.writer.write_record(record_type, identity.anchor_id, &body);
            }
        }
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
        if shared.active_connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            shared.active_connections.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let shared = shared.clone();
        let _ = thread::Builder::new().name("vivid-1.5-connection".into()).spawn(move || {
            if let Err(error) = handle_connection(stream, &shared) {
                log::debug!("Vivid connection closed: {error}");
            }
            shared.active_connections.fetch_sub(1, Ordering::AcqRel);
        });
    }
}

fn handle_connection(stream: LocalStream, shared: &Arc<ServiceShared>) -> io::Result<()> {
    let (mut reader, preface, preface_bytes) = Reader::new(stream)?;
    match preface.kind {
        ConnectionKind::Control => handle_control(&mut reader, &preface_bytes, shared),
        ConnectionKind::Track => handle_track_channel(&mut reader, shared),
        ConnectionKind::Lane => reject_lane(&mut reader),
    }
}

fn handle_control(
    reader: &mut Reader,
    preface: &[u8; 16],
    shared: &Arc<ServiceShared>,
) -> io::Result<()> {
    let writer = Arc::new(reader.writer(ConnectionKind::Control)?);
    let first = reader.read_record(ConnectionKind::Control)?;
    let (hello_request, hello) = Hello::decode(&first.body)?;
    let session = establish_root_session(shared, writer.clone(), preface, &hello, hello_request)?;
    reader.set_maximum(hello.maximum_control_body)?;
    writer.set_maximum(hello.maximum_control_body)?;

    // A session is a reader, an actor, and an egress. This thread is the reader: it parses and
    // enqueues, and never writes, so a peer that stops draining its replies cannot stall parsing.
    let egress = Egress::start(writer);
    let (records, incoming) = mpsc::sync_channel::<Record>(actor::INGRESS_CAPACITY);
    let clean_goodbye = Arc::new(AtomicBool::new(false));
    let shutdown = reader.shutdown_handle()?;
    let actor = {
        let shared = shared.clone();
        let session = session.clone();
        let egress = egress.clone();
        let clean_goodbye = clean_goodbye.clone();
        thread::Builder::new()
            .name("vivid-control-actor".into())
            .spawn(move || actor_loop(shared, session, incoming, egress, clean_goodbye, shutdown))?
    };

    loop {
        let record = match reader.read_record(ConnectionKind::Control) {
            Ok(record) => record,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => {
                drop(records);
                let _ = actor.join();
                egress.close();
                egress.join();
                finish_session(shared, &session, false);
                return Err(error);
            },
        };
        if records.send(record).is_err() {
            break;
        }
    }
    drop(records);
    let _ = actor.join();
    egress.close();
    egress.join();
    if egress.overflowed() {
        log::debug!(
            "Vivid session {} closed: the producer stopped draining its control replies",
            session.identity.session_id
        );
    }
    let clean_goodbye = clean_goodbye.load(Ordering::Acquire);
    finish_session(shared, &session, clean_goodbye);
    Ok(())
}

/// The session actor: owns mutable session state, applies mutations in receive order, and services
/// outstanding operations on a tick so a long one never blocks the next record.
fn actor_loop(
    shared: Arc<ServiceShared>,
    session: Arc<SessionRuntime>,
    incoming: mpsc::Receiver<Record>,
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
    loop {
        match incoming.recv_timeout(actor::TICK) {
            Ok(record) => {
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
            Err(mpsc::RecvTimeoutError::Timeout) => {},
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !pending.is_empty() {
            pending.service(&shared.scene, &egress, &mut cancelled, Instant::now());
        }
    }
    egress.close();
    // Release the reader, which is parked on a peer that has no obligation to close promptly.
    shutdown.stop();
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

fn finish_session(shared: &Arc<ServiceShared>, session: &Arc<SessionRuntime>, clean: bool) {
    lock(&shared.registry).sessions.remove(&session.identity.session_id);
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
    (shared.wake)();
}

fn establish_root_session(
    shared: &Arc<ServiceShared>,
    writer: Arc<Writer>,
    preface: &[u8; 16],
    hello: &Hello,
    request_id: u64,
) -> io::Result<Arc<SessionRuntime>> {
    let proof = match &hello.authentication {
        HelloAuthentication::Root { proof } => proof,
        _ => {
            writer.write_record(
                messages::ERROR,
                0,
                &protocol_error(
                    request_id,
                    messages::ERROR_AUTH_FAILED,
                    true,
                    "Vivido accepts root authentication only on a new terminal control session",
                )?,
            )?;
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "unsupported session authentication",
            ));
        },
    };
    let authless = hello.authless_payload()?;
    if !auth::verify_root_hello_proof(&shared.root_secret, preface, &authless, proof) {
        writer.write_record(
            messages::ERROR,
            0,
            &protocol_error(
                request_id,
                messages::ERROR_AUTH_FAILED,
                true,
                "root authentication failed",
            )?,
        )?;
        return Err(io::Error::new(ErrorKind::PermissionDenied, "root authentication failed"));
    }
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
    let identity = SessionIdentity::new(shared.presenter, session_id).map_err(io::Error::other)?;
    let root_context = identity.context(1).map_err(io::Error::other)?;
    let mut server_nonce = [0_u8; auth::NONCE_BYTES];
    let mut session_tag = [0_u8; messages::SESSION_TAG_BYTES];
    getrandom::fill(&mut server_nonce).map_err(io::Error::other)?;
    getrandom::fill(&mut session_tag).map_err(io::Error::other)?;
    let prk = auth::extract_handshake_prk(
        &shared.root_secret,
        &hello.client_nonce,
        &server_nonce,
        &[0; 32],
    );
    let (keys, anchor_key) = auth::derive_session_keys(&prk, session_id, 0, &session_tag);

    let mut welcome = Welcome {
        session_id,
        session_tag,
        root_context_id: root_context.context_id,
        target_generation: target.generation(),
        target_profile: target.profile_name().into(),
        target_descriptor: target.descriptor(),
        accepted_profiles: accepted,
        maximum_control_body: hello
            .maximum_control_body
            .min(vivid_protocol::CONTROL_MAX_RECORD_BODY),
        server_nonce,
        authentication: WelcomeAuthentication {
            kind: messages::AUTHENTICATION_ROOT,
            confirmation: [0; 32],
            lease_state: 0,
            activation_attempt_status: 0,
        },
        session_revision: 1,
        scene_revision: 0,
        resource_contract: presenter_contract(),
        establishment_state: 0,
        resume_generation: 0,
        extensions: vec![],
    };
    welcome.confirm(&prk)?;
    writer.write_record(messages::WELCOME, 0, &welcome.encode(request_id)?)?;
    let runtime = Arc::new(SessionRuntime {
        identity,
        root_context,
        session_tag,
        channel_key: Secret32::new(*keys.channel_key()),
        anchor_key,
        writer,
        contexts: Mutex::new(HashMap::from([(
            root_context.context_id,
            ContextState::root(
                identity,
                root_context.context_id,
                target.root_operation_classes(),
                presenter_contract(),
            )
            .map_err(io::Error::other)?,
        )])),
        seen_anchors: Mutex::new(HashSet::new()),
    });
    shared
        .scene
        .register_session(identity, TargetGeneration::new(target.generation()))
        .map_err(io::Error::other)?;
    registry.sessions.insert(session_id, runtime.clone());
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
        messages::QUERY_SESSION => (
            messages::SESSION_STATUS,
            0,
            Envelope::new(
                request_id,
                vec![
                    (0, Value::Unsigned(session.identity.session_id)),
                    (1, Value::Bytes(session.session_tag.to_vec())),
                    (2, Value::Unsigned(session.root_context.context_id)),
                    (3, Value::Unsigned(shared.scene.target().generation())),
                    (4, Value::Unsigned(1)),
                    (5, Value::Unsigned(0)),
                ],
            )
            .encode(),
        ),
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
            let _status = shared
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
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::DESTROY_SURFACE => {
            let identity = payload_surface_identity(session, &value)?;
            shared.scene.destroy_surface(identity).map_err(ControlError::state)?;
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
            if !supports_track(&configuration) {
                return Err(ControlError::unsupported("track configuration is unsupported"));
            }
            let identity = track_identity(
                session,
                configuration.context_id,
                configuration.surface_id,
                configuration.track_id,
            )?;
            let status =
                shared.scene.create_track(identity, configuration).map_err(ControlError::state)?;
            (
                messages::TRACK_READY,
                record.object_id,
                Envelope::new(
                    request_id,
                    vec![
                        (0, Value::Unsigned(identity.surface.context.context_id)),
                        (1, Value::Unsigned(identity.surface.surface_id)),
                        (2, Value::Unsigned(identity.track_id)),
                        (3, Value::Unsigned(status.state.revision.get())),
                        (4, Value::Unsigned(status.state.channel_generation.get())),
                        (5, Value::Unsigned(CHANNEL_OPEN_DEADLINE_US)),
                        (6, Value::Unsigned(u64::from(status.configuration.maximum_record_body))),
                        (7, Value::Map(status.configuration.payload(false).unwrap_or_default())),
                        (8, Value::Bool(true)),
                    ],
                )
                .encode(),
            )
        },
        messages::DESTROY_TRACK => {
            let identity = payload_track_identity(session, &value)?;
            shared.scene.destroy_track(identity).map_err(ControlError::state)?;
            if let Some(output) = lock(&shared.audio_outputs).remove(&identity) {
                output.stop();
            }
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
                Envelope::new(request_id, track_status_payload(&status)).encode(),
            )
        },
        messages::ADVANCE_CHANNEL => {
            let identity = payload_track_identity(session, &value)?;
            let map = StrictMap::new("ADVANCE_CHANNEL", &value, &[0, 1, 2, 3, 4, 5])
                .map_err(|_| ControlError::bad_message("invalid channel advance"))?;
            let current =
                map.required_u64(3).map_err(|_| ControlError::bad_message("current generation"))?;
            let next =
                map.required_u64(4).map_err(|_| ControlError::bad_message("next generation"))?;
            let status = shared
                .scene
                .track_status(identity)
                .ok_or_else(|| ControlError::not_found("track does not exist"))?;
            if status.state.channel_generation.get() != current
                || current.checked_add(1) != Some(next)
            {
                return Err(ControlError::state("channel advance is not exact"));
            }
            let status = shared.scene.advance_channel(identity).map_err(ControlError::state)?;
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
            (shared.wake)();
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
                    if let Err(error) = session.writer.write_record(
                        messages::TARGET_CHANGED,
                        0,
                        &target_change_body(&current),
                    ) {
                        log::debug!("could not re-announce the target for a stale commit: {error}");
                    }
                    return Err(ControlError::stale_target());
                },
                Err(CommitRejection::StaleRevision) => {
                    return Err(ControlError::precondition("stale scene revision"));
                },
                Err(CommitRejection::Failed(message)) => return Err(ControlError::state(message)),
            };
            (shared.wake)();
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
                    });
                },
                TrackWaitEvaluation::NotVisible => {
                    return Err(ControlError {
                        code: messages::ERROR_NOT_VISIBLE,
                        message: "track has no eligible visible placement",
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
                        },
                        AdmissionError::Requests => ControlError {
                            code: messages::ERROR_LIMIT_EXCEEDED,
                            message: "pending request capacity is exhausted",
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
            let status = shared
                .scene
                .track_status(identity)
                .ok_or_else(|| ControlError::not_found("track does not exist"))?;
            let output = lock(&shared.audio_outputs).get(&identity).cloned();
            match record.record_type {
                messages::PLAY => {
                    let map = StrictMap::new("PLAY", &value, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
                        .map_err(|_| ControlError::bad_message("invalid PLAY schema"))?;
                    let start = map
                        .required(3)
                        .map_err(|_| ControlError::bad_message("PLAY start PTS"))?
                        .as_i64()
                        .ok_or_else(|| ControlError::bad_message("PLAY start PTS"))?;
                    let minimum = map
                        .required_u64(4)
                        .map_err(|_| ControlError::bad_message("PLAY minimum buffer"))?;
                    let maximum = map
                        .required_u64(5)
                        .map_err(|_| ControlError::bad_message("PLAY maximum latency"))?;
                    let rate = map
                        .required(6)
                        .map_err(|_| ControlError::bad_message("PLAY rate"))?
                        .as_i64()
                        .ok_or_else(|| ControlError::bad_message("PLAY rate"))?;
                    let generation = map
                        .required_u64(10)
                        .map_err(|_| ControlError::bad_message("PLAY generation"))?;
                    if minimum > maximum
                        || rate != 1_i64 << 32
                        || map.required_u64(7).ok() != Some(1)
                        || map.required_u64(8).ok() != Some(0)
                        || map.required_u64(9).ok() != Some(1)
                        || generation != status.state.channel_generation.get()
                    {
                        return Err(ControlError::bad_state(
                            "PLAY policy, latency, rate, or generation is invalid",
                        ));
                    }
                    shared.scene.start_playback(identity, start).map_err(ControlError::state)?;
                    if let Some(output) = output {
                        output.configure_play(start, minimum);
                        output.start();
                    }
                },
                messages::PAUSE => {
                    shared.scene.pause_playback(identity).map_err(ControlError::state)?;
                    if let Some(output) = output {
                        output.pause();
                    }
                },
                messages::FLUSH => {
                    let epoch = StrictMap::new("FLUSH", &value, &[0, 1, 2, 3])
                        .map_err(|_| ControlError::bad_message("invalid FLUSH schema"))?
                        .required_u64(3)
                        .ok()
                        .and_then(|value| u32::try_from(value).ok())
                        .ok_or_else(|| ControlError::bad_message("FLUSH epoch"))?;
                    shared.scene.flush_playback(identity, epoch).map_err(ControlError::state)?;
                    if let Some(output) = output {
                        output.flush();
                    }
                },
                messages::DRAIN => {
                    if let Some(output) = output {
                        output.signal_eos();
                        pending
                            .register(Pending::AudioDrain {
                                request_id,
                                object_id: record.object_id,
                                identity,
                                generation: status.state.channel_generation,
                                output,
                            })
                            .map_err(|_| ControlError {
                                code: messages::ERROR_LIMIT_EXCEEDED,
                                message: "pending request capacity is exhausted",
                            })?;
                        return Ok(None);
                    }
                    shared
                        .scene
                        .mark_buffered_ended(identity, status.state.channel_generation)
                        .map_err(ControlError::state)?;
                },
                _ => unreachable!(),
            }
            (messages::OK, record.object_id, Ok(messages::ok(request_id)))
        },
        messages::SET_OBSERVATION => (messages::OK, record.object_id, Ok(messages::ok(request_id))),
        _ if record.flags & RECORD_OPTIONAL != 0 => {
            return Ok(None);
        },
        _ => {
            return Err(ControlError::unsupported(
                "record is not implemented by the terminal presentation target",
            ));
        },
    };
    let body = reply.2.map_err(|_| ControlError::bad_message("reply encoding failed"))?;
    Ok(Some((reply.0, reply.1, body)))
}

fn handle_track_channel(reader: &mut Reader, shared: &Arc<ServiceShared>) -> io::Result<()> {
    let writer = Arc::new(reader.writer(ConnectionKind::Track)?);
    let first = reader.read_record(ConnectionKind::Track)?;
    let envelope = messages::decode_control(&first.body)?;
    let request_id = envelope.request_id;
    let open = ChannelOpen::decode(first.object_id, &first.body)?;
    let session = lock(&shared.registry)
        .sessions
        .get(&open.session_id)
        .cloned()
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "session does not exist"))?;
    let identity = track_identity(&session, open.context_id, open.surface_id, open.track_id)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.message))?;
    let status = shared
        .scene
        .track_status(identity)
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "track does not exist"))?;
    if status.state.channel_generation.get() != open.channel_generation
        || status.configuration.kind.kind() != open.track_kind
        || status.configuration.lane != open.lane
    {
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
    let generation = ChannelGeneration::new(open.channel_generation);
    let maximum_bytes = CHANNEL_FLOW_BYTES.max(u64::from(status.configuration.maximum_record_body));
    let acceptance = vec![
        (0, Value::Unsigned(open.context_id)),
        (1, Value::Unsigned(open.surface_id)),
        (2, Value::Unsigned(open.track_id)),
        (3, Value::Unsigned(open.channel_generation)),
        (4, Value::Unsigned(maximum_bytes)),
        (5, Value::Unsigned(CHANNEL_FLOW_RECORDS)),
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
            return Err(send_fatal(
                &writer,
                request_id,
                messages::ERROR_CHANNEL_BUSY,
                "track channel generation is already attached",
            ));
        },
        ChannelOpenDecision::DifferentBytes | ChannelOpenDecision::StaleGeneration => {
            return Err(send_fatal(
                &writer,
                request_id,
                messages::ERROR_STALE_CHANNEL_GENERATION,
                "CHANNEL_OPEN retry is stale or differs from the accepted bytes",
            ));
        },
    };
    let status = if replayed {
        shared
            .scene
            .track_status(identity)
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "track disappeared"))?
    } else {
        shared
            .scene
            .accept_channel(identity, generation, maximum_bytes, CHANNEL_FLOW_RECORDS)
            .map_err(io::Error::other)?
    };
    writer.write_record(
        messages::CHANNEL_ACCEPTED,
        open.track_id,
        &Envelope::new(request_id, acceptance).encode()?,
    )?;
    reader.set_maximum(status.configuration.maximum_record_body)?;
    let result = channel_loop(reader, &writer, shared, identity, generation);
    if let Some(state) = lock(&shared.registry).channel_opens.get_mut(&identity) {
        state.transport_lost(generation);
    }
    let _ = shared.scene.detach_channel(identity, generation);
    if let Err(error) = &result {
        stop_failed_audio_output(&shared.audio_outputs, identity);
        let _ = shared.scene.lose_track(identity);
        let status = shared.scene.track_status(identity);
        let diagnostic = error.to_string().chars().take(4_096).collect::<String>();
        let error_code = if diagnostic.contains("rate exceeded") {
            messages::ERROR_RATE_LIMITED
        } else if diagnostic.contains("flow allowance") {
            messages::ERROR_FLOW_CONTROL
        } else if matches!(
            error.kind(),
            ErrorKind::NotFound | ErrorKind::NotConnected | ErrorKind::BrokenPipe
        ) {
            messages::ERROR_DEVICE_LOST
        } else {
            messages::ERROR_DECODER
        };
        let body = Envelope::new(
            0,
            vec![
                (0, Value::Unsigned(identity.surface.context.context_id)),
                (1, Value::Unsigned(identity.surface.surface_id)),
                (2, Value::Unsigned(identity.track_id)),
                (3, Value::Unsigned(error_code)),
                (
                    4,
                    Value::Unsigned(
                        status.as_ref().map_or(1, |status| status.state.revision.get()),
                    ),
                ),
                (5, Value::Map(vec![])),
                (6, Value::Text(diagnostic)),
            ],
        )
        .encode()?;
        let _ = session.writer.write_record(messages::TRACK_LOST, identity.track_id, &body);
    }
    result
}

fn channel_loop(
    reader: &mut Reader,
    writer: &Writer,
    shared: &Arc<ServiceShared>,
    identity: TrackIdentity,
    generation: ChannelGeneration,
) -> io::Result<()> {
    let configuration = shared
        .scene
        .track_status(identity)
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "track disappeared"))?
        .configuration;
    let mut video_decoder = match &configuration.kind {
        KindConfiguration::Video(configuration) => Some(Decoder::new(configuration)?),
        _ => None,
    };
    let mut audio = match &configuration.kind {
        KindConfiguration::Audio(audio_configuration) => {
            let output = AudioOutput::open()?;
            let decoder = output.decoder(audio_configuration)?;
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
    loop {
        let record = match reader.read_record(ConnectionKind::Track) {
            Ok(record) => record,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        if record.object_id != identity.track_id {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "media record object ID does not match the track",
            ));
        }
        if matches!(
            record.record_type,
            messages::VIDEO_PACKET
                | messages::AUDIO_PACKET
                | messages::RASTER_FRAME
                | messages::IMAGE_DATA
        ) {
            pace_ingress(
                &mut byte_bucket,
                &mut record_bucket,
                &mut last_rate_update,
                u64::try_from(record.body.len()).unwrap_or(u64::MAX),
            )?;
            lock(&shared.registry)
                .channel_opens
                .get_mut(&identity)
                .ok_or_else(|| io::Error::other("channel-open state disappeared"))?
                .admit_media(generation)
                .map_err(io::Error::other)?;
        }
        match record.record_type {
            messages::RASTER_FRAME => {
                let KindConfiguration::Raster(raster) = &configuration.kind else {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "RASTER_FRAME used a non-raster track",
                    ));
                };
                let frame = if let Ok(parsed) = media::parse_full_raster_frame(&record.body) {
                    if parsed.width != raster.width
                        || parsed.height != raster.height
                        || (parsed.compressed && !raster.zstd_enabled)
                    {
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            "raster frame differs from immutable configuration",
                        ));
                    }
                    Frame {
                        frame_id: parsed.frame_id,
                        pts_us: parsed.pts_us,
                        width: parsed.width,
                        height: parsed.height,
                        sar_num: 1,
                        sar_den: 1,
                        alpha_mode: raster.alpha_mode,
                        rgba: Arc::from(media::decode_raster_pixels(parsed)?),
                        damage: None,
                    }
                } else {
                    if !raster.delta_enabled {
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            "raster delta was not negotiated",
                        ));
                    }
                    let delta = media::parse_delta_raster_frame(
                        &record.body,
                        raster.width,
                        raster.height,
                        u32::from(raster.maximum_delta_operations),
                    )?;
                    let base = shared.scene.latest_frame(identity).ok_or_else(|| {
                        io::Error::new(
                            ErrorKind::InvalidData,
                            "raster delta has no retained full frame",
                        )
                    })?;
                    apply_raster_delta(&base, delta)?
                };
                shared
                    .scene
                    .publish_frame(
                        identity,
                        generation,
                        u32::try_from(record.body.len()).map_err(|_| {
                            io::Error::new(ErrorKind::InvalidData, "raster record exceeds u32")
                        })?,
                        frame.damage.as_ref().map_or_else(
                            || {
                                media::parse_full_raster_frame(&record.body)
                                    .map(|value| value.epoch)
                            },
                            |_| {
                                media::parse_delta_raster_frame(
                                    &record.body,
                                    raster.width,
                                    raster.height,
                                    u32::from(raster.maximum_delta_operations),
                                )
                                .map(|value| value.epoch)
                            },
                        )?,
                        frame.frame_id,
                        frame.damage.is_none(),
                        record.sequence,
                        frame,
                    )
                    .map_err(io::Error::other)?;
                (shared.wake)();
            },
            messages::IMAGE_DATA => {
                let status = shared
                    .scene
                    .track_status(identity)
                    .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "track disappeared"))?;
                let KindConfiguration::EncodedImage(configuration) = status.configuration.kind
                else {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "IMAGE_DATA used a non-image track",
                    ));
                };
                if record.body.len() != configuration.encoded_length as usize
                    || configuration.sha256.is_some_and(|expected| {
                        let actual: [u8; 32] = Sha256::digest(&record.body).into();
                        actual != expected
                    })
                {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "encoded image length or hash differs from immutable configuration",
                    ));
                }
                let image = image::load_from_memory_with_format(
                    &record.body,
                    image_format(configuration.encoding)?,
                )
                .map_err(io::Error::other)?
                .to_rgba8();
                let (width, height) = image.dimensions();
                if width != configuration.width || height != configuration.height {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "decoded image dimensions differ from immutable configuration",
                    ));
                }
                shared
                    .scene
                    .publish_frame(
                        identity,
                        generation,
                        u32::try_from(record.body.len()).map_err(|_| {
                            io::Error::new(ErrorKind::InvalidData, "image record exceeds u32")
                        })?,
                        0,
                        1,
                        true,
                        record.sequence,
                        Frame {
                            frame_id: 1,
                            pts_us: 0,
                            width,
                            height,
                            sar_num: 1,
                            sar_den: 1,
                            alpha_mode: scene::ALPHA_STRAIGHT,
                            rgba: Arc::from(image.into_raw()),
                            damage: None,
                        },
                    )
                    .map_err(io::Error::other)?;
                (shared.wake)();
            },
            messages::VIDEO_PACKET => {
                let packet = media::parse_video_packet(&record.body)?;
                let random_access = packet.flags & media::VIDEO_PACKET_KEY != 0;
                // A decoder may release multiple reordered frames for one encoded record. Treat
                // every output from the first output-bearing record as part of the same priming
                // unit: the producer cannot observe OUTPUT_READY or issue PLAY until its record
                // write completes. Later records remain paced against the playback clock.
                let priming_record = shared.scene.latest_frame(identity).is_none();
                shared
                    .scene
                    .admit_media(
                        identity,
                        generation,
                        u32::try_from(record.body.len()).map_err(|_| {
                            io::Error::new(ErrorKind::InvalidData, "video record exceeds u32")
                        })?,
                        packet.epoch,
                        packet.packet_id,
                        random_access,
                        record.sequence,
                    )
                    .map_err(io::Error::other)?;
                let frames = video_decoder
                    .as_mut()
                    .ok_or_else(|| {
                        io::Error::new(
                            ErrorKind::InvalidData,
                            "VIDEO_PACKET used a non-video track",
                        )
                    })?
                    .push(packet)?;
                for decoded in frames {
                    let (sar_num, sar_den) = match &configuration.kind {
                        KindConfiguration::Video(configuration) => (
                            u32::try_from(configuration.aspect_numerator).unwrap_or(u32::MAX),
                            u32::try_from(configuration.aspect_denominator).unwrap_or(u32::MAX),
                        ),
                        _ => unreachable!(),
                    };
                    wait_until_video_due(shared, identity, decoded.pts_us, priming_record)?;
                    shared
                        .scene
                        .publish_decoded_frame(
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
                                rgba: Arc::from(decoded.rgba),
                                damage: None,
                            },
                        )
                        .map_err(io::Error::other)?;
                    (shared.wake)();
                }
            },
            messages::AUDIO_PACKET => {
                let packet = media::parse_audio_packet(&record.body)?;
                shared
                    .scene
                    .admit_media(
                        identity,
                        generation,
                        u32::try_from(record.body.len()).map_err(|_| {
                            io::Error::new(ErrorKind::InvalidData, "audio record exceeds u32")
                        })?,
                        packet.epoch,
                        packet.packet_id,
                        true,
                        record.sequence,
                    )
                    .map_err(io::Error::other)?;
                let (output, decoder) = audio.as_mut().ok_or_else(|| {
                    io::Error::new(ErrorKind::InvalidData, "AUDIO_PACKET used a non-audio track")
                })?;
                let mut samples = decoder.push(packet)?;
                output.observe_audio_pts(packet.pts_us);
                output.trim_before_start(packet.pts_us, packet.duration_us, &mut samples);
                output.push(&samples)?;
                shared.scene.mark_output_ready(identity, generation).map_err(io::Error::other)?;
            },
            messages::CHANNEL_EOS => {
                let envelope = messages::decode_control(&record.body)?;
                if envelope.request_id != 0 {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "CHANNEL_EOS must be uncorrelated",
                    ));
                }
                let value = Value::Map(envelope.payload);
                let eos = StrictMap::new("CHANNEL_EOS", &value, &[0, 1, 2, 3, 4, 5])
                    .map_err(io::Error::other)?;
                let eos_epoch = eos
                    .required_u64(4)
                    .ok()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| {
                        io::Error::new(ErrorKind::InvalidData, "invalid CHANNEL_EOS media epoch")
                    })?;
                if eos.required_u64(0).ok() != Some(identity.surface.context.context_id)
                    || eos.required_u64(1).ok() != Some(identity.surface.surface_id)
                    || eos.required_u64(2).ok() != Some(identity.track_id)
                    || eos.required_u64(3).ok() != Some(generation.get())
                {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "CHANNEL_EOS does not name this track generation",
                    ));
                }
                shared
                    .scene
                    .mark_eos(
                        identity,
                        generation,
                        eos_epoch,
                        eos.required_u64(5).map_err(io::Error::other)?,
                    )
                    .map_err(io::Error::other)?;
                if let Some(decoder) = video_decoder.as_mut() {
                    // CHANNEL_EOS is also one channel record. If draining it produces the first
                    // output, complete that bounded priming unit before waiting for PLAY.
                    let priming_record = shared.scene.latest_frame(identity).is_none();
                    for decoded in decoder.finish()? {
                        let (sar_num, sar_den) = match &configuration.kind {
                            KindConfiguration::Video(configuration) => (
                                u32::try_from(configuration.aspect_numerator).unwrap_or(u32::MAX),
                                u32::try_from(configuration.aspect_denominator).unwrap_or(u32::MAX),
                            ),
                            _ => unreachable!(),
                        };
                        wait_until_video_due(shared, identity, decoded.pts_us, priming_record)?;
                        shared
                            .scene
                            .publish_decoded_frame(
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
                                    rgba: Arc::from(decoded.rgba),
                                    damage: None,
                                },
                            )
                            .map_err(io::Error::other)?;
                        (shared.wake)();
                    }
                    shared
                        .scene
                        .mark_buffered_ended(identity, generation)
                        .map_err(io::Error::other)?;
                }
                if let Some((output, decoder)) = audio.as_mut() {
                    output.push(&decoder.finish()?)?;
                    output.finish_decode();
                    output.signal_eos();
                }
                return Ok(());
            },
            _ if record.flags & RECORD_OPTIONAL != 0 => {},
            _ => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "record is not legal on a track channel",
                ));
            },
        }
        if matches!(
            record.record_type,
            messages::VIDEO_PACKET
                | messages::AUDIO_PACKET
                | messages::RASTER_FRAME
                | messages::IMAGE_DATA
        ) {
            let (maximum_bytes, maximum_records) = shared
                .scene
                .return_channel_capacity(identity, generation, record.body.len() as u64, 1)
                .map_err(io::Error::other)?;
            writer.write_record(
                messages::MAX_CHANNEL_DATA,
                identity.track_id,
                &Envelope::new(
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
                .encode()?,
            )?;
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
        let mut next_bytes = byte_bucket.clone();
        let mut next_records = record_bucket.clone();
        if next_bytes.charge(body_bytes).is_ok() && next_records.charge(1).is_ok() {
            *byte_bucket = next_bytes;
            *record_bucket = next_records;
            return Ok(());
        }

        // Transport scheduling can turn a correctly paced producer stream into an arrival burst
        // after SSH, WebTransport, or WebSocket buffering. Shape admission here instead of
        // destroying the track for that transport artifact. Absolute channel flow remains the
        // finite bound and no capacity is returned until this record is reusable.
        thread::sleep(Duration::from_millis(1));
    }
}

fn wait_until_video_due(
    shared: &Arc<ServiceShared>,
    identity: TrackIdentity,
    pts_us: i64,
    priming_record: bool,
) -> io::Result<()> {
    if priming_record {
        return shared.scene.wait_until_due(identity, pts_us, true).map_err(io::Error::other);
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
        thread::sleep(Duration::from_millis(2));
    }
}

fn reject_lane(reader: &mut Reader) -> io::Result<()> {
    let writer = reader.writer(ConnectionKind::Lane)?;
    let first = reader.read_record(ConnectionKind::Lane)?;
    let request_id =
        messages::decode_control(&first.body).map(|envelope| envelope.request_id).unwrap_or(0);
    writer.write_record(
        messages::ERROR,
        first.object_id,
        &protocol_error(
            request_id,
            messages::ERROR_UNSUPPORTED_PROFILE,
            true,
            "Vivido does not implement input or multiplexed carrier lanes",
        )?,
    )
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

fn track_status_payload(status: &TrackStatus) -> Vec<(u64, Value)> {
    vec![
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
    ]
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
                Decoder::new(video).is_ok()
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
}

impl ControlError {
    const fn bad_message(message: &'static str) -> Self {
        Self { code: messages::ERROR_BAD_MESSAGE, message }
    }

    const fn bad_state(message: &'static str) -> Self {
        Self { code: messages::ERROR_BAD_STATE, message }
    }

    const fn state(message: &'static str) -> Self {
        Self::bad_state(message)
    }

    const fn not_found(message: &'static str) -> Self {
        Self { code: messages::ERROR_NOT_FOUND, message }
    }

    const fn precondition(message: &'static str) -> Self {
        Self { code: messages::ERROR_PRECONDITION_FAILED, message }
    }

    const fn stale_target() -> Self {
        Self { code: registry::error::STALE_TARGET_GENERATION, message: "stale target generation" }
    }

    const fn unsupported(message: &'static str) -> Self {
        Self { code: messages::ERROR_UNSUPPORTED_CONFIG, message }
    }

    const fn limit(message: &'static str) -> Self {
        Self { code: messages::ERROR_LIMIT_EXCEEDED, message }
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
                let mut copied = Vec::with_capacity(width as usize * height as usize * 4);
                for row in 0..height {
                    let source =
                        ((source_y + row) as usize * base.width as usize + source_x as usize) * 4;
                    let length = width as usize * 4;
                    copied.extend_from_slice(&rgba[source..source + length]);
                }
                for row in 0..height {
                    let destination = ((destination_y + row) as usize * base.width as usize
                        + destination_x as usize)
                        * 4;
                    let source = row as usize * width as usize * 4;
                    let length = width as usize * 4;
                    rgba[destination..destination + length]
                        .copy_from_slice(&copied[source..source + length]);
                }
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
        rgba: Arc::from(rgba),
        damage: Some(Arc::from(damage)),
    })
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
    use vivid_sdk::{
        CoordinateModel, MILESTONE_OUTPUT_READY, MILESTONE_PRESENTED, ProducerAuthentication,
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
        let service = VividService::start_with_wake(test_geometry(), Arc::new(|| {})).unwrap();
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

    /// A scene commit names the target generation it was planned against, so an announced resize
    /// has to carry every live scene onto the new target. Leaving a scene behind rejects the
    /// commits a producer makes in response to the announcement it was just sent, which is fatal
    /// for a producer that re-places its node on every resize.
    #[test]
    fn an_announced_display_change_carries_every_live_scene_onto_the_new_target() {
        let service = VividService::start_with_wake(test_geometry(), Arc::new(|| {})).unwrap();
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
        let service = VividService::start_with_wake(test_geometry(), Arc::new(|| {})).unwrap();
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
        let service = VividService::start_with_wake(test_geometry(), Arc::new(|| {})).unwrap();
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
        let item = service.scene.snapshot().1.pop().unwrap();
        assert_eq!((item.x, item.y), (3_i64 << 32, 4_i64 << 32));
        drop(channel);
        session.close().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !service.scene.session_ids().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(service.scene.session_ids().is_empty());
        assert_eq!(
            service.scene.snapshot().1.len(),
            1,
            "clean GOODBYE must preserve an anchored, policy-permitted terminal poster"
        );
    }

    fn desktop_service() -> VividService {
        let target = Arc::new(DesktopTarget::new(test_geometry()).unwrap());
        VividService::start_with_target(target, Arc::new(|| {})).unwrap()
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
        let service = desktop_service();
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
        let service = desktop_service();
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
        let desktop = desktop_service();
        let mut session = connect_desktop(&desktop);
        let context_id = session.info().root_context_id;
        let mut terminal_shaped = desktop_surface(context_id, 1920);
        terminal_shaped.semantic_profile = registry::TERMINAL_CONTENT.into();
        terminal_shaped.coordinate_model = CoordinateModel::TerminalContentCells;
        assert!(
            session.create_surface(terminal_shaped, &RequestMetadata::default()).is_err(),
            "a desktop target cannot present terminal content"
        );

        let terminal = VividService::start_with_wake(test_geometry(), Arc::new(|| {})).unwrap();
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
        let service = desktop_service();
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
        let service = desktop_service();
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
        let service = desktop_service();
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
        let service = desktop_service();
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

    #[test]
    fn an_outstanding_wait_does_not_block_the_control_reader() {
        // Core §5.1: a long operation becomes bounded pending state and must not stall the
        // parser. Registering a wait that cannot be satisfied used to be harmless only because
        // it spawned a thread; the actor now holds it, so prove the session still answers.
        let service = VividService::start_with_wake(test_geometry(), Arc::new(|| {})).unwrap();
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
