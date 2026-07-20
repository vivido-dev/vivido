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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use image::GenericImageView;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use vivid_protocol::anchor::{self, AnchorKey};
use vivid_protocol::media::{self, VIDEO_PACKET_KEY};
use vivid_protocol::messages::{self, Credits, DisplayChanged};
use vivid_protocol::wire::{ConnectionKind, RECORD_OPTIONAL, Record};

use crate::event::{EventProxy, EventType};
use crate::vivid::audio::AudioOutput;
use crate::vivid::decoder::{DecodedFrame, Decoder};
use crate::vivid::scene::{
    Frame, SceneMutation, SceneNode, SessionId, SharedScene, SourceConfig, SourceKey,
};
use crate::vivid::transport::{Reader, Writer};

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
}

#[derive(Default)]
struct Registry {
    next_session_id: u64,
    sessions: HashMap<SessionId, SessionRuntime>,
    tickets: HashMap<Vec<u8>, Ticket>,
}

struct ServiceShared {
    token: [u8; 32],
    scene: SharedScene,
    registry: Mutex<Registry>,
    metrics: Mutex<DisplayMetrics>,
    active_connections: AtomicUsize,
    audio_outputs: Mutex<HashMap<SourceKey, Arc<AudioOutput>>>,
    /// Last `(renderable, display_offset)` reported by the UI thread. Cached so scene changes
    /// applied on the control-dispatcher thread (e.g. a newly committed node) can recompute
    /// source visibility without the UI-thread inputs directly at hand.
    render_state: Mutex<(bool, usize)>,
    wake: Arc<dyn Fn() + Send + Sync>,
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
        let shared = Arc::new(ServiceShared {
            token,
            scene: scene.clone(),
            registry: Mutex::new(Registry::default()),
            metrics: Mutex::new(metrics),
            active_connections: AtomicUsize::new(0),
            audio_outputs: Mutex::new(HashMap::new()),
            render_state: Mutex::new((true, 0)),
            wake,
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

    pub fn update_metrics(&self, mut metrics: DisplayMetrics) {
        {
            let mut current = lock_metrics(&self.shared);
            if current.viewport_width == metrics.viewport_width
                && current.viewport_height == metrics.viewport_height
                && current.columns == metrics.columns
                && current.rows == metrics.rows
                && current.cell_width == metrics.cell_width
                && current.cell_height == metrics.cell_height
            {
                return;
            }
            metrics.generation = current.generation.saturating_add(1);
            *current = metrics;
        }

        let writers = {
            let registry = lock_registry(&self.shared);
            registry
                .sessions
                .values()
                .filter_map(|session| session.writer.upgrade())
                .collect::<Vec<_>>()
        };
        let body = messages::display_changed(
            0,
            DisplayChanged {
                display_generation: metrics.generation,
                viewport_width: metrics.viewport_width,
                viewport_height: metrics.viewport_height,
                grid_columns: metrics.columns,
                grid_rows: metrics.rows,
                cell_width: metrics.cell_width,
                cell_height: metrics.cell_height,
            },
        );
        for writer in writers {
            if let Err(error) = writer.write_record(messages::DISPLAY_CHANGED, 0, &body) {
                log::debug!("Could not notify Vivid session of display change: {error}");
            }
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
        let removed = self.scene.scroll_anchors(origin, end, lines, history_size);
        self.notify_anchor_events(messages::ANCHOR_GONE, removed);
        wake(&self.shared);
    }

    pub fn handle_terminal_clear(&self) {
        let removed = self.scene.clear_terminal();
        self.notify_anchor_events(messages::ANCHOR_GONE, removed);
        wake(&self.shared);
    }

    /// The terminal switched between the primary and alternate screens. Anchored media on the
    /// inactive screen is hidden; anchors created on the alternate screen are gone once it exits.
    pub fn handle_screen_swap(&self, alternate: bool) {
        let removed = self.scene.set_alternate_screen(alternate);
        self.notify_anchor_events(messages::ANCHOR_GONE, removed);
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
    if !constant_time_token_eq(&shared.token, hello.token.as_bytes()) {
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
    let unsupported_feature =
        hello.required_features.iter().any(|feature| !is_supported_feature(*feature));
    let supports_1_1 = offers_protocol_1_1(
        hello.minimum_major,
        hello.minimum_minor,
        hello.maximum_major,
        hello.maximum_minor,
    );
    if !supports_1_1 || hello.maximum_record_body == 0 || unsupported_feature {
        let (code, diagnostic) = if !supports_1_1 {
            (messages::ERROR_UNSUPPORTED_VERSION, "Vivid protocol 1.1 is required")
        } else if hello.maximum_record_body == 0 {
            (messages::ERROR_BAD_MESSAGE, "maximum record body is zero")
        } else {
            (messages::ERROR_UNSUPPORTED_FEATURE, "required Vivid feature is unsupported")
        };
        writer.write_record(messages::ERROR, 0, &messages::error(request_id, code, diagnostic))?;
        return Ok(());
    }

    let mut accepted_features = hello.required_features.clone();
    accepted_features.extend(hello.optional_features.iter().copied());
    accepted_features.retain(|feature| is_supported_feature(*feature));
    accepted_features.sort_unstable();
    accepted_features.dedup();

    let (session_id, session_tag, root_context_id) = {
        let mut registry = lock_registry(&shared);
        if registry.sessions.len() >= MAX_SESSIONS {
            writer.write_record(
                messages::ERROR,
                0,
                &messages::error(
                    request_id,
                    messages::ERROR_LIMIT_EXCEEDED,
                    "session quota exceeded",
                ),
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
        let anchor_key = anchor::derive_key(&shared.token, &tag);
        registry.sessions.insert(
            session_id,
            SessionRuntime {
                writer: Arc::downgrade(&writer),
                tag,
                anchor_key,
                seen_anchors: HashSet::new(),
                last_visibility: HashMap::new(),
                accepted_features: accepted_features.iter().copied().collect(),
            },
        );
        (session_id, tag, (session_id << 32) | 1)
    };

    let metrics = *lock_metrics(&shared);
    writer.write_record(
        messages::WELCOME,
        0,
        &messages::welcome(
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
            },
            &accepted_features,
        ),
    )?;
    log::info!("Authenticated Vivid producer {:?} as session {session_id}", hello.producer);

    let mut transactions: HashMap<u64, Vec<SceneMutation>> = HashMap::new();
    let result = loop {
        let record = match reader.read_record() {
            Ok(record) => record,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break Ok(()),
            Err(error) => break Err(error),
        };
        let result = dispatch_control(
            &record,
            session_id,
            root_context_id,
            &shared,
            &writer,
            &mut transactions,
        );
        match result {
            Ok(ControlAction::Continue) => {},
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
    )
}

fn offers_protocol_1_1(
    minimum_major: u64,
    minimum_minor: u64,
    maximum_major: u64,
    maximum_minor: u64,
) -> bool {
    (minimum_major, minimum_minor) <= (1, 1) && (maximum_major, maximum_minor) >= (1, 1)
}

fn negotiated(shared: &Arc<ServiceShared>, session_id: SessionId, feature: u64) -> bool {
    lock_registry(shared)
        .sessions
        .get(&session_id)
        .is_some_and(|session| session.accepted_features.contains(&feature))
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

enum ControlAction {
    Continue,
    Goodbye,
}

struct ProtocolError {
    code: u64,
    message: &'static str,
    fatal: bool,
}

fn dispatch_control(
    record: &Record,
    session_id: SessionId,
    root_context_id: u64,
    shared: &Arc<ServiceShared>,
    writer: &Arc<Writer>,
    transactions: &mut HashMap<u64, Vec<SceneMutation>>,
) -> Result<ControlAction, ProtocolError> {
    let bad = |message| ProtocolError { code: messages::ERROR_BAD_MESSAGE, message, fatal: false };
    match record.record_type {
        messages::PING => {
            let envelope =
                messages::decode_control(&record.body).map_err(|_| bad("invalid PING"))?;
            if record.object_id != 0 || envelope.request_id == 0 {
                return Err(bad("PING is not a correlated session-level request"));
            }
            writer
                .write_record(messages::PONG, 0, &messages::ok(envelope.request_id))
                .map_err(|_| bad("could not send PONG"))?;
        },
        messages::PROBE_VIDEO_CONFIG => {
            let (envelope, config) = messages::parse_create_video(&record.body)
                .map_err(|_| bad("invalid PROBE_VIDEO_CONFIG"))?;
            if record.object_id != 0 || config.source_id != 0 {
                return Err(bad("PROBE_VIDEO_CONFIG must be session-level"));
            }
            let supported = media::is_portable_packetization(&config.codec, &config.packetization)
                && Decoder::new(&config).is_ok();
            writer
                .write_record(
                    messages::VIDEO_SUPPORT,
                    0,
                    &messages::video_support(envelope.request_id, supported, &config.codec),
                )
                .map_err(|_| bad("could not send VIDEO_SUPPORT"))?;
        },
        messages::PROBE_AUDIO_CONFIG => {
            if !negotiated(shared, session_id, messages::FEATURE_AUDIO_ACCESS_UNIT_V1) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_FEATURE,
                    message: "audio access units were not negotiated",
                    fatal: false,
                });
            }
            let (envelope, config) = messages::parse_create_audio(&record.body)
                .map_err(|_| bad("invalid PROBE_AUDIO_CONFIG"))?;
            if record.object_id != 0 || config.source_id != 0 {
                return Err(bad("PROBE_AUDIO_CONFIG must be session-level"));
            }
            let supported = messages::audio_config_supported(&config) && audio::supports(&config);
            writer
                .write_record(
                    messages::AUDIO_SUPPORT,
                    0,
                    &messages::audio_support(envelope.request_id, supported, &config.codec),
                )
                .map_err(|_| bad("could not send AUDIO_SUPPORT"))?;
        },
        messages::CREATE_RASTER => {
            let (envelope, config) = messages::parse_create_raster(&record.body)
                .map_err(|_| bad("invalid CREATE_RASTER"))?;
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
            shared
                .scene
                .add_source(session_id, config.source_id, SourceConfig::Raster(config.clone()))
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
            let (envelope, config) = messages::parse_create_video(&record.body)
                .map_err(|_| bad("invalid CREATE_VIDEO"))?;
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
            shared
                .scene
                .add_source(session_id, config.source_id, SourceConfig::Video(config.clone()))
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
            let (envelope, config) = messages::parse_create_audio(&record.body)
                .map_err(|_| bad("invalid CREATE_AUDIO"))?;
            if config.source_id == 0 || record.object_id != config.source_id {
                return Err(bad("CREATE_AUDIO object ID mismatch"));
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
            if !audio::supports(&config) {
                return Err(ProtocolError {
                    code: messages::ERROR_UNSUPPORTED_CONFIG,
                    message: "audio decoder configuration is unavailable",
                    fatal: false,
                });
            }
            let max_body =
                media::audio_body_len(config.max_access_unit_bytes).map_err(|_| ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message: "maximum audio access unit exceeds the media-body limit",
                    fatal: false,
                })?;
            let output = AudioOutput::open().map_err(|_| ProtocolError {
                code: messages::ERROR_DEVICE_LOST,
                message: "default audio output is unavailable",
                fatal: false,
            })?;
            shared
                .scene
                .add_source(session_id, config.source_id, SourceConfig::Audio(config.clone()))
                .map_err(|message| ProtocolError {
                    code: messages::ERROR_LIMIT_EXCEEDED,
                    message,
                    fatal: false,
                })?;
            shared
                .audio_outputs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert((session_id, config.source_id), output);
            issue_source_ready(
                shared,
                writer,
                envelope.request_id,
                (session_id, config.source_id),
                ConnectionKind::Audio,
                max_body,
            )
            .map_err(|_| bad("could not create audio media ticket"))?;
        },
        messages::CREATE_IMAGE => {
            let (envelope, config) = messages::parse_create_image(&record.body)
                .map_err(|_| bad("invalid CREATE_IMAGE"))?;
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
            shared
                .scene
                .add_source(session_id, config.source_id, SourceConfig::Image(config.clone()))
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
                .write_record(messages::OK, source_id, &messages::ok(envelope.request_id))
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
            if transactions.insert(transaction_id, Vec::new()).is_some() {
                return Err(ProtocolError {
                    code: messages::ERROR_DUPLICATE_ID,
                    message: "transaction ID already exists",
                    fatal: false,
                });
            }
            writer
                .write_record(messages::OK, 0, &messages::ok(envelope.request_id))
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
                .write_record(messages::OK, record.object_id, &messages::ok(envelope.request_id))
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
                .write_record(messages::OK, record.object_id, &messages::ok(envelope.request_id))
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
                .write_record(messages::OK, record.object_id, &messages::ok(envelope.request_id))
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
            shared.scene.commit_mutations(session_id, nodes).map_err(|message| ProtocolError {
                code: messages::ERROR_BAD_STATE,
                message,
                fatal: false,
            })?;
            writer
                .write_record(messages::PRESENTED, 0, &messages::ok(envelope.request_id))
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
                .write_record(messages::OK, 0, &messages::ok(envelope.request_id))
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
                .write_record(messages::OK, source_id, &messages::ok(envelope.request_id))
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
                .write_record(messages::OK, source_id, &messages::ok(envelope.request_id))
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
                messages::parse_eos(&record.body).map_err(|_| bad("invalid FLUSH"))?;
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
                .write_record(messages::OK, source_id, &messages::ok(envelope.request_id))
                .map_err(|_| bad("could not acknowledge FLUSH"))?;
        },
        messages::EOS => {
            let (envelope, source_id, epoch) =
                messages::parse_eos(&record.body).map_err(|_| bad("invalid EOS"))?;
            if record.object_id != source_id {
                return Err(bad("EOS object ID mismatch"));
            }
            let key = (session_id, source_id);
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
            for output in audio_group(shared, key) {
                output.signal_eos();
            }
            writer
                .write_record(messages::OK, source_id, &messages::ok(envelope.request_id))
                .map_err(|_| bad("could not acknowledge EOS"))?;
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
            output.wait_drained().map_err(|_| ProtocolError {
                code: messages::ERROR_DEVICE_LOST,
                message: "audio output failed while draining",
                fatal: false,
            })?;
            writer
                .write_record(messages::OK, source_id, &messages::ok(envelope.request_id))
                .map_err(|_| bad("could not acknowledge DRAIN"))?;
        },
        messages::GOODBYE => {
            let envelope =
                messages::decode_control(&record.body).map_err(|_| bad("invalid GOODBYE"))?;
            writer
                .write_record(messages::OK, 0, &messages::ok(envelope.request_id))
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
    let mut ticket_bytes = vec![0_u8; 32];
    getrandom::fill(&mut ticket_bytes)
        .map_err(|error| io::Error::other(format!("could not generate media ticket: {error}")))?;
    lock_registry(shared)
        .tickets
        .insert(ticket_bytes.clone(), Ticket { session_id: source_key.0, source_key, kind });
    writer.write_record(
        messages::SOURCE_READY,
        source_key.1,
        &messages::source_ready(
            request_id,
            source_key.1,
            &ticket_bytes,
            Credits {
                bytes: INITIAL_BYTE_CREDITS.max(u64::from(max_media_body)),
                packets: INITIAL_PACKET_CREDITS,
                fragments: 0,
            },
            max_media_body,
        ),
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
        let writer = registry
            .sessions
            .get(&ticket.session_id)
            .and_then(|session| session.writer.upgrade())
            .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "control session is gone"))?;
        (ticket, writer)
    };
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
        let _ = shared.scene.remove_source(source_key);
        if let Some(output) = shared
            .audio_outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&source_key)
        {
            output.stop();
        }
        let diagnostic = error.to_string();
        let code = if diagnostic.contains("hash mismatch") {
            messages::ERROR_HASH_MISMATCH
        } else {
            messages::ERROR_DECODER
        };
        let _ = writer.write_record(
            messages::SOURCE_LOST,
            source_key.1,
            &messages::source_lost(source_key.1, code, &diagnostic),
        );
        wake(&shared);
        return Ok(());
    }
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
    loop {
        let record = match reader.read_record() {
            Ok(record) => record,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        let _charge = ChargedBody::new(writer, key.1, record.body.len() as u64);
        if record.record_type != messages::RASTER_FRAME || record.object_id != key.1 {
            return Err(invalid("unexpected record on raster media channel"));
        }
        let raster = media::parse_full_raster_frame(&record.body)?;
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
    let mut current_epoch = None;
    let mut sequence = media::MediaSequence::default();
    let mut frame_id = 0_u64;
    let mut pending = VecDeque::with_capacity(MAX_QUEUED_VIDEO_FRAMES);
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
        let record = match reader.read_record() {
            Ok(record) => record,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        let _charge = ChargedBody::new(writer, key.1, record.body.len() as u64);
        if record.record_type != messages::VIDEO_PACKET || record.object_id != key.1 {
            return Err(invalid("unexpected record on video media channel"));
        }
        let packet = media::parse_video_packet(&record.body)?;
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
                    &messages::need_keyframe(key.1, packet.epoch, 1, None),
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
                    &messages::need_keyframe(key.1, packet.epoch, 3, Some(packet.packet_id)),
                )?;
                continue;
            },
            Some(epoch) if packet.epoch > epoch => decoder = Decoder::new(&config)?,
            _ => {},
        }
        current_epoch = Some(packet.epoch);
        let epoch = packet.epoch;
        let decoded_frames = match decoder.push(packet) {
            Ok(frames) => frames,
            Err(_) => {
                let minimum_epoch = epoch.saturating_add(1);
                writer.write_record(
                    messages::NEED_KEYFRAME,
                    key.1,
                    &messages::need_keyframe(key.1, minimum_epoch, 2, Some(packet.packet_id)),
                )?;
                decoder = Decoder::new(&config)?;
                current_epoch = None;
                continue;
            },
        };
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
        let mut sequence = media::MediaSequence::default();
        let mut decoder_epoch = None;
        loop {
            if !reader.wait_readable(Duration::from_millis(50))? {
                if shared.scene.eos_epoch(key).is_some() {
                    break;
                }
                continue;
            }
            let record = match reader.read_record() {
                Ok(record) => record,
                Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            };
            let _charge = ChargedBody::new(writer, key.1, record.body.len() as u64);
            if record.record_type != messages::AUDIO_PACKET || record.object_id != key.1 {
                return Err(invalid("unexpected record on audio media channel"));
            }
            let packet = media::parse_audio_packet(&record.body)?;
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
            let mut samples = decoder.push(packet)?;
            output.trim_before_start(packet.pts_us, packet.duration_us, &mut samples);
            if !samples.is_empty() {
                output.observe_audio_pts(packet.pts_us);
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
    let record = reader.read_record()?;
    let _charge = ChargedBody::new(writer, key.1, record.body.len() as u64);
    if record.record_type != messages::IMAGE_DATA
        || record.object_id != key.1
        || record.body.len() != config.encoded_length as usize
    {
        return Err(invalid("invalid IMAGE_DATA record"));
    }
    if let Some(expected) = config.sha256 {
        let actual: [u8; 32] = Sha256::digest(&record.body).into();
        if actual != expected {
            return Err(invalid("encoded image hash mismatch"));
        }
    }
    if encoded_image_has_multiple_pictures(config.encoding, &record.body)? {
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
    let mut image_reader = image::ImageReader::with_format(Cursor::new(&record.body), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(config.width);
    limits.max_image_height = Some(config.height);
    limits.max_alloc = Some(decoded_bytes.saturating_add(16 * 1024 * 1024));
    image_reader.limits(limits);
    let decoded = image_reader.decode().map_err(|_| invalid("encoded image decoder failed"))?;
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
    writer.write_record(messages::CREDIT, source_id, &messages::credit(bytes, 1, 0))
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

fn cleanup_session(shared: &Arc<ServiceShared>, session_id: SessionId) {
    let mut registry = lock_registry(shared);
    registry.sessions.remove(&session_id);
    registry.tickets.retain(|_, ticket| ticket.session_id != session_id);
    drop(registry);
    let outputs = {
        let mut outputs =
            shared.audio_outputs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let keys = outputs.keys().copied().filter(|key| key.0 == session_id).collect::<Vec<_>>();
        keys.into_iter().filter_map(|key| outputs.remove(&key)).collect::<Vec<_>>()
    };
    for output in outputs {
        output.stop();
    }
    shared.scene.detach_session(session_id);
}

fn lock_registry(shared: &Arc<ServiceShared>) -> std::sync::MutexGuard<'_, Registry> {
    shared.registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_metrics(shared: &Arc<ServiceShared>) -> std::sync::MutexGuard<'_, DisplayMetrics> {
    shared.metrics.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
    let states = shared.scene.aggregate_visibility(
        metrics.columns,
        metrics.rows,
        display_offset,
        renderable,
    );
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
            active_connections: AtomicUsize::new(0),
            audio_outputs: Mutex::new(HashMap::from([((1, 11), output.clone())])),
            render_state: Mutex::new((true, 0)),
            wake: Arc::new(|| {}),
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
    fn protocol_selection_rejects_1_0_only_ranges() {
        assert!(!offers_protocol_1_1(1, 0, 1, 0));
        assert!(offers_protocol_1_1(1, 0, 1, 1));
        assert!(offers_protocol_1_1(1, 1, 1, 1));
        assert!(!offers_protocol_1_1(1, 2, 2, 0));
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

        let mut legacy = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
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
        hello.text("legacy-test");
        hello.u64(6);
        hello.text("1.0");
        hello.u64(7);
        hello.array(0);
        hello.u64(8);
        hello.array(0);
        hello.u64(9);
        hello.u64(u64::from(vivid_protocol::CONTROL_MAX_RECORD_BODY));
        legacy.write_record(messages::HELLO, 0, 0, &hello.into_vec()).unwrap();
        let rejection = messages::parse_error_reply(&legacy.read_record().unwrap().body).unwrap();
        assert_eq!(rejection.code, messages::ERROR_UNSUPPORTED_VERSION);

        let mut control = Connection::open(&endpoint, ConnectionKind::Control).unwrap();
        control.write_record(messages::HELLO, 0, 0, &messages::hello(1, service.token())).unwrap();
        let welcome = parse_welcome(&control.read_record().unwrap().body).unwrap();
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
        let changed_record = control.read_record().unwrap();
        assert_eq!(changed_record.record_type, messages::DISPLAY_CHANGED);
        let changed = parse_display_changed(&changed_record.body).unwrap();
        assert_eq!(changed.display_generation, 2);
        assert_eq!((changed.grid_columns, changed.grid_rows), (100, 35));

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
        while messages::request_id(&control.read_record().unwrap().body).unwrap() != 5 {}

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
