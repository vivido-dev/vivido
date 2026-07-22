//! Versioned, owner-only Unix socket automation protocol.

use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Error as IoError, ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use log::{error, warn};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use winit::event_loop::EventLoopProxy;

use crate::cli::{IpcMouseAction, IpcWaitCondition, MessageOptions, Options, SocketMessage};
use crate::event::{Event, EventType};
use crate::terminal::thread;

/// Formal Vivido automation protocol version.
pub const PROTOCOL_VERSION: u16 = 1;

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

/// Number of serialized frames buffered for one connection.
const OUTPUT_QUEUE_FRAMES: usize = MAX_SUBSCRIBER_EVENTS + MAX_IN_FLIGHT_REQUESTS;

/// Write timeout prevents a dead client from retaining a writer forever.
const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Methods advertised by the protocol handshake.
pub const METHODS: &[&str] = &[
    "hello",
    "ping",
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
    "focus",
    "signal",
    "list_windows",
    "inspect",
    "get_grid",
    "wait_text",
    "wait_output",
    "wait_screen_change",
    "wait_screen_stable",
    "wait_frame",
    "wait_exit",
    "transcript",
    "subscribe",
];

/// Event kinds advertised by the protocol handshake.
pub const EVENT_KINDS: &[&str] = &[
    "screen_changed",
    "output",
    "frame_presented",
    "title_changed",
    "focus_changed",
    "resized",
    "bell",
    "child_exit",
    "window_created",
    "window_closed",
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
    pub version: u16,
    pub subscription_id: u64,
    pub event_sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<u64>,
    pub event: Value,
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
    shutdown: Mutex<Option<UnixStream>>,
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
            let _ = stream.shutdown(std::net::Shutdown::Both);
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

struct OutputFrame {
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
    pub socket: UnixListener,
    event_proxy: EventLoopProxy<Event>,
    connection_count: Arc<AtomicUsize>,
    next_connection_id: AtomicU64,
}

impl IpcListener {
    pub fn new(
        options: &Options,
        event_proxy: EventLoopProxy<Event>,
        path: &Path,
    ) -> Result<Self, IoError> {
        let socket = bind_socket(path)?;
        unsafe { env::set_var(VIVIDO_SOCKET_ENV, path.as_os_str()) };
        if options.daemon {
            println!("VIVIDO_SOCKET={}; export VIVIDO_SOCKET", path.display());
        }

        Ok(Self {
            socket,
            event_proxy,
            connection_count: Arc::new(AtomicUsize::new(0)),
            next_connection_id: AtomicU64::new(1),
        })
    }

    /// Accept and start one persistent full-duplex IPC session.
    pub fn process_message(&mut self) -> Result<(), IoError> {
        let (stream, _) = self.socket.accept()?;
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
    stream: UnixStream,
    connection_id: u64,
    event_proxy: EventLoopProxy<Event>,
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
        let mut writer = writer;
        let _ = writer.set_write_timeout(Some(IPC_WRITE_TIMEOUT));
        while let Ok(frame) = output_rx.recv() {
            if writer.write_all(&frame.bytes).and_then(|()| writer.flush()).is_err() {
                let _ = writer.shutdown(std::net::Shutdown::Both);
                break;
            }
        }
        if let Some(writer_inner) = writer_inner.upgrade() {
            writer_inner.alive.store(false, Ordering::Release);
        }
    });

    thread::spawn_named("IPC reader", move || {
        let _guard = guard;
        let connection = IpcConnection { inner };
        run_connection(stream, connection.clone(), &event_proxy);
        connection.inner.alive.store(false, Ordering::Release);
        let _ = event_proxy.send_event(Event::new(EventType::IpcDisconnect(connection_id), None));
    });
}

fn configure_connection(stream: &UnixStream) -> io::Result<()> {
    stream.set_nonblocking(false)
}

fn run_connection(
    stream: UnixStream,
    connection: IpcConnection,
    event_proxy: &EventLoopProxy<Event>,
) {
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
            IpcError::new("unsupported_version", "Vivido IPC requires protocol version 1")
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
                IpcError::new("unsupported_version", "Vivido IPC requires protocol version 1"),
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
    json!({
        "server_version": env!("CARGO_PKG_VERSION"),
        "protocol_version": PROTOCOL_VERSION,
        "methods": METHODS,
        "event_kinds": EVENT_KINDS,
        "error_codes": [
            "unsupported_version", "invalid_request", "invalid_params",
            "duplicate_request_id", "limit_exceeded", "window_not_found",
            "no_focused_window", "unsupported", "timeout", "sequence_gap", "pty_closed",
            "resize_mismatch", "focus_denied", "regex_invalid", "subscription_overflow",
            "invalid_state"
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

fn send_direct_error(mut stream: UnixStream, id: u64, error: IpcError) {
    let response = ResponseEnvelope::error(id, error);
    if let Ok(mut frame) = serde_json::to_vec(&response) {
        frame.push(b'\n');
        let _ = stream.write_all(&frame);
    }
}

/// Bind and secure the Vivido IPC socket.
fn bind_socket(path: &Path) -> io::Result<UnixListener> {
    let socket = UnixListener::bind(path)?;
    let result = fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .and_then(|()| socket.set_nonblocking(true));
    if let Err(err) = result {
        drop(socket);
        let _ = fs::remove_file(path);
        return Err(err);
    }
    Ok(socket)
}

/// Send one CLI command using a versioned protocol session.
pub fn send_message(options: MessageOptions) -> io::Result<()> {
    validate_message(&options.message)?;
    let mut stream = find_socket(options.socket)?;
    stream.set_nonblocking(false)?;
    let mut reader = BufReader::new(stream.try_clone()?);

    send_client_request(&mut stream, 1, "hello", json!({}))?;
    let hello = read_client_response(&mut reader, 1)?;
    if matches!(options.message, SocketMessage::Capabilities) {
        return write_json(&hello);
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

fn send_client_request(
    stream: &mut UnixStream,
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
        let response: ResponseEnvelope = serde_json::from_slice(&frame)
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
        SocketMessage::Config(params) => Ok(("config", serialize_params(params)?)),
        SocketMessage::GetConfig(params) => Ok(("get_config", serialize_params(params)?)),
        SocketMessage::Typing(params) => Ok(("typing", serialize_params(params)?)),
        SocketMessage::GetText(params) => Ok(("get_text", serialize_params(params)?)),
        SocketMessage::Screenshot(params) => Ok(("screenshot", serialize_params(params)?)),
        SocketMessage::Capabilities => unreachable!(),
        SocketMessage::Key(params) => Ok(("key", serialize_params(params)?)),
        SocketMessage::Paste(params) => Ok(("paste", serialize_params(params)?)),
        SocketMessage::Mouse(params) => Ok(("mouse", serialize_params(params)?)),
        SocketMessage::Resize(params) => Ok(("resize", serialize_params(params)?)),
        SocketMessage::Focus(params) => Ok(("focus", serialize_params(params)?)),
        SocketMessage::Signal(params) => Ok(("signal", serialize_params(params)?)),
        SocketMessage::ListWindows => Ok(("list_windows", json!({}))),
        SocketMessage::Inspect(params) => Ok(("inspect", serialize_params(params)?)),
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
            IpcWaitCondition::Exit(params) => Ok(("wait_exit", serialize_params(params)?)),
        },
    }
}

fn serialize_params<T: Serialize>(params: &T) -> io::Result<Value> {
    serde_json::to_value(params).map_err(IoError::other)
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
    if let SocketMessage::Mouse(params) = message {
        let position = match &params.action {
            IpcMouseAction::Move(position) => position,
            IpcMouseAction::Click(action)
            | IpcMouseAction::DoubleClick(action)
            | IpcMouseAction::Down(action)
            | IpcMouseAction::Up(action)
            | IpcMouseAction::Drag(action) => &action.position,
            IpcMouseAction::Scroll(action) => &action.position,
        };
        let cell = position.cell_column.is_some() && position.cell_row.is_some();
        let pixel = position.x.is_some() && position.y.is_some();
        if cell == pixel {
            return Err(IoError::new(
                ErrorKind::InvalidInput,
                "mouse requires exactly one complete cell or pixel coordinate pair",
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
        | SocketMessage::ListWindows
        | SocketMessage::Inspect(_)
        | SocketMessage::GetGrid(_)
        | SocketMessage::Wait(_)
        | SocketMessage::Transcript(_)
        | SocketMessage::Subscribe(_) => write_json_to(&mut stdout, result),
        SocketMessage::Config(_)
        | SocketMessage::Typing(_)
        | SocketMessage::Key(_)
        | SocketMessage::Paste(_)
        | SocketMessage::Mouse(_)
        | SocketMessage::Resize(_)
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
#[cfg(not(target_os = "macos"))]
pub fn socket_dir() -> PathBuf {
    xdg::BaseDirectories::with_prefix("vivido")
        .get_runtime_directory()
        .map(ToOwned::to_owned)
        .ok()
        .and_then(|path| fs::create_dir_all(&path).map(|_| path).ok())
        .unwrap_or_else(env::temp_dir)
}

/// Directory for the IPC socket file.
#[cfg(target_os = "macos")]
pub fn socket_dir() -> PathBuf {
    env::temp_dir()
}

/// Find a socket using an override, inherited endpoint, or current display discovery.
fn find_socket(socket_path: Option<PathBuf>) -> io::Result<UnixStream> {
    if let Some(socket_path) = socket_path {
        return UnixStream::connect(&socket_path).map_err(|err| {
            IoError::new(err.kind(), format!("invalid socket path {socket_path:?}"))
        });
    }

    if let Ok(path) = env::var(VIVIDO_SOCKET_ENV)
        && let Ok(socket) = UnixStream::connect(path)
    {
        return Ok(socket);
    }

    let mut candidates = Vec::new();
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
    candidates.sort();
    candidates.reverse();
    for path in candidates {
        match UnixStream::connect(&path) {
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
#[cfg(not(target_os = "macos"))]
pub fn socket_prefix() -> String {
    let display = env::var("WAYLAND_DISPLAY").or_else(|_| env::var("DISPLAY")).unwrap_or_default();
    format!("Vivido-{}", display.replace('/', "-"))
}

/// File prefix matching sockets on macOS.
#[cfg(target_os = "macos")]
pub fn socket_prefix() -> String {
    String::from("Vivido")
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;

    use serde_json::json;

    use super::*;

    fn test_connection() -> (IpcConnection, mpsc::Receiver<OutputFrame>) {
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
        let (mut client, server) = UnixStream::pair().unwrap();
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
        let (mut client, server) = UnixStream::pair().unwrap();
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

    #[test]
    fn socket_is_owner_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vivido.sock");
        let _socket = bind_socket(&path).unwrap();
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn accepted_connections_are_restored_to_blocking_mode() {
        let (_client, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        configure_connection(&server).unwrap();

        let flags = unsafe { libc::fcntl(server.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(flags & libc::O_NONBLOCK, 0);
    }

    #[test]
    fn hello_advertises_required_limits() {
        let hello = hello_result();
        assert_eq!(hello["protocol_version"], 1);
        assert_eq!(hello["limits"]["connections"], 32);
        assert!(hello["methods"].as_array().unwrap().iter().any(|value| value == "get_grid"));
    }
}
