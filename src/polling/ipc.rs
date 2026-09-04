//! Versioned, owner-only local automation protocol.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
#[cfg(unix)]
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Error as IoError, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use base64::Engine;
use log::{error, warn};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cli::{
    IpcAutomationPlan, IpcAutomationPlanStep, IpcCapture, IpcMouseAction, IpcPlanErrorPolicy,
    IpcRunPlan, IpcVividCommand, IpcWait, IpcWaitCondition, MessageOptions, Options, SocketMessage,
};
use crate::client_fault::{self, ClientFaultClass};
use crate::event::{Event, EventSink, EventType};
use crate::polling::transport::{LocalListener, LocalStream};
use crate::terminal::thread;

/// Formal Vivido automation protocol version.
pub const PROTOCOL_VERSION: u16 = 2;

/// Maximum request frame size.
pub const MAX_REQUEST_FRAME_BYTES: usize = 1024 * 1024;

/// Maximum reply or event frame size.
pub const MAX_REPLY_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Maximum terminal text returned through IPC.
pub const MAX_IPC_TEXT_BYTES: usize = MAX_REPLY_FRAME_BYTES;

/// Maximum accepted client connections.
pub const MAX_CONNECTIONS: usize = 32;

/// Maximum concurrent request IDs for one connection.
pub const MAX_IN_FLIGHT_REQUESTS: usize = 64;

/// Maximum subscriptions across the process.
pub const MAX_SUBSCRIPTIONS: usize = 32;

/// Maximum queued events for one subscriber.
pub const MAX_SUBSCRIBER_EVENTS: usize = 256;

/// Maximum literal input/paste request.
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Environment variable name for the IPC socket path.
const VIVIDO_SOCKET_ENV: &str = "VIVIDO_SOCKET";

/// Environment variable naming the headless session a client should reach.
const VIVIDO_SESSION_ENV: &str = "VIVIDO_SESSION";

/// How this instance describes itself in the `hello` capability document.
///
/// Whether a process is headless, and which session it serves, is fixed at startup, so it is
/// recorded once rather than threaded through every connection.
static INSTANCE: std::sync::OnceLock<Instance> = std::sync::OnceLock::new();

#[derive(Debug, Default)]
struct Instance {
    headless: bool,
    session: Option<String>,
    automation_name: Option<String>,
}

/// Number of serialized frames buffered for one connection.
const OUTPUT_QUEUE_FRAMES: usize = MAX_SUBSCRIBER_EVENTS + MAX_IN_FLIGHT_REQUESTS;

/// Write timeout prevents a dead client from retaining a writer forever.
const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Methods advertised by the protocol handshake.
pub const METHODS: &[&str] = &[
    "hello",
    "ping",
    "reset_terminal",
    "restart_terminal",
    "unsubscribe",
    "create_window",
    "config",
    "get_config",
    "typing",
    "get_text",
    "screenshot",
    "key",
    "paste",
    "mouse",
    "resize",
    "set_geometry",
    "set_geometry_batch",
    "set_visible",
    "set_level",
    "focus",
    "signal",
    "list_windows",
    "inspect",
    "diagnose",
    "vivid_sessions",
    "vivid_surfaces",
    "vivid_surface_status",
    "vivid_tracks",
    "vivid_track_status",
    "vivid_scene_status",
    "vivid_trace",
    "get_grid",
    "wait_text",
    "wait_output",
    "wait_screen_change",
    "wait_screen_stable",
    "wait_frame",
    "wait_exit",
    "quit",
    "wait_vivid_track",
    "transcript",
    "subscribe",
];

/// Additional methods an in-process host has claimed, advertised alongside [`METHODS`].
///
/// The handshake is answered on the listener thread, while claiming happens on the main loop, so
/// the claimed set lives here rather than on the processor that owns it.
static HOST_METHODS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Descriptors supplied by an embedding host for its claimed methods.
static HOST_METHOD_CAPABILITIES: RwLock<Vec<MethodCapability>> = RwLock::new(Vec::new());

/// Stable high-level effect class for one automation method.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MethodClass {
    Observe,
    Input,
    Window,
    Config,
    Process,
    Lifecycle,
    Extension,
}

/// Additive method metadata advertised by the version-2 handshake.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MethodCapability {
    pub name: String,
    pub class: MethodClass,
    pub mutating: bool,
    pub host_claimed: bool,
}

impl MethodCapability {
    pub fn host(name: impl Into<String>, class: MethodClass, mutating: bool) -> Self {
        Self { name: name.into(), class, mutating, host_claimed: true }
    }
}

/// Publish the host's claimed method names for the handshake to advertise.
pub(crate) fn publish_host_methods<'a>(methods: impl IntoIterator<Item = &'a String>) {
    let claimed = methods.into_iter().cloned().collect::<Vec<_>>();
    match HOST_METHODS.write() {
        Ok(mut host_methods) => *host_methods = claimed,
        Err(poisoned) => *poisoned.into_inner() = claimed,
    }
}

/// Publish effect metadata for methods claimed by an embedding host.
pub(crate) fn publish_host_method_capabilities(capabilities: &[MethodCapability]) {
    match HOST_METHOD_CAPABILITIES.write() {
        Ok(mut published) => *published = capabilities.to_vec(),
        Err(poisoned) => *poisoned.into_inner() = capabilities.to_vec(),
    }
}

/// Every method the handshake advertises: Vivido's own plus the host's claimed names.
fn advertised_methods() -> Vec<String> {
    let mut methods = METHODS.iter().map(|method| (*method).to_owned()).collect::<Vec<_>>();
    let host_methods = match HOST_METHODS.read() {
        Ok(host_methods) => host_methods,
        Err(poisoned) => poisoned.into_inner(),
    };
    for method in host_methods.iter() {
        if !methods.iter().any(|advertised| advertised == method) {
            methods.push(method.clone());
        }
    }
    methods
}

fn method_class(name: &str) -> (MethodClass, bool) {
    match name {
        "hello"
        | "ping"
        | "get_config"
        | "get_text"
        | "screenshot"
        | "list_windows"
        | "inspect"
        | "diagnose"
        | "vivid_sessions"
        | "vivid_surfaces"
        | "vivid_surface_status"
        | "vivid_tracks"
        | "vivid_track_status"
        | "vivid_scene_status"
        | "vivid_trace"
        | "get_grid"
        | "wait_text"
        | "wait_output"
        | "wait_screen_change"
        | "wait_screen_stable"
        | "wait_frame"
        | "wait_exit"
        | "wait_vivid_track"
        | "transcript"
        | "subscribe"
        | "unsubscribe" => (MethodClass::Observe, false),
        "typing" | "key" | "paste" | "mouse" => (MethodClass::Input, true),
        "create_window" | "resize" | "set_geometry" | "set_geometry_batch" | "set_visible"
        | "set_level" => (MethodClass::Window, true),
        "config" => (MethodClass::Config, true),
        "focus" | "signal" => (MethodClass::Process, true),
        "quit" | "reset_terminal" | "restart_terminal" => (MethodClass::Lifecycle, true),
        _ => (MethodClass::Extension, true),
    }
}

fn advertised_method_capabilities() -> Vec<MethodCapability> {
    let methods = advertised_methods();
    let host = match HOST_METHOD_CAPABILITIES.read() {
        Ok(host) => host,
        Err(poisoned) => poisoned.into_inner(),
    };
    let host_methods = match HOST_METHODS.read() {
        Ok(methods) => methods,
        Err(poisoned) => poisoned.into_inner(),
    };
    methods
        .into_iter()
        .map(|name| {
            if let Some(capability) = host.iter().find(|capability| capability.name == name) {
                return capability.clone();
            }
            let (class, mutating) = method_class(&name);
            MethodCapability {
                host_claimed: host_methods.iter().any(|method| method == &name),
                name,
                class,
                mutating,
            }
        })
        .collect()
}

/// Event kinds advertised by the protocol handshake.
///
/// This is also the `subscribe` allowlist, so a kind missing here is one no client can ask for even
/// though the event loop still delivers it to unfiltered subscriptions. `AutomationHub::emit_payload`
/// debug-asserts against this list to keep the two from drifting apart again.
pub const EVENT_KINDS: &[&str] = &[
    "screen_changed",
    "output",
    "frame_presented",
    "title_changed",
    "directory_changed",
    "focus_changed",
    "resized",
    "moved",
    "bell",
    "child_exit",
    "window_created",
    "window_closed",
    "client_fault",
    "client_recovered",
    "overflow",
];

/// Request envelope for one newline-delimited JSON frame.
#[derive(Debug, Deserialize, Serialize)]
pub struct RequestEnvelope {
    pub version: u16,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Structured stable IPC error.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IpcError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl IpcError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), data: None }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// Reply envelope for one correlated request.
#[derive(Debug, Deserialize, Serialize)]
pub struct ResponseEnvelope {
    pub version: u16,
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

impl ResponseEnvelope {
    fn success(id: u64, result: Value) -> Self {
        Self { version: PROTOCOL_VERSION, id, ok: true, result: Some(result), error: None }
    }

    fn error(id: u64, error: IpcError) -> Self {
        Self { version: PROTOCOL_VERSION, id, ok: false, result: None, error: Some(error) }
    }
}

/// Subscription event envelope.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SubscriptionEventEnvelope {
    /// The protocol version, the same one requests and responses carry.
    ///
    /// This was a literal `1` written when the protocol was version 1, and it stayed behind when
    /// the protocol moved to 2 — so an event frame disagreed with every other frame on the same
    /// connection. [`SubscriptionEventEnvelope::new`] is the only place it is set.
    pub version: u16,
    pub subscription_id: u64,
    pub event_sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<u64>,
    pub event: Value,
}

impl SubscriptionEventEnvelope {
    /// An event frame for the current protocol version.
    pub fn new(
        subscription_id: u64,
        event_sequence: u64,
        window_id: Option<u64>,
        event: Value,
    ) -> Self {
        Self { version: PROTOCOL_VERSION, subscription_id, event_sequence, window_id, event }
    }
}

/// A request delivered to the main UI event loop.
#[derive(Clone)]
pub struct IpcRequest {
    pub connection: IpcConnection,
    pub id: u64,
    pub method: String,
    pub params: Value,
}

impl fmt::Debug for IpcRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IpcRequest")
            .field("connection_id", &self.connection.id())
            .field("id", &self.id)
            .field("method", &self.method)
            .field("params", &"<redacted>")
            .finish()
    }
}

/// Cloneable response/event endpoint for one socket connection.
#[derive(Clone)]
pub struct IpcConnection {
    inner: Arc<ConnectionInner>,
}

struct ConnectionInner {
    id: u64,
    output: SyncSender<OutputFrame>,
    in_flight: Mutex<HashSet<u64>>,
    alive: AtomicBool,
    shutdown: Mutex<Option<LocalStream>>,
}

impl fmt::Debug for IpcConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("IpcConnection").field("id", &self.id()).finish()
    }
}

impl IpcConnection {
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }

    fn close(&self) {
        self.inner.alive.store(false, Ordering::Release);
        if let Some(stream) = self.inner.shutdown.lock().unwrap().take() {
            let _ = stream.shutdown();
        }
    }

    pub fn reply(&self, request_id: u64, result: Value) {
        self.finish_request(request_id, ResponseEnvelope::success(request_id, result));
    }

    pub fn error(&self, request_id: u64, error: IpcError) {
        self.finish_request(request_id, ResponseEnvelope::error(request_id, error));
    }

    fn protocol_error(&self, request_id: u64, error: IpcError) {
        let _ = self.queue_json(&ResponseEnvelope::error(request_id, error));
    }

    pub fn event(
        &self,
        event: SubscriptionEventEnvelope,
        queued_events: &Arc<AtomicUsize>,
    ) -> Result<(), IpcError> {
        let slot = EventQueueSlot::reserve(queued_events.clone()).ok_or_else(|| {
            IpcError::new("subscription_overflow", "subscriber event queue is full")
        })?;
        self.queue_json_with_slot(&event, Some(slot)).map_err(|kind| match kind {
            QueueError::TooLarge => IpcError::new("limit_exceeded", "IPC event exceeds 16 MiB"),
            QueueError::Full => {
                IpcError::new("subscription_overflow", "subscriber output queue is full")
            },
            QueueError::Closed => IpcError::new("invalid_request", "IPC connection is closed"),
            QueueError::Serialize(message) => IpcError::new("invalid_request", message),
        })
    }

    fn finish_request(&self, request_id: u64, response: ResponseEnvelope) {
        self.inner.in_flight.lock().unwrap().remove(&request_id);
        match self.queue_json(&response) {
            Err(QueueError::TooLarge) => {
                if self
                    .queue_json(&ResponseEnvelope::error(
                        request_id,
                        IpcError::new("limit_exceeded", "encoded IPC reply exceeds 16 MiB"),
                    ))
                    .is_err()
                {
                    self.close();
                }
            },
            Err(err) if !matches!(err, QueueError::Closed) => {
                warn!("failed to queue IPC response on connection {}: {err}", self.id());
                self.close();
            },
            Err(_) | Ok(()) => (),
        }
    }

    fn queue_json<T: Serialize>(&self, value: &T) -> Result<(), QueueError> {
        self.queue_json_with_slot(value, None)
    }

    fn queue_json_with_slot<T: Serialize>(
        &self,
        value: &T,
        event_slot: Option<EventQueueSlot>,
    ) -> Result<(), QueueError> {
        if !self.is_alive() {
            return Err(QueueError::Closed);
        }
        let mut frame =
            serde_json::to_vec(value).map_err(|err| QueueError::Serialize(err.to_string()))?;
        frame.push(b'\n');
        if frame.len() > MAX_REPLY_FRAME_BYTES {
            return Err(QueueError::TooLarge);
        }
        self.inner.output.try_send(OutputFrame { bytes: frame, _event_slot: event_slot }).map_err(
            |err| match err {
                TrySendError::Full(_) => QueueError::Full,
                TrySendError::Disconnected(_) => QueueError::Closed,
            },
        )
    }
}

pub(crate) struct OutputFrame {
    bytes: Vec<u8>,
    _event_slot: Option<EventQueueSlot>,
}

struct EventQueueSlot(Arc<AtomicUsize>);

impl EventQueueSlot {
    fn reserve(counter: Arc<AtomicUsize>) -> Option<Self> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                (queued < MAX_SUBSCRIBER_EVENTS).then_some(queued + 1)
            })
            .ok()
            .map(|_| Self(counter))
    }
}

impl Drop for EventQueueSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
enum QueueError {
    TooLarge,
    Full,
    Closed,
    Serialize(String),
}

impl fmt::Display for QueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("frame is too large"),
            Self::Full => formatter.write_str("output queue is full"),
            Self::Closed => formatter.write_str("connection is closed"),
            Self::Serialize(message) => write!(formatter, "serialization failed: {message}"),
        }
    }
}

struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// IPC socket listener.
pub struct IpcListener {
    pub socket: LocalListener,
    event_proxy: EventSink,
    connection_count: Arc<AtomicUsize>,
    next_connection_id: AtomicU64,
}

impl IpcListener {
    pub fn new(options: &Options, event_proxy: EventSink, path: &Path) -> Result<Self, IoError> {
        let socket = bind_socket(path)?;
        unsafe { env::set_var(VIVIDO_SOCKET_ENV, path.as_os_str()) };
        let _ = INSTANCE.set(Instance {
            headless: options.headless,
            session: options.session.clone(),
            automation_name: options.automation_name.clone().or_else(|| options.session.clone()),
        });

        Ok(Self {
            socket,
            event_proxy,
            connection_count: Arc::new(AtomicUsize::new(0)),
            next_connection_id: AtomicU64::new(1),
        })
    }

    /// Accept and start one persistent full-duplex IPC session.
    pub fn process_message(&mut self) -> Result<(), IoError> {
        // `accept` verifies the peer's process owner in addition to the endpoint ACL/mode.
        let stream = self.socket.accept()?;

        let previous = self.connection_count.fetch_add(1, Ordering::AcqRel);
        if previous >= MAX_CONNECTIONS {
            self.connection_count.fetch_sub(1, Ordering::AcqRel);
            send_direct_error(
                stream,
                0,
                IpcError::new("limit_exceeded", "Vivido accepts at most 32 IPC connections"),
            );
            return Ok(());
        }

        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        spawn_connection(
            stream,
            connection_id,
            self.event_proxy.clone(),
            ConnectionGuard(self.connection_count.clone()),
        );
        Ok(())
    }
}

fn spawn_connection(
    stream: LocalStream,
    connection_id: u64,
    event_proxy: EventSink,
    guard: ConnectionGuard,
) {
    // The listener is nonblocking for the polling thread. Accepted sockets inherit that flag on
    // some Unix platforms (notably macOS), but each connection has a dedicated reader thread and
    // must block while the client waits for a reply before sending its next request.
    if let Err(err) = configure_connection(&stream) {
        error!("failed to configure IPC connection: {err}");
        return;
    }
    let writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(err) => {
            error!("failed to clone IPC connection: {err}");
            return;
        },
    };
    let shutdown = match stream.try_clone() {
        Ok(shutdown) => shutdown,
        Err(err) => {
            error!("failed to clone IPC connection shutdown handle: {err}");
            return;
        },
    };
    let (output, output_rx) = mpsc::sync_channel::<OutputFrame>(OUTPUT_QUEUE_FRAMES);
    let inner = Arc::new(ConnectionInner {
        id: connection_id,
        output,
        in_flight: Mutex::new(HashSet::new()),
        alive: AtomicBool::new(true),
        shutdown: Mutex::new(Some(shutdown)),
    });
    let writer_inner = Arc::downgrade(&inner);
    thread::spawn_named("IPC writer", move || {
        let result =
            client_fault::catch(ClientFaultClass::Ipc, "IPC writer worker panicked", || {
                let mut writer = writer;
                let _ = writer.set_write_timeout(Some(IPC_WRITE_TIMEOUT));
                while let Ok(frame) = output_rx.recv() {
                    if writer.write_all(&frame.bytes).and_then(|()| writer.flush()).is_err() {
                        let _ = writer.shutdown();
                        break;
                    }
                }
            });
        if let Err(fault) = result {
            error!("contained IPC writer fault {}", fault.id);
        }
        if let Some(writer_inner) = writer_inner.upgrade() {
            writer_inner.alive.store(false, Ordering::Release);
        }
    });

    thread::spawn_named("IPC reader", move || {
        let _guard = guard;
        let connection = IpcConnection { inner };
        let result =
            client_fault::catch(ClientFaultClass::Ipc, "IPC reader worker panicked", || {
                run_connection(stream, connection.clone(), &event_proxy)
            });
        if let Err(fault) = result {
            error!("contained IPC reader fault {}", fault.id);
        }
        connection.inner.alive.store(false, Ordering::Release);
        let _ = event_proxy.send_event(Event::new(EventType::IpcDisconnect(connection_id), None));
    });
}

fn configure_connection(stream: &LocalStream) -> io::Result<()> {
    stream.set_nonblocking(false)
}

fn run_connection(stream: LocalStream, connection: IpcConnection, event_proxy: &EventSink) {
    let mut reader = BufReader::new(stream);
    let Some(first) = read_request_frame(&mut reader, &connection) else {
        return;
    };

    let first = match decode_request(&first) {
        Ok(first) => first,
        Err(error) => {
            connection.error(0, error);
            return;
        },
    };
    if first.version != PROTOCOL_VERSION {
        connection.error(
            first.id,
            IpcError::new("unsupported_version", "Vivido IPC requires protocol version 2")
                .with_data(json!({"supported_versions": [PROTOCOL_VERSION]})),
        );
        return;
    }
    if first.method != "hello" {
        connection.error(
            first.id,
            IpcError::new("invalid_request", "the first IPC request must be hello"),
        );
        return;
    }
    if !insert_request_id(&connection, first.id) {
        return;
    }
    connection.reply(first.id, hello_result());

    while let Some(frame) = read_request_frame(&mut reader, &connection) {
        let request = match decode_request(&frame) {
            Ok(request) => request,
            Err(error) => {
                connection.protocol_error(0, error);
                continue;
            },
        };
        if !insert_request_id(&connection, request.id) {
            continue;
        }
        if request.version != PROTOCOL_VERSION {
            connection.error(
                request.id,
                IpcError::new("unsupported_version", "Vivido IPC requires protocol version 2"),
            );
            continue;
        }
        if request.method == "hello" {
            connection.error(
                request.id,
                IpcError::new("invalid_request", "hello is only valid as the first request"),
            );
            continue;
        }

        let ipc_request = IpcRequest {
            connection: connection.clone(),
            id: request.id,
            method: request.method,
            params: request.params,
        };
        if event_proxy.send_event(Event::new(EventType::IpcRequest(ipc_request), None)).is_err() {
            connection.error(
                request.id,
                IpcError::new("unsupported", "Vivido event loop is shutting down"),
            );
            break;
        }
    }
}

fn insert_request_id(connection: &IpcConnection, id: u64) -> bool {
    let mut in_flight = connection.inner.in_flight.lock().unwrap();
    if in_flight.contains(&id) {
        drop(in_flight);
        connection.protocol_error(
            id,
            IpcError::new("duplicate_request_id", format!("request ID {id} is already active")),
        );
        return false;
    }
    if in_flight.len() >= MAX_IN_FLIGHT_REQUESTS {
        drop(in_flight);
        connection.protocol_error(
            id,
            IpcError::new("limit_exceeded", "at most 64 requests may be in flight"),
        );
        return false;
    }
    in_flight.insert(id);
    true
}

fn read_request_frame<R: BufRead>(reader: &mut R, connection: &IpcConnection) -> Option<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(_) => return None,
        };
        if available.is_empty() {
            return (!frame.is_empty()).then_some(frame);
        }
        let take =
            available.iter().position(|byte| *byte == b'\n').map_or(available.len(), |i| i + 1);
        let remaining = MAX_REQUEST_FRAME_BYTES.saturating_add(1).saturating_sub(frame.len());
        frame.extend_from_slice(&available[..take.min(remaining)]);
        reader.consume(take);
        if frame.len() > MAX_REQUEST_FRAME_BYTES || take > remaining {
            connection.protocol_error(
                0,
                IpcError::new("limit_exceeded", "IPC request frame exceeds 1 MiB"),
            );
            return None;
        }
        if frame.last() == Some(&b'\n') {
            return Some(frame);
        }
    }
}

fn decode_request(frame: &[u8]) -> Result<RequestEnvelope, IpcError> {
    serde_json::from_slice(frame)
        .map_err(|err| IpcError::new("invalid_request", format!("invalid IPC request: {err}")))
}

fn hello_result() -> Value {
    let instance = INSTANCE.get();
    json!({
        "server_version": env!("CARGO_PKG_VERSION"),
        "protocol_version": PROTOCOL_VERSION,
        // Lets an automation client tell a windowless instance from a windowed one without
        // inferring it from a failed `focus`.
        "headless": instance.is_some_and(|instance| instance.headless),
        "session": instance.and_then(|instance| instance.session.clone()),
        "automation_name": instance.and_then(|instance| instance.automation_name.clone()),
        "methods": advertised_methods(),
        "method_capabilities": advertised_method_capabilities(),
        "event_kinds": EVENT_KINDS,
        "error_codes": [
            "unsupported_version", "invalid_request", "invalid_params",
            "duplicate_request_id", "limit_exceeded", "window_not_found",
            "no_focused_window", "unsupported", "timeout", "sequence_gap", "pty_closed",
            "resize_mismatch", "focus_denied", "regex_invalid", "subscription_overflow",
            "invalid_state", "client_fault"
        ],
        "limits": {
            "request_frame_bytes": MAX_REQUEST_FRAME_BYTES,
            "reply_event_frame_bytes": MAX_REPLY_FRAME_BYTES,
            "connections": MAX_CONNECTIONS,
            "in_flight_requests_per_connection": MAX_IN_FLIGHT_REQUESTS,
            "subscriptions": MAX_SUBSCRIPTIONS,
            "queued_events_per_subscriber": MAX_SUBSCRIBER_EVENTS,
            "transcript_bytes_per_window": 1024 * 1024,
            "event_replay_bytes": 4 * 1024 * 1024,
            "event_replay_count": 4096,
        }
    })
}

fn send_direct_error(mut stream: LocalStream, id: u64, error: IpcError) {
    let response = ResponseEnvelope::error(id, error);
    if let Ok(mut frame) = serde_json::to_vec(&response) {
        frame.push(b'\n');
        let _ = stream.write_all(&frame);
    }
}

/// Bind and secure the Vivido IPC socket.
fn bind_socket(path: &Path) -> io::Result<LocalListener> {
    let socket = LocalListener::bind(path)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

const AUTOMATION_PLAN_VERSION: u16 = 1;
const MAX_AUTOMATION_PLAN_STEPS: usize = 256;
const MAX_PLAN_NAME_BYTES: usize = 64;

struct AutomationClient {
    stream: LocalStream,
    reader: BufReader<LocalStream>,
    hello: Value,
    next_request_id: u64,
}

impl AutomationClient {
    fn connect(socket: Option<PathBuf>, target: Option<&str>) -> io::Result<Self> {
        let mut stream = find_socket(socket, target)?;
        stream.set_nonblocking(false)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        send_client_request(&mut stream, 1, "hello", json!({}))?;
        let hello = read_client_response(&mut reader, 1)?;
        Ok(Self { stream, reader, hello, next_request_id: 2 })
    }

    fn request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        validate_method_name(method)?;
        grant_focus_activation_for_method(&self.stream, method);
        let id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or_else(|| IoError::other("IPC request ID exhausted"))?;
        send_client_request(&mut self.stream, id, method, params)?;
        read_client_response(&mut self.reader, id)
    }
}

fn validate_method_name(method: &str) -> io::Result<()> {
    if method.is_empty()
        || method.len() > 128
        || !method.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(IoError::new(
            ErrorKind::InvalidInput,
            "IPC method must contain 1-128 ASCII letters, digits, or underscores",
        ));
    }
    Ok(())
}

fn read_plan(options: &IpcRunPlan) -> io::Result<IpcAutomationPlan> {
    let mut bytes = Vec::new();
    match options.file.as_deref() {
        Some(path) if path != Path::new("-") => {
            fs::File::open(path)?
                .take((MAX_REQUEST_FRAME_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
        },
        _ => {
            io::stdin().take((MAX_REQUEST_FRAME_BYTES + 1) as u64).read_to_end(&mut bytes)?;
        },
    };
    if bytes.len() > MAX_REQUEST_FRAME_BYTES {
        return Err(IoError::new(ErrorKind::InvalidInput, "automation plan exceeds 1 MiB"));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        IoError::new(ErrorKind::InvalidInput, format!("invalid plan JSON: {error}"))
    })
}

fn valid_plan_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PLAN_NAME_BYTES
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn collect_references(value: &Value, references: &mut Vec<String>) -> io::Result<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_references(value, references)?;
            }
        },
        Value::Object(map) if map.len() == 1 && map.contains_key("$ref") => {
            let reference = map["$ref"].as_str().ok_or_else(|| {
                IoError::new(ErrorKind::InvalidInput, "$ref value must be a string")
            })?;
            references.push(reference.to_owned());
        },
        Value::Object(map) => {
            for value in map.values() {
                collect_references(value, references)?;
            }
        },
        _ => (),
    }
    Ok(())
}

fn validate_plan(plan: &IpcAutomationPlan, methods: &HashSet<String>) -> io::Result<()> {
    if plan.version != AUTOMATION_PLAN_VERSION {
        return Err(IoError::new(
            ErrorKind::InvalidInput,
            format!("unsupported automation plan version {}", plan.version),
        ));
    }
    if plan.steps.is_empty() || plan.steps.len() > MAX_AUTOMATION_PLAN_STEPS {
        return Err(IoError::new(
            ErrorKind::InvalidInput,
            format!("automation plan must contain 1 through {MAX_AUTOMATION_PLAN_STEPS} steps"),
        ));
    }

    let mut ids = HashSet::new();
    let mut aliases = HashSet::new();
    for step in &plan.steps {
        if !valid_plan_name(&step.id) || !ids.insert(step.id.clone()) {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                format!("invalid or duplicate plan step ID {:?}", step.id),
            ));
        }
        validate_method_name(&step.method)?;
        if step.method == "hello" || !methods.contains(&step.method) {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                format!("plan step {:?} uses unsupported method {:?}", step.id, step.method),
            ));
        }
        if !step.params.is_object() {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                format!("plan step {:?} params must be a JSON object", step.id),
            ));
        }
        let mut references = Vec::new();
        collect_references(&step.params, &mut references)?;
        if let Some(verification) = &step.verify {
            collect_references(&verification.window_id, &mut references)?;
            if !(1..=86_400_000).contains(&verification.timeout) {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    format!("plan step {:?} verification timeout is invalid", step.id),
                ));
            }
        }
        if let Some(condition) = &step.when {
            references.push(condition.reference.clone());
        }
        if let Some(reference) = references.iter().find(|reference| !aliases.contains(*reference)) {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                format!("plan step {:?} has unavailable reference {:?}", step.id, reference),
            ));
        }
        for (alias, pointer) in &step.bind {
            if !valid_plan_name(alias) || !aliases.insert(alias.clone()) {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    format!("invalid or duplicate plan alias {alias:?}"),
                ));
            }
            if !pointer.is_empty() && !pointer.starts_with('/') {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    format!("binding {alias:?} must use a JSON Pointer"),
                ));
            }
        }
    }
    Ok(())
}

fn resolve_plan_references(value: &Value, aliases: &BTreeMap<String, Value>) -> io::Result<Value> {
    match value {
        Value::Array(values) => values
            .iter()
            .map(|value| resolve_plan_references(value, aliases))
            .collect::<io::Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(map) if map.len() == 1 && map.contains_key("$ref") => {
            let reference = map["$ref"].as_str().ok_or_else(|| {
                IoError::new(ErrorKind::InvalidInput, "$ref value must be a string")
            })?;
            aliases.get(reference).cloned().ok_or_else(|| {
                IoError::new(
                    ErrorKind::InvalidInput,
                    format!("plan reference {reference:?} is unavailable"),
                )
            })
        },
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| Ok((key.clone(), resolve_plan_references(value, aliases)?)))
            .collect::<io::Result<serde_json::Map<_, _>>>()
            .map(Value::Object),
        _ => Ok(value.clone()),
    }
}

fn plan_capabilities(hello: &Value) -> io::Result<Vec<MethodCapability>> {
    if let Some(capabilities) = hello.get("method_capabilities") {
        return serde_json::from_value(capabilities.clone()).map_err(|error| {
            IoError::new(ErrorKind::InvalidData, format!("invalid method capabilities: {error}"))
        });
    }
    Ok(hello
        .get("methods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| {
            let (class, mutating) = method_class(name);
            MethodCapability { name: name.to_owned(), class, mutating, host_claimed: false }
        })
        .collect())
}

fn write_plan_event(output: &mut impl Write, value: Value) -> io::Result<()> {
    write_json_to(output, &value)?;
    output.flush()
}

fn resolve_verification_window(
    step: &IpcAutomationPlanStep,
    aliases: &BTreeMap<String, Value>,
) -> io::Result<Option<(u64, u64, bool)>> {
    let Some(verification) = &step.verify else {
        return Ok(None);
    };
    let window_id =
        resolve_plan_references(&verification.window_id, aliases)?.as_u64().ok_or_else(|| {
            IoError::new(ErrorKind::InvalidInput, "verification window_id is not u64")
        })?;
    Ok(Some((window_id, verification.timeout, verification.screenshot)))
}

fn run_plan(socket: Option<PathBuf>, target: Option<&str>, options: &IpcRunPlan) -> io::Result<()> {
    let plan = read_plan(options)?;
    let mut client = AutomationClient::connect(socket, target)?;
    let capabilities = plan_capabilities(&client.hello)?;
    let methods =
        capabilities.iter().map(|capability| capability.name.clone()).collect::<HashSet<_>>();
    validate_plan(&plan, &methods)?;
    let classes = capabilities
        .iter()
        .map(|capability| (capability.name.as_str(), capability))
        .collect::<HashMap<_, _>>();
    let mut output = io::stdout().lock();
    write_plan_event(
        &mut output,
        json!({"type":"plan_started","version":plan.version,"steps":plan.steps.len(),"mode":if options.dry_run {"dry_run"} else if options.preflight {"preflight"} else {"execute"}}),
    )?;

    let mut aliases = BTreeMap::new();
    let mut failures = 0_u64;
    for step in &plan.steps {
        let capability = classes[step.method.as_str()];
        if options.dry_run {
            write_plan_event(
                &mut output,
                json!({"type":"step","id":step.id,"method":step.method,"class":capability.class,"mutating":capability.mutating,"status":"planned"}),
            )?;
            continue;
        }
        if options.preflight && capability.mutating {
            write_plan_event(
                &mut output,
                json!({"type":"step","id":step.id,"method":step.method,"class":capability.class,"mutating":true,"status":"skipped","reason":"preflight_mutation"}),
            )?;
            continue;
        }
        if options.preflight {
            let mut references = Vec::new();
            collect_references(&step.params, &mut references)?;
            if let Some(verification) = &step.verify {
                collect_references(&verification.window_id, &mut references)?;
            }
            if let Some(condition) = &step.when {
                references.push(condition.reference.clone());
            }
            if references.iter().any(|reference| !aliases.contains_key(reference)) {
                write_plan_event(
                    &mut output,
                    json!({"type":"step","id":step.id,"method":step.method,"status":"skipped","reason":"dependency_unavailable"}),
                )?;
                continue;
            }
        }
        if let Some(condition) = &step.when
            && aliases.get(&condition.reference) != Some(&condition.equals)
        {
            write_plan_event(
                &mut output,
                json!({"type":"step","id":step.id,"method":step.method,"status":"skipped","reason":"condition_false"}),
            )?;
            continue;
        }

        let execution = (|| -> io::Result<Value> {
            let params = resolve_plan_references(&step.params, &aliases)?;
            if serde_json::to_vec(&params).map_err(IoError::other)?.len() > MAX_REQUEST_FRAME_BYTES
            {
                return Err(IoError::new(
                    ErrorKind::InvalidInput,
                    format!("resolved params for step {:?} exceed 1 MiB", step.id),
                ));
            }
            let verification = resolve_verification_window(step, &aliases)?;
            let before_frame = if let Some((window_id, _, _)) = verification {
                Some(
                    client
                        .request("inspect", json!({"window_id":window_id}))?
                        .pointer("/window/sequences/frame")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| {
                            IoError::new(
                                ErrorKind::InvalidData,
                                "inspect reply is missing frame sequence",
                            )
                        })?,
                )
            } else {
                None
            };
            let action = client.request(&step.method, params)?;
            let mut result = json!({"action":action});
            if let Some((window_id, timeout, screenshot)) = verification {
                let mut verification_result = serde_json::Map::new();
                if step.verify.as_ref().is_some_and(|verification| verification.frame_changed) {
                    let frame = client.request(
                        "wait_frame",
                        json!({"after_frame":before_frame,"common":{"timeout":timeout,"target":{"window_id":window_id}}}),
                    )?;
                    verification_result.insert("frame".into(), frame);
                }
                if screenshot {
                    let screenshot =
                        client.request("screenshot", json!({"window_id":window_id}))?;
                    verification_result.insert("screenshot".into(), screenshot);
                }
                result["verification"] = Value::Object(verification_result);
            }
            Ok(result)
        })();

        match execution {
            Ok(result) => {
                let binding_source = result.get("action").unwrap_or(&result);
                let binding_result = step.bind.iter().try_for_each(|(alias, pointer)| {
                    let value = if pointer.is_empty() {
                        Some(binding_source)
                    } else {
                        binding_source.pointer(pointer)
                    }
                    .ok_or_else(|| {
                        IoError::new(
                            ErrorKind::InvalidData,
                            format!("step {:?} result has no binding pointer {pointer:?}", step.id),
                        )
                    })?;
                    aliases.insert(alias.clone(), value.clone());
                    Ok::<_, io::Error>(())
                });
                match binding_result {
                    Ok(()) => write_plan_event(
                        &mut output,
                        json!({"type":"step","id":step.id,"method":step.method,"class":capability.class,"mutating":capability.mutating,"status":"ok","result":result}),
                    )?,
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        write_plan_event(
                            &mut output,
                            json!({"type":"step","id":step.id,"method":step.method,"status":"error","error":error.to_string()}),
                        )?;
                        if step.on_error == IpcPlanErrorPolicy::Abort {
                            write_plan_event(
                                &mut output,
                                json!({"type":"plan_completed","status":"failed","failures":failures}),
                            )?;
                            return Err(IoError::other(format!(
                                "automation plan failed while binding step {:?}",
                                step.id
                            )));
                        }
                    },
                }
            },
            Err(error) => {
                failures = failures.saturating_add(1);
                write_plan_event(
                    &mut output,
                    json!({"type":"step","id":step.id,"method":step.method,"status":"error","error":error.to_string()}),
                )?;
                if step.on_error == IpcPlanErrorPolicy::Abort {
                    write_plan_event(
                        &mut output,
                        json!({"type":"plan_completed","status":"failed","failures":failures}),
                    )?;
                    return Err(IoError::other(format!(
                        "automation plan failed at step {:?}",
                        step.id
                    )));
                }
            },
        }
    }

    write_plan_event(
        &mut output,
        json!({"type":"plan_completed","status":if failures == 0 {"ok"} else {"completed_with_errors"},"failures":failures}),
    )?;
    if failures == 0 {
        Ok(())
    } else {
        Err(IoError::other("automation plan completed with errors"))
    }
}

fn run_capture(
    socket: Option<PathBuf>,
    target: Option<&str>,
    options: &IpcCapture,
) -> io::Result<()> {
    if !(1..=86_400_000).contains(&options.timeout) {
        return Err(IoError::new(ErrorKind::InvalidInput, "timeout must be 1 ms through 24 hours"));
    }
    let mut client = AutomationClient::connect(socket, target)?;
    let methods = client
        .hello
        .get("methods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    if options.activate {
        let window_id = options.window_id.ok_or_else(|| {
            IoError::new(ErrorKind::InvalidInput, "--activate requires --window-id")
        })?;
        if !methods.contains("vivida_activate_pane") {
            return Err(IoError::new(
                ErrorKind::Unsupported,
                "target does not advertise pane activation",
            ));
        }
        client.request("vivida_activate_pane", json!({"window_id":window_id}))?;
    }
    if let Some(after_frame) = options.after_frame {
        client.request(
            "wait_frame",
            json!({"after_frame":after_frame,"common":{"timeout":options.timeout,"target":{"window_id":options.window_id}}}),
        )?;
    }
    if let Some(quiet) = options.stable {
        client.request(
            "wait_screen_stable",
            json!({"quiet":quiet,"after_screen":null,"common":{"timeout":options.timeout,"target":{"window_id":options.window_id}}}),
        )?;
    }
    let screenshot = client.request("screenshot", json!({"window_id":options.window_id}))?;
    write_json(&screenshot)
}

/// Send one CLI command using a versioned protocol session.
pub fn send_message(options: MessageOptions) -> io::Result<()> {
    if let SocketMessage::RunPlan(params) = &options.message {
        return run_plan(options.socket, options.target.as_deref(), params);
    }
    if let SocketMessage::Capture(params) = &options.message {
        return run_capture(options.socket, options.target.as_deref(), params);
    }
    validate_message(&options.message)?;
    let mut stream = find_socket(options.socket, options.target.as_deref())?;
    stream.set_nonblocking(false)?;
    grant_focus_activation_if_requested(&stream, &options.message);
    let mut reader = BufReader::new(stream.try_clone()?);

    send_client_request(&mut stream, 1, "hello", json!({}))?;
    let hello = read_client_response(&mut reader, 1)?;
    if matches!(options.message, SocketMessage::Capabilities) {
        return write_json(&hello);
    }

    if let SocketMessage::Vivid { command: IpcVividCommand::Trace(params) } = &options.message
        && params.follow
    {
        return run_vivid_trace_follow(&mut stream, &mut reader, params.clone());
    }

    let (method, params) = message_request(&options.message)?;
    send_client_request(&mut stream, 2, method, params)?;
    let result = read_client_response(&mut reader, 2)?;
    write_cli_result(&options.message, &result)?;

    if matches!(options.message, SocketMessage::Subscribe(_)) {
        let mut stdout = io::stdout().lock();
        loop {
            let Some(frame) = read_client_frame(&mut reader)? else {
                return Ok(());
            };
            let event: SubscriptionEventEnvelope = serde_json::from_slice(&frame)
                .map_err(|err| IoError::new(ErrorKind::InvalidData, err))?;
            serde_json::to_writer(&mut stdout, &event).map_err(IoError::other)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }

    Ok(())
}

/// Issue one bounded automation request without rendering CLI output.
pub fn request_once(
    socket: Option<PathBuf>,
    target: Option<&str>,
    message: &SocketMessage,
) -> io::Result<(Value, Value)> {
    validate_message(message)?;
    let mut stream = find_socket(socket, target)?;
    stream.set_nonblocking(false)?;
    grant_focus_activation_if_requested(&stream, message);
    let mut reader = BufReader::new(stream.try_clone()?);
    send_client_request(&mut stream, 1, "hello", json!({}))?;
    let hello = read_client_response(&mut reader, 1)?;
    if matches!(message, SocketMessage::Capabilities) {
        return Ok((hello.clone(), hello));
    }
    let (method, params) = message_request(message)?;
    send_client_request(&mut stream, 2, method, params)?;
    let result = read_client_response(&mut reader, 2)?;
    Ok((hello, result))
}

#[cfg(windows)]
fn grant_focus_activation_if_requested(stream: &LocalStream, message: &SocketMessage) {
    if !matches!(message, SocketMessage::Focus(_)) {
        return;
    }
    let Ok(server_pid) = stream.server_process_id() else {
        return;
    };
    // Windows accepts this grant only when the caller is itself foreground-eligible. Failure is
    // deliberately non-fatal: the server may already be foreground and still confirm focus.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(server_pid);
    }
}

#[cfg(not(windows))]
fn grant_focus_activation_if_requested(_stream: &LocalStream, _message: &SocketMessage) {}

#[cfg(windows)]
fn grant_focus_activation_for_method(stream: &LocalStream, method: &str) {
    if method != "focus" {
        return;
    }
    let Ok(server_pid) = stream.server_process_id() else {
        return;
    };
    // SAFETY: This only grants the owner-verified local server process permission to request
    // foreground activation; Windows still applies its normal foreground policy.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(server_pid);
    }
}

#[cfg(not(windows))]
fn grant_focus_activation_for_method(_stream: &LocalStream, _method: &str) {}

/// Issue one bounded automation request by wire method name.
///
/// This is the extension point for an embedding host which adds methods beyond
/// [`SocketMessage`]. It performs the same endpoint discovery, owner checks, handshake, framing,
/// and response validation as [`send_message`].
pub fn request_method(
    socket: Option<PathBuf>,
    target: Option<&str>,
    method: &str,
    params: Value,
) -> io::Result<(Value, Value)> {
    validate_method_name(method)?;
    let mut stream = find_socket(socket, target)?;
    stream.set_nonblocking(false)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    send_client_request(&mut stream, 1, "hello", json!({}))?;
    let hello = read_client_response(&mut reader, 1)?;
    send_client_request(&mut stream, 2, method, params)?;
    let result = read_client_response(&mut reader, 2)?;
    Ok((hello, result))
}

fn run_vivid_trace_follow(
    stream: &mut LocalStream,
    reader: &mut BufReader<LocalStream>,
    mut params: crate::cli::IpcVividTrace,
) -> io::Result<()> {
    let mut request_id = 2_u64;
    let mut stdout = io::stdout().lock();
    loop {
        send_client_request(stream, request_id, "vivid_trace", vivid_trace_params(&params)?)?;
        let batch = read_client_response(reader, request_id)?;
        if let Some(gap) = batch.get("gap") {
            write_json_to(&mut stdout, &json!({"type": "gap", "gap": gap}))?;
        }
        if let Some(events) = batch.get("events").and_then(Value::as_array) {
            for event in events {
                write_json_to(&mut stdout, event)?;
            }
        }
        stdout.flush()?;
        params.after = batch.get("current_sequence").and_then(Value::as_u64);
        request_id = request_id
            .checked_add(1)
            .ok_or_else(|| IoError::other("Vivid trace request ID exhausted"))?;
    }
}

fn send_client_request(
    stream: &mut LocalStream,
    id: u64,
    method: &str,
    params: Value,
) -> io::Result<()> {
    let request = RequestEnvelope { version: PROTOCOL_VERSION, id, method: method.into(), params };
    let mut frame = serde_json::to_vec(&request).map_err(IoError::other)?;
    frame.push(b'\n');
    if frame.len() > MAX_REQUEST_FRAME_BYTES {
        return Err(IoError::new(ErrorKind::InvalidInput, "IPC request exceeds 1 MiB"));
    }
    stream.write_all(&frame)?;
    stream.flush()
}

fn read_client_response<R: BufRead>(reader: &mut R, expected_id: u64) -> io::Result<Value> {
    loop {
        let Some(frame) = read_client_frame(reader)? else {
            return Err(IoError::new(ErrorKind::UnexpectedEof, "Vivido closed the IPC connection"));
        };
        let value: Value = serde_json::from_slice(&frame)
            .map_err(|err| IoError::new(ErrorKind::InvalidData, err))?;
        // Subscription events can be interleaved with later responses on a plan's long-lived
        // connection. They have no request ID, so leave them out of the synchronous reply path.
        if value.get("id").is_none() {
            serde_json::from_value::<SubscriptionEventEnvelope>(value)
                .map_err(|err| IoError::new(ErrorKind::InvalidData, err))?;
            continue;
        }
        let response: ResponseEnvelope = serde_json::from_value(value)
            .map_err(|err| IoError::new(ErrorKind::InvalidData, err))?;
        if response.version != PROTOCOL_VERSION || response.id != expected_id {
            continue;
        }
        if response.ok {
            return Ok(response.result.unwrap_or(Value::Null));
        }
        let error = response
            .error
            .unwrap_or_else(|| IpcError::new("invalid_request", "missing IPC error payload"));
        return Err(IoError::other(format!("{}: {}", error.code, error.message)));
    }
}

fn read_client_frame<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((!frame.is_empty()).then_some(frame));
        }
        let take =
            available.iter().position(|byte| *byte == b'\n').map_or(available.len(), |i| i + 1);
        let remaining = MAX_REPLY_FRAME_BYTES.saturating_add(1).saturating_sub(frame.len());
        frame.extend_from_slice(&available[..take.min(remaining)]);
        reader.consume(take);
        if frame.len() > MAX_REPLY_FRAME_BYTES || take > remaining {
            return Err(IoError::new(ErrorKind::InvalidData, "IPC reply exceeds 16 MiB"));
        }
        if frame.last() == Some(&b'\n') {
            return Ok(Some(frame));
        }
    }
}

fn message_request(message: &SocketMessage) -> io::Result<(&'static str, Value)> {
    match message {
        SocketMessage::CreateWindow(params) => Ok(("create_window", serialize_params(params)?)),
        SocketMessage::Quit => Ok(("quit", Value::Object(Default::default()))),
        SocketMessage::Ping => Ok(("ping", json!({}))),
        SocketMessage::ResetTerminal(params) => Ok(("reset_terminal", serialize_params(params)?)),
        SocketMessage::RestartTerminal(params) => {
            Ok(("restart_terminal", serialize_params(params)?))
        },
        SocketMessage::Config(params) => Ok(("config", serialize_params(params)?)),
        SocketMessage::GetConfig(params) => Ok(("get_config", serialize_params(params)?)),
        SocketMessage::Typing(params) => Ok(("typing", serialize_params(params)?)),
        SocketMessage::GetText(params) => Ok(("get_text", serialize_params(params)?)),
        SocketMessage::Screenshot(params) => Ok(("screenshot", serialize_params(params)?)),
        SocketMessage::Capabilities | SocketMessage::RunPlan(_) | SocketMessage::Capture(_) => {
            unreachable!("client-only automation command")
        },
        SocketMessage::Key(params) => Ok(("key", serialize_params(params)?)),
        SocketMessage::Paste(params) => Ok(("paste", serialize_params(params)?)),
        SocketMessage::Mouse(params) => Ok(("mouse", serialize_params(params)?)),
        SocketMessage::Resize(params) => Ok(("resize", serialize_params(params)?)),
        SocketMessage::SetGeometry(params) => Ok(("set_geometry", serialize_params(params)?)),
        SocketMessage::SetVisible(params) => Ok(("set_visible", serialize_params(params)?)),
        SocketMessage::SetLevel(params) => Ok(("set_level", serialize_params(params)?)),
        SocketMessage::Focus(params) => Ok(("focus", serialize_params(params)?)),
        SocketMessage::Signal(params) => Ok(("signal", serialize_params(params)?)),
        SocketMessage::ListWindows => Ok(("list_windows", json!({}))),
        SocketMessage::Inspect(params) => Ok(("inspect", serialize_params(params)?)),
        SocketMessage::Diagnose(params) => Ok(("diagnose", serialize_params(params)?)),
        SocketMessage::Vivid { command } => match command {
            IpcVividCommand::Sessions(target) => Ok(("vivid_sessions", serialize_params(target)?)),
            IpcVividCommand::Surfaces(target) => Ok(("vivid_surfaces", serialize_params(target)?)),
            IpcVividCommand::SurfaceStatus { identity, target } => Ok((
                "vivid_surface_status",
                json!({
                    "window_id": target.window_id,
                    "session_id": identity.session_id,
                    "context_id": identity.context_id,
                    "surface_id": identity.surface_id,
                }),
            )),
            IpcVividCommand::Tracks(target) => Ok(("vivid_tracks", serialize_params(target)?)),
            IpcVividCommand::TrackStatus { identity, target } => Ok((
                "vivid_track_status",
                json!({
                    "window_id": target.window_id,
                    "session_id": identity.session_id,
                    "context_id": identity.context_id,
                    "surface_id": identity.surface_id,
                    "track_id": identity.track_id,
                }),
            )),
            IpcVividCommand::SceneStatus { session_id, target } => Ok((
                "vivid_scene_status",
                json!({"window_id": target.window_id, "session_id": session_id}),
            )),
            IpcVividCommand::Trace(params) => Ok(("vivid_trace", vivid_trace_params(params)?)),
        },
        SocketMessage::GetGrid(params) => Ok(("get_grid", serialize_params(params)?)),
        SocketMessage::Transcript(params) => Ok(("transcript", serialize_params(params)?)),
        SocketMessage::Subscribe(params) => Ok(("subscribe", serialize_params(params)?)),
        SocketMessage::Wait(params) => match &params.condition {
            IpcWaitCondition::Text(params) => Ok(("wait_text", serialize_params(params)?)),
            IpcWaitCondition::Output(params) => Ok(("wait_output", serialize_params(params)?)),
            IpcWaitCondition::ScreenChange(params) => {
                Ok(("wait_screen_change", serialize_params(params)?))
            },
            IpcWaitCondition::ScreenStable(params) => {
                Ok(("wait_screen_stable", serialize_params(params)?))
            },
            IpcWaitCondition::Frame(params) => Ok(("wait_frame", serialize_params(params)?)),
            IpcWaitCondition::VividTrack(params) => Ok((
                "wait_vivid_track",
                json!({
                    "window_id": params.target.window_id,
                    "session_id": params.identity.session_id,
                    "context_id": params.identity.context_id,
                    "surface_id": params.identity.surface_id,
                    "track_id": params.identity.track_id,
                    "channel_generation": params.channel_generation,
                    "condition": params.condition.wire_value(),
                    "value": params.value,
                    "timeout": params.timeout,
                }),
            )),
            IpcWaitCondition::Exit(params) => Ok(("wait_exit", serialize_params(params)?)),
        },
    }
}

fn serialize_params<T: Serialize>(params: &T) -> io::Result<Value> {
    serde_json::to_value(params).map_err(IoError::other)
}

fn vivid_trace_params(params: &crate::cli::IpcVividTrace) -> io::Result<Value> {
    let mut value = serialize_params(params)?;
    if let Some(sequence) = params.around {
        value
            .as_object_mut()
            .ok_or_else(|| IoError::new(ErrorKind::InvalidData, "trace params are not an object"))?
            .insert(
                "around".into(),
                json!({
                    "sequence": sequence,
                    "preceding": params.preceding,
                    "following": params.following,
                }),
            );
    }
    Ok(value)
}

fn validate_message(message: &SocketMessage) -> io::Result<()> {
    let input_length = match message {
        SocketMessage::Typing(params) => Some(params.text.len()),
        SocketMessage::Paste(params) => Some(params.text.len()),
        _ => None,
    };
    if input_length.is_some_and(|length| length > MAX_INPUT_BYTES) {
        return Err(IoError::new(
            ErrorKind::InvalidInput,
            format!("terminal input exceeds {MAX_INPUT_BYTES} bytes"),
        ));
    }
    if let SocketMessage::GetText(params) = message
        && params.rows.is_some_and(|rows| rows == 0 || rows > 1000)
    {
        return Err(IoError::new(ErrorKind::InvalidInput, "row count must be between 1 and 1000"));
    }
    if let SocketMessage::Resize(params) = message
        && !matches!(
            (params.columns, params.rows, params.width, params.height),
            (Some(_), Some(_), None, None) | (None, None, Some(_), Some(_))
        )
    {
        return Err(IoError::new(
            ErrorKind::InvalidInput,
            "resize requires either --columns/--rows or --width/--height",
        ));
    }
    if let SocketMessage::SetGeometry(params) = message {
        if params.x.is_none() && params.width.is_none() {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                "set-geometry requires --x/--y, --width/--height, or both",
            ));
        }
        if params.x.is_some() != params.y.is_some()
            || params.width.is_some() != params.height.is_some()
        {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                "set-geometry coordinate and size pairs must be complete",
            ));
        }
    }
    if let SocketMessage::Mouse(params) = message {
        let position = match &params.action {
            IpcMouseAction::Move(position) => position,
            IpcMouseAction::Click(action)
            | IpcMouseAction::DoubleClick(action)
            | IpcMouseAction::Down(action)
            | IpcMouseAction::Up(action)
            | IpcMouseAction::Drag(action) => &action.position,
            IpcMouseAction::Path(path) => {
                if !(2..=1000).contains(&path.points.len())
                    || path.points.iter().any(|point| !point.x.is_finite() || !point.y.is_finite())
                {
                    return Err(IoError::new(
                        ErrorKind::InvalidInput,
                        "mouse path requires 2 through 1000 finite physical-pixel points",
                    ));
                }
                if path.duration.is_some_and(|duration| !(1..=30_000).contains(&duration)) {
                    return Err(IoError::new(
                        ErrorKind::InvalidInput,
                        "paced mouse path duration must be 1 ms through 30 seconds",
                    ));
                }
                if path.wait_frame && !(1..=86_400_000).contains(&path.timeout) {
                    return Err(IoError::new(
                        ErrorKind::InvalidInput,
                        "mouse path timeout must be 1 ms through 24 hours",
                    ));
                }
                return Ok(());
            },
            IpcMouseAction::Scroll(action) => &action.position,
        };
        let cell = position.cell_column.is_some() && position.cell_row.is_some();
        let pixel = position.x.is_some() && position.y.is_some();
        let relative = position.relative_x.is_some() && position.relative_y.is_some();
        if usize::from(cell) + usize::from(pixel) + usize::from(relative) != 1 {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                "mouse requires exactly one complete cell, pixel, or relative coordinate pair",
            ));
        }
        if relative
            && [position.relative_x, position.relative_y]
                .into_iter()
                .flatten()
                .any(|coordinate| !coordinate.is_finite() || !(0.0..=1.0).contains(&coordinate))
        {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                "relative mouse coordinates must be finite values from 0 through 1",
            ));
        }
    }
    if let SocketMessage::Wait(IpcWait { condition: IpcWaitCondition::VividTrack(params) }) =
        message
        && params.condition.requires_value() != params.value.is_some()
    {
        return Err(IoError::new(
            ErrorKind::InvalidInput,
            if params.condition.requires_value() {
                "this Vivid track wait condition requires --value"
            } else {
                "this Vivid track wait condition does not accept --value"
            },
        ));
    }
    if let SocketMessage::Vivid { command: IpcVividCommand::Trace(params) } = message {
        let selectors = usize::from(params.after.is_some())
            + usize::from(params.tail)
            + usize::from(params.before.is_some())
            + usize::from(params.around.is_some());
        let around_count = u32::from(params.preceding) + u32::from(params.following);
        if selectors > 1
            || (params.follow
                && (params.tail || params.before.is_some() || params.around.is_some()))
            || (params.around.is_some()
                && !(1..=u32::from(crate::vivid::trace::MAX_QUERY_EVENTS)).contains(&around_count))
        {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                "invalid Vivid trace selector, follow mode, or around count",
            ));
        }
    }
    Ok(())
}

fn write_cli_result(message: &SocketMessage, result: &Value) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    match message {
        SocketMessage::GetText(_) => stdout
            .write_all(result.get("text").and_then(Value::as_str).unwrap_or_default().as_bytes()),
        SocketMessage::Screenshot(params) if params.json => write_json_to(&mut stdout, result),
        SocketMessage::Screenshot(_) => {
            let path = result.get("path").and_then(Value::as_str).ok_or_else(|| {
                IoError::new(ErrorKind::InvalidData, "screenshot reply is missing path")
            })?;
            writeln!(stdout, "{path}")
        },
        SocketMessage::CreateWindow(_) => {
            let window_id = result.get("window_id").and_then(Value::as_u64).ok_or_else(|| {
                IoError::new(ErrorKind::InvalidData, "create-window reply is missing window_id")
            })?;
            writeln!(stdout, "{window_id}")
        },
        // The instance is shutting down; acknowledging it on stdout would be noise in a script.
        SocketMessage::Quit => Ok(()),
        SocketMessage::GetConfig(_) => {
            let config = result.get("config").unwrap_or(result);
            serde_json::to_writer(&mut stdout, config).map_err(IoError::other)?;
            stdout.write_all(b"\n")
        },
        SocketMessage::Transcript(params) if params.raw => {
            let encoded = result.get("data").and_then(Value::as_str).ok_or_else(|| {
                IoError::new(ErrorKind::InvalidData, "transcript reply is missing data")
            })?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|err| IoError::new(ErrorKind::InvalidData, err))?;
            stdout.write_all(&bytes)
        },
        SocketMessage::Capabilities
        | SocketMessage::RunPlan(_)
        | SocketMessage::Capture(_)
        | SocketMessage::Ping
        | SocketMessage::ResetTerminal(_)
        | SocketMessage::RestartTerminal(_)
        | SocketMessage::ListWindows
        | SocketMessage::Inspect(_)
        | SocketMessage::Diagnose(_)
        | SocketMessage::Vivid { .. }
        | SocketMessage::GetGrid(_)
        | SocketMessage::Wait(_)
        | SocketMessage::Transcript(_)
        | SocketMessage::Subscribe(_) => write_json_to(&mut stdout, result),
        SocketMessage::Typing(params) if params.report => write_json_to(&mut stdout, result),
        SocketMessage::Key(params) if params.report => write_json_to(&mut stdout, result),
        SocketMessage::Paste(params) if params.report => write_json_to(&mut stdout, result),
        SocketMessage::Config(_)
        | SocketMessage::Typing(_)
        | SocketMessage::Key(_)
        | SocketMessage::Paste(_)
        | SocketMessage::Mouse(_)
        | SocketMessage::Resize(_)
        | SocketMessage::SetGeometry(_)
        | SocketMessage::SetVisible(_)
        | SocketMessage::SetLevel(_)
        | SocketMessage::Focus(_)
        | SocketMessage::Signal(_) => Ok(()),
    }
}

fn write_json(value: &Value) -> io::Result<()> {
    write_json_to(&mut io::stdout().lock(), value)
}

fn write_json_to<W: Write>(output: &mut W, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, value).map_err(IoError::other)?;
    output.write_all(b"\n")
}

/// Directory for the IPC socket file.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn socket_dir() -> PathBuf {
    xdg::BaseDirectories::with_prefix("vivido")
        .get_runtime_directory()
        .map(ToOwned::to_owned)
        .ok()
        .and_then(private_socket_dir)
        .unwrap_or_else(env::temp_dir)
}

/// Hold the socket directory to the same standard `crate::session` holds a runtime root to.
///
/// The socket inside it injects input and reads screen content, so the 0600 mode `bind_socket`
/// sets is not the whole defence: a directory another user can write to lets the socket be
/// replaced wholesale rather than read. A directory Vivido has to create is therefore created
/// owner-only rather than at the umask's discretion, and one that already exists is used only if
/// it is this user's and closed to everyone else — otherwise discovery falls back to the temporary
/// directory, where the socket mode and the peer check still stand. A path Vivido does not own is
/// never modified, only declined.
#[cfg(all(unix, not(target_os = "macos")))]
fn private_socket_dir(path: PathBuf) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !path.exists() {
        fs::create_dir_all(&path).ok()?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).ok()?;
    }
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o077 != 0
    {
        warn!("declining IPC socket directory {}: it is not owner-only", path.display());
        return None;
    }
    Some(path)
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Directory for the IPC socket file.
#[cfg(target_os = "macos")]
pub fn socket_dir() -> PathBuf {
    env::temp_dir()
}

/// Default endpoint for a windowed instance.
#[cfg(unix)]
pub fn default_endpoint() -> PathBuf {
    let mut path = socket_dir();
    path.push(format!("{}-{}.sock", socket_prefix(), std::process::id()));
    path
}

/// Default endpoint for a windowed Windows instance.
#[cfg(windows)]
pub fn default_endpoint() -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\Vivido-{}", std::process::id()))
}

/// Connect, refusing a socket that belongs to another user.
///
/// The transport authenticates the peer's uid from both ends once a connection exists, so this is
/// the check that runs *before* one does: a socket another user planted where Vivido looks — macOS
/// still discovers them in the shared `env::temp_dir()` — is declined without a byte being written
/// to it, and reported as what it is rather than as a protocol failure.
fn connect_checked(path: &Path) -> io::Result<LocalStream> {
    #[cfg(unix)]
    require_socket_owner(path)?;
    LocalStream::connect(path)
}

#[cfg(unix)]
fn require_socket_owner(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)?;
    let owner = effective_uid();
    if metadata.file_type().is_symlink() || metadata.uid() != owner {
        return Err(IoError::new(
            ErrorKind::PermissionDenied,
            format!("IPC socket {} is not owned by uid {owner}", path.display()),
        ));
    }
    Ok(())
}

/// Find a socket using an override, inherited endpoint, or current display discovery.
fn find_socket(socket_path: Option<PathBuf>, target: Option<&str>) -> io::Result<LocalStream> {
    if let Some(socket_path) = socket_path {
        return connect_checked(&socket_path).map_err(|err| {
            IoError::new(err.kind(), format!("invalid socket path {socket_path:?}"))
        });
    }

    // An explicitly named session must never silently fall through to a different instance.
    if let Some(target) = target.map(str::to_owned).or_else(|| env::var(VIVIDO_SESSION_ENV).ok()) {
        let registry = crate::session::registered_instance(&target)?;
        return connect_checked(&registry.socket).map_err(|err| {
            IoError::new(err.kind(), format!("no running Vivido instance named {target:?}"))
        });
    }

    if let Ok(path) = env::var(VIVIDO_SOCKET_ENV)
        && let Ok(socket) = connect_checked(Path::new(&path))
    {
        return Ok(socket);
    }

    // A single live headless session is unambiguous, so an unqualified `msg` should reach it.
    if let Ok(sessions) = crate::session::list_registries()
        && let [session] = sessions.as_slice()
        && let Ok(socket) = connect_checked(&session.socket)
    {
        return Ok(socket);
    }

    #[cfg(unix)]
    let mut candidates = Vec::new();
    #[cfg(unix)]
    for entry in fs::read_dir(socket_dir())?.filter_map(Result::ok) {
        let path = entry.path();
        let prefix = socket_prefix();
        if path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|file| file.starts_with(&prefix) && file.ends_with(".sock"))
        {
            candidates.push(path);
        }
    }
    #[cfg(unix)]
    candidates.sort();
    #[cfg(unix)]
    candidates.reverse();
    #[cfg(unix)]
    for path in candidates {
        match connect_checked(&path) {
            Ok(socket) => return Ok(socket),
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                let _ = fs::remove_file(path);
            },
            Err(_) => (),
        }
    }

    Err(IoError::new(ErrorKind::NotFound, "no socket found"))
}

/// File prefix matching sockets on the current display server.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn socket_prefix() -> String {
    let display = env::var("WAYLAND_DISPLAY").or_else(|_| env::var("DISPLAY")).unwrap_or_default();
    format!("Vivido-{}", display.replace('/', "-"))
}

/// File prefix matching sockets on macOS.
#[cfg(target_os = "macos")]
pub fn socket_prefix() -> String {
    String::from("Vivido")
}

/// A connection whose frames land in the returned channel, for tests in this and other modules.
#[cfg(test)]
pub(crate) fn test_connection() -> (IpcConnection, mpsc::Receiver<OutputFrame>) {
    let (output, receiver) = mpsc::sync_channel(OUTPUT_QUEUE_FRAMES);
    let connection = IpcConnection {
        inner: Arc::new(ConnectionInner {
            id: 1,
            output,
            in_flight: Mutex::new(HashSet::new()),
            alive: AtomicBool::new(true),
            shutdown: Mutex::new(None),
        }),
    };
    (connection, receiver)
}

#[cfg(test)]
mod tests {

    #[test]
    fn every_emitted_event_kind_is_advertised() {
        // `EVENT_KINDS` is both the handshake advertisement and the `subscribe` allowlist, so a
        // kind missing from it is delivered to unfiltered subscriptions and rejected when asked for
        // by name. `directory_changed` was in exactly that state: emitted on OSC 7, documented in
        // `docs/ipc.md`, and unreachable. `AutomationHub::emit_payload` now debug-asserts against
        // this list; this pins the kind that was missing and the list's shape.
        assert!(
            EVENT_KINDS.contains(&"directory_changed"),
            "OSC 7 emits directory_changed, so a client must be able to subscribe to it"
        );

        let mut sorted = EVENT_KINDS.to_vec();
        sorted.sort_unstable();
        let unique = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), unique, "an advertised kind is listed twice");
    }

    #[cfg(windows)]
    use std::io::Read;
    use std::io::{BufReader, Write};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::os::unix::io::AsRawFd;

    use serde_json::json;

    use super::*;

    #[test]
    fn bounded_plan_accepts_backward_only_alias_references() {
        let plan: IpcAutomationPlan = serde_json::from_value(json!({
            "version": 1,
            "steps": [
                {
                    "id": "resolve",
                    "method": "inspect",
                    "params": {"window_id": 42},
                    "bind": {"target": "/window/window_id"}
                },
                {
                    "id": "capture",
                    "method": "screenshot",
                    "params": {"window_id": {"$ref": "target"}},
                    "when": {"reference": "target", "equals": 42}
                }
            ]
        }))
        .unwrap();
        let methods = ["inspect".to_owned(), "screenshot".to_owned()].into_iter().collect();
        validate_plan(&plan, &methods).unwrap();

        let aliases = BTreeMap::from([(String::from("target"), json!(42))]);
        assert_eq!(
            resolve_plan_references(&plan.steps[1].params, &aliases).unwrap(),
            json!({"window_id": 42})
        );

        let no_params: IpcAutomationPlan = serde_json::from_value(json!({
            "version": 1,
            "steps": [{"id": "windows", "method": "list_windows"}]
        }))
        .unwrap();
        let methods = ["list_windows".to_owned()].into_iter().collect();
        validate_plan(&no_params, &methods).unwrap();
        assert_eq!(no_params.steps[0].params, json!({}));
    }

    #[test]
    fn plan_rejects_forward_alias_references_and_unbounded_steps() {
        let forward: IpcAutomationPlan = serde_json::from_value(json!({
            "version": 1,
            "steps": [{
                "id": "capture",
                "method": "screenshot",
                "params": {"window_id": {"$ref": "later"}},
                "bind": {"later": "/window_id"}
            }]
        }))
        .unwrap();
        let methods = ["screenshot".to_owned()].into_iter().collect();
        assert!(validate_plan(&forward, &methods).is_err());

        let step = forward.steps[0].clone();
        let oversized =
            IpcAutomationPlan { version: 1, steps: vec![step; MAX_AUTOMATION_PLAN_STEPS + 1] };
        assert!(validate_plan(&oversized, &methods).is_err());
    }

    #[test]
    fn handshake_classifies_standard_and_host_methods() {
        let descriptors = [MethodCapability::host("vivida_layout", MethodClass::Observe, false)];
        publish_host_methods([String::from("vivida_layout")].iter());
        publish_host_method_capabilities(&descriptors);
        let capabilities = advertised_method_capabilities();
        assert!(capabilities.iter().any(|capability| {
            capability.name == "mouse"
                && capability.class == MethodClass::Input
                && capability.mutating
                && !capability.host_claimed
        }));
        assert!(capabilities.iter().any(|capability| capability == &descriptors[0]));
        publish_host_methods([].iter());
        publish_host_method_capabilities(&[]);
    }

    #[test]
    fn protocol_envelopes_round_trip() {
        let request = RequestEnvelope {
            version: 1,
            id: 17,
            method: String::from("inspect"),
            params: json!({"window_id": 42}),
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded: RequestEnvelope = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.id, 17);
        assert_eq!(decoded.method, "inspect");
    }

    #[test]
    fn synchronous_response_reader_skips_interleaved_events() {
        let frames = concat!(
            "{\"version\":2,\"subscription_id\":3,\"event_sequence\":8,\"event\":{}}\n",
            "{\"version\":2,\"id\":17,\"ok\":true,\"result\":{\"pong\":true}}\n"
        );
        let mut reader = BufReader::new(frames.as_bytes());
        assert_eq!(read_client_response(&mut reader, 17).unwrap(), json!({"pong": true}));
    }

    #[test]
    fn vivid_trace_around_is_structured_and_bounded() {
        let params = crate::cli::IpcVividTrace {
            target: crate::cli::IpcTarget::default(),
            after: None,
            tail: false,
            before: None,
            around: Some(420),
            preceding: 64,
            following: 16,
            limit: 128,
            timeout: 30_000,
            follow: false,
            session_id: None,
            context_id: None,
            surface_id: None,
            track_id: None,
            category: None,
            recovery_only: false,
        };
        let value = vivid_trace_params(&params).unwrap();
        assert_eq!(value["around"], json!({"sequence": 420, "preceding": 64, "following": 16}));
        assert!(value.get("preceding").is_none());
        assert!(value.get("following").is_none());

        let mut invalid = params.clone();
        invalid.preceding = 0;
        invalid.following = 0;
        assert!(
            validate_message(&SocketMessage::Vivid { command: IpcVividCommand::Trace(invalid) })
                .is_err()
        );
    }

    #[test]
    fn legacy_raw_enum_is_not_a_request_envelope() {
        let legacy = br#"{"GetText":{"rows":1}}"#;
        assert_eq!(decode_request(legacy).unwrap_err().code, "invalid_request");
    }

    #[test]
    fn duplicate_request_id_keeps_original_request_active() {
        let (connection, output) = test_connection();
        assert!(insert_request_id(&connection, 17));
        assert!(!insert_request_id(&connection, 17));
        assert!(connection.inner.in_flight.lock().unwrap().contains(&17));

        let frame = output.recv().unwrap();
        let response: ResponseEnvelope = serde_json::from_slice(&frame.bytes).unwrap();
        assert_eq!(response.id, 17);
        assert_eq!(response.error.unwrap().code, "duplicate_request_id");

        connection.reply(17, json!({"done": true}));
        assert!(!connection.inner.in_flight.lock().unwrap().contains(&17));
    }

    #[test]
    fn in_flight_request_limit_is_enforced() {
        let (connection, output) = test_connection();
        for id in 0..MAX_IN_FLIGHT_REQUESTS as u64 {
            assert!(insert_request_id(&connection, id));
        }
        assert!(!insert_request_id(&connection, 1_000));
        let frame = output.recv().unwrap();
        let response: ResponseEnvelope = serde_json::from_slice(&frame.bytes).unwrap();
        assert_eq!(response.error.unwrap().code, "limit_exceeded");
        assert_eq!(connection.inner.in_flight.lock().unwrap().len(), MAX_IN_FLIGHT_REQUESTS);
    }

    #[test]
    fn subscription_queue_is_bounded_per_subscriber() {
        let (connection, output) = test_connection();
        let queued = Arc::new(AtomicUsize::new(0));
        for sequence in 1..=MAX_SUBSCRIBER_EVENTS as u64 {
            connection
                .event(
                    SubscriptionEventEnvelope {
                        version: 1,
                        subscription_id: 7,
                        event_sequence: sequence,
                        window_id: Some(42),
                        event: json!({"type": "bell", "data": {}}),
                    },
                    &queued,
                )
                .unwrap();
        }
        assert_eq!(queued.load(Ordering::Acquire), MAX_SUBSCRIBER_EVENTS);
        let error = connection
            .event(
                SubscriptionEventEnvelope {
                    version: 1,
                    subscription_id: 7,
                    event_sequence: 999,
                    window_id: Some(42),
                    event: json!({"type": "bell", "data": {}}),
                },
                &queued,
            )
            .unwrap_err();
        assert_eq!(error.code, "subscription_overflow");

        drop(output.recv().unwrap());
        assert_eq!(queued.load(Ordering::Acquire), MAX_SUBSCRIBER_EVENTS - 1);
    }

    #[test]
    fn partial_frame_is_read_until_newline() {
        let (mut client, server) = LocalStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            client.write_all(br#"{"version":1,"id":1,"method":"hello","params":{}"#).unwrap();
            client.write_all(b"}\n").unwrap();
        });
        let (tx, _rx) = mpsc::sync_channel(4);
        let connection = IpcConnection {
            inner: Arc::new(ConnectionInner {
                id: 1,
                output: tx,
                in_flight: Mutex::new(HashSet::new()),
                alive: AtomicBool::new(true),
                shutdown: Mutex::new(None),
            }),
        };
        let frame = read_request_frame(&mut BufReader::new(server), &connection).unwrap();
        assert_eq!(decode_request(&frame).unwrap().method, "hello");
        writer.join().unwrap();
    }

    #[test]
    fn rejects_oversized_frame() {
        let (mut client, server) = LocalStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            client.write_all(&vec![b'x'; MAX_REQUEST_FRAME_BYTES + 1]).unwrap();
        });
        let (tx, _rx) = mpsc::sync_channel(4);
        let connection = IpcConnection {
            inner: Arc::new(ConnectionInner {
                id: 1,
                output: tx,
                in_flight: Mutex::new(HashSet::new()),
                alive: AtomicBool::new(true),
                shutdown: Mutex::new(None),
            }),
        };
        assert!(read_request_frame(&mut BufReader::new(server), &connection).is_none());
        writer.join().unwrap();
    }

    /// The IPC surface injects input and reads screen content, so its socket is owner-only —
    /// stated outright rather than inherited from whatever umask the shell happened to have.
    #[test]
    #[cfg(unix)]
    fn socket_is_owner_only_regardless_of_umask() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vivido.sock");

        // SAFETY: umask has no preconditions. It is process-wide, so it is restored immediately
        // and held for no longer than the bind.
        let previous = unsafe { libc::umask(0) };
        let bound = bind_socket(&path);
        unsafe { libc::umask(previous) };

        let Some(_socket) = bound_socket_or_skip(bound) else {
            return;
        };
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    /// A socket belonging to somebody else is declined before a byte is written to it.
    ///
    /// On macOS the sockets are discovered in a shared `env::temp_dir()`, so a path where Vivido
    /// looks is not proof of who put it there.
    #[test]
    #[cfg(unix)]
    fn a_socket_owned_by_another_user_is_refused_before_connecting() {
        use std::os::unix::fs::MetadataExt;

        let directory = tempfile::tempdir().unwrap();
        let ours = directory.path().join("ours.sock");
        std::fs::File::create(&ours).unwrap();
        require_socket_owner(&ours).expect("a path this user owns is accepted");

        // Faking a peer uid needs privileges this runner may not have; an existing path owned by
        // another user proves the same predicate without them.
        let Some(theirs) =
            ["/etc/hosts", "/bin/sh", "/usr/bin/env"].into_iter().map(Path::new).find(|path| {
                fs::symlink_metadata(path).is_ok_and(|metadata| metadata.uid() != effective_uid())
            })
        else {
            eprintln!("skipping foreign-owner test: this runner owns every candidate path");
            return;
        };
        let error =
            require_socket_owner(theirs).expect_err("a socket owned by another user is refused");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    }

    /// A directory Vivido has to create for its sockets is owner-only whatever the umask says.
    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn the_socket_directory_vivido_creates_is_owner_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime");

        // SAFETY: umask has no preconditions. It is process-wide, so it is restored at once.
        let previous = unsafe { libc::umask(0) };
        let created = private_socket_dir(path.clone());
        unsafe { libc::umask(previous) };

        assert_eq!(created, Some(path.clone()));
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o700);
    }

    /// One that anybody else can write to is declined rather than trusted or rewritten.
    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn a_socket_directory_open_to_others_is_declined() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();

        assert_eq!(private_socket_dir(path.clone()), None);
        assert_eq!(
            path.metadata().unwrap().permissions().mode() & 0o777,
            0o777,
            "a directory Vivido does not own is left exactly as it was found"
        );
    }

    #[test]
    #[cfg(unix)]
    fn accepted_connections_are_restored_to_blocking_mode() {
        let (_client, server) = LocalStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        configure_connection(&server).unwrap();

        let flags = unsafe { libc::fcntl(server.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(flags & libc::O_NONBLOCK, 0);
    }

    #[test]
    fn hello_advertises_required_limits() {
        let hello = hello_result();
        assert_eq!(hello["protocol_version"], 2);
        assert_eq!(hello["limits"]["connections"], 32);
        assert!(hello["methods"].as_array().unwrap().iter().any(|value| value == "get_grid"));
        for method in [
            "vivid_sessions",
            "vivid_surfaces",
            "vivid_surface_status",
            "vivid_tracks",
            "vivid_track_status",
            "vivid_scene_status",
            "wait_vivid_track",
            "set_geometry",
            "set_visible",
            "set_level",
        ] {
            assert!(hello["methods"].as_array().unwrap().iter().any(|value| value == method));
        }
        assert!(hello["event_kinds"].as_array().unwrap().iter().any(|value| value == "moved"));
        for retired in
            ["vivid_sources", "vivid_source_status", "vivid_milestones", "wait_vivid_source"]
        {
            assert!(!hello["methods"].as_array().unwrap().iter().any(|value| value == retired));
        }
    }

    #[test]
    fn hello_advertises_host_claimed_methods_beside_vivido_own() {
        let claimed = [String::from("vvbox_list_tabs"), String::from("create_window")];
        publish_host_methods(claimed.iter());

        let methods = advertised_methods();
        // A host name is added once, and re-claiming a built-in never duplicates it.
        assert_eq!(methods.iter().filter(|method| *method == "vvbox_list_tabs").count(), 1);
        assert_eq!(methods.iter().filter(|method| *method == "create_window").count(), 1);
        // Claiming does not withdraw anything Vivido still answers itself.
        assert!(methods.iter().any(|method| method == "get_grid"));

        publish_host_methods([].iter());
        assert!(!advertised_methods().iter().any(|method| method == "vvbox_list_tabs"));
    }

    /// A connection from this very process is by definition the owner, so it must be accepted.
    #[test]
    #[cfg(unix)]
    fn the_owner_is_accepted_on_both_ends_of_a_socket() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner.sock");
        let Some(listener) = bound_socket_or_skip(LocalListener::bind(&path)) else {
            return;
        };
        let client = std::thread::spawn({
            let path = path.clone();
            move || LocalStream::connect(&path).expect("client accepts server owner")
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let _server = loop {
            match listener.accept() {
                Ok(server) => break server,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "owner connection was not accepted"
                    );
                    std::thread::yield_now();
                },
                Err(error) => panic!("server accepts client owner: {error}"),
            }
        };
        let _client = client.join().unwrap();
    }

    #[cfg(unix)]
    fn bound_socket_or_skip<T>(result: io::Result<T>) -> Option<T> {
        match result {
            Ok(socket) => Some(socket),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("skipping socket test: this runner forbids local socket binds");
                None
            },
            Err(error) => panic!("could not bind test socket: {error}"),
        }
    }

    /// Owner authentication runs on both pipe ends, and a blocked reader must not stall writes.
    #[test]
    #[cfg(windows)]
    fn the_owner_is_accepted_by_a_full_duplex_named_pipe() {
        let path = PathBuf::from(format!(r"\\.\pipe\vivido-owner-test-{}", std::process::id()));
        let listener = LocalListener::bind(&path).expect("bind");
        let client = std::thread::spawn({
            let path = path.clone();
            move || LocalStream::connect(&path).expect("client accepts server owner")
        });
        let mut server = listener.accept().expect("server accepts client owner");
        let mut client = client.join().unwrap();

        let mut server_reader = server.try_clone().unwrap();
        let mut client_reader = client.try_clone().unwrap();
        let server_read = std::thread::spawn(move || {
            let mut bytes = [0; 6];
            server_reader.read_exact(&mut bytes).unwrap();
            bytes
        });
        let client_read = std::thread::spawn(move || {
            let mut bytes = [0; 6];
            client_reader.read_exact(&mut bytes).unwrap();
            bytes
        });
        std::thread::sleep(Duration::from_millis(10));
        server.write_all(b"server").unwrap();
        client.write_all(b"client").unwrap();
        assert_eq!(&server_read.join().unwrap(), b"client");
        assert_eq!(&client_read.join().unwrap(), b"server");
    }

    /// The capability document tells a client whether it reached a windowless instance.
    #[test]
    fn hello_reports_whether_this_instance_is_headless() {
        let hello = hello_result();

        // `INSTANCE` is unset in tests, which must read as "windowed" rather than panic.
        assert_eq!(hello["headless"], serde_json::json!(false));
        assert_eq!(hello["session"], Value::Null);
        assert!(hello["methods"].as_array().unwrap().iter().any(|value| value == "quit"));
    }

    #[test]
    fn extension_method_names_are_validated_before_endpoint_discovery() {
        let error = request_method(None, None, "invalid-method", json!({})).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
