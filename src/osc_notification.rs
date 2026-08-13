//! Bounded OSC 9/99 parsing and desktop-notification lifecycle management.

use std::collections::{HashMap, VecDeque};
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD as BASE64_NO_PAD};

use crate::event::{EventProxy, EventType};
use crate::terminal::event::Notify;
use crate::terminal::vte::{self, Perform};

const MAX_OSC_BYTES: usize = 8 * 1024;
const MAX_PLAIN_PAYLOAD_BYTES: usize = 2 * 1024;
const MAX_ENCODED_PAYLOAD_BYTES: usize = 4 * 1024;
const MAX_ASSEMBLED_BYTES: usize = 16 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_PENDING: usize = 64;
const WORKER_QUEUE_CAPACITY: usize = 64;
const TASK_QUEUE_CAPACITY: usize = 4;
const ASSEMBLY_TIMEOUT: Duration = Duration::from_secs(30);
const RATE_BURST: f64 = 5.;
const RATE_REFILL_PER_SECOND: f64 = 1.;

/// One complete, bounded notification escape observed in the PTY stream.
#[derive(Clone, Debug)]
pub enum OscNotification {
    Legacy(String),
    Kitty(KittyNotification),
}

#[derive(Clone, Debug)]
pub struct KittyNotification {
    id: Option<String>,
    payload: Payload,
    done: bool,
    focus: Option<bool>,
    occasion: Option<Occasion>,
    urgency: Option<Urgency>,
    expiry: Option<Expiry>,
    sound: Option<Sound>,
}

#[derive(Clone, Debug)]
enum Payload {
    Title(String),
    Body(String),
    Close,
    Query,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Default)]
enum Occasion {
    #[default]
    Always,
    Unfocused,
    Invisible,
}

#[derive(Clone, Copy, Debug, Default)]
enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

#[derive(Clone, Copy, Debug, Default)]
enum Expiry {
    #[default]
    Default,
    Never,
    Milliseconds(u32),
}

#[derive(Clone, Copy, Debug, Default)]
enum Sound {
    #[default]
    System,
    Silent,
}

/// A second VTE parser confirms that a captured `ESC ]` sequence is a real top-level OSC.
///
/// The raw capture is necessary because VTE intentionally exposes at most sixteen semicolon
/// parameters, while notification bodies are arbitrary plain text and may contain more.
#[derive(Default)]
pub(crate) struct OscNotificationParser {
    parser: vte::Parser<8193>,
    capture: OscCapture,
    suppress_until_terminator: bool,
    pending_dispatch: bool,
}

impl OscNotificationParser {
    pub(crate) fn advance(&mut self, bytes: &[u8]) -> Vec<OscNotification> {
        let mut notifications = Vec::new();

        for &byte in bytes {
            let observation = self.capture.advance(byte);
            match observation {
                CaptureObservation::Overflow => {
                    // Reset before VTE's fixed ArrayVec can fill and panic. Ignore everything until
                    // the actual string terminator so an overlong suffix cannot become a new OSC.
                    self.parser = vte::Parser::default();
                    self.suppress_until_terminator = true;
                    self.pending_dispatch = false;
                    continue;
                },
                CaptureObservation::Complete(None) if self.suppress_until_terminator => {
                    self.parser = vte::Parser::default();
                    self.suppress_until_terminator = false;
                    self.pending_dispatch = false;
                    continue;
                },
                _ if self.suppress_until_terminator => continue,
                _ => (),
            }

            let mut performer = OscDispatch::default();
            self.parser.advance(&mut performer, std::slice::from_ref(&byte));
            self.pending_dispatch |= performer.dispatched;

            if matches!(&observation, CaptureObservation::Discard) {
                self.pending_dispatch = false;
            }

            if let CaptureObservation::Complete(raw) = observation {
                if self.pending_dispatch
                    && let Some(raw) = raw
                    && let Some(notification) = parse_osc(&raw)
                {
                    notifications.push(notification);
                }
                self.pending_dispatch = false;
            }
        }

        notifications
    }
}

#[derive(Default)]
struct OscDispatch {
    dispatched: bool,
}

impl Perform for OscDispatch {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.dispatched = params.first().is_some_and(|param| matches!(*param, b"9" | b"99"));
    }
}

#[derive(Default)]
struct OscCapture {
    state: CaptureState,
}

#[derive(Default)]
enum CaptureState {
    #[default]
    Ground,
    Escape,
    OtherString {
        escape: bool,
    },
    Osc {
        raw: Vec<u8>,
        escape: bool,
        overflow: bool,
    },
}

enum CaptureObservation {
    None,
    Discard,
    Overflow,
    Complete(Option<Vec<u8>>),
}

impl OscCapture {
    fn advance(&mut self, byte: u8) -> CaptureObservation {
        match &mut self.state {
            CaptureState::Ground => {
                if byte == b'\x1b' {
                    self.state = CaptureState::Escape;
                }
            },
            CaptureState::Escape => match byte {
                b']' => {
                    self.state = CaptureState::Osc {
                        raw: Vec::with_capacity(128),
                        escape: false,
                        overflow: false,
                    };
                },
                b'P' | b'_' | b'^' | b'X' => {
                    self.state = CaptureState::OtherString { escape: false };
                },
                b'\x1b' => (),
                _ => self.state = CaptureState::Ground,
            },
            CaptureState::OtherString { escape } => {
                if *escape && byte == b'\\' {
                    self.state = CaptureState::Ground;
                    return CaptureObservation::Discard;
                } else {
                    *escape = byte == b'\x1b';
                }
            },
            CaptureState::Osc { raw, escape, overflow } => {
                let complete = byte == b'\x07' || (*escape && byte == b'\\');
                if complete {
                    let raw = (!*overflow).then(|| std::mem::take(raw));
                    self.state = CaptureState::Ground;
                    return CaptureObservation::Complete(raw);
                }

                if *overflow {
                    *escape = byte == b'\x1b';
                    return CaptureObservation::None;
                }

                if *escape {
                    raw.push(b'\x1b');
                }
                *escape = byte == b'\x1b';
                if !*escape {
                    raw.push(byte);
                }

                if raw.len() > MAX_OSC_BYTES {
                    raw.clear();
                    *overflow = true;
                    return CaptureObservation::Overflow;
                }
            },
        }

        CaptureObservation::None
    }
}

fn parse_osc(raw: &[u8]) -> Option<OscNotification> {
    if let Some(message) = raw.strip_prefix(b"9;") {
        return parse_legacy(message).map(OscNotification::Legacy);
    }

    let rest = raw.strip_prefix(b"99;")?;
    let separator = rest.iter().position(|&byte| byte == b';')?;
    parse_kitty(&rest[..separator], &rest[separator + 1..]).map(OscNotification::Kitty)
}

fn parse_legacy(message: &[u8]) -> Option<String> {
    if message.len() > MAX_PLAIN_PAYLOAD_BYTES || !is_escape_safe(message) {
        return None;
    }

    // OSC 9 has incompatible numeric subfamilies, including OSC 9;4 progress reports.
    if message.iter().position(|&byte| byte == b';').is_some_and(|separator| {
        let selector = &message[..separator];
        !selector.is_empty() && selector.iter().all(u8::is_ascii_digit)
    }) {
        return None;
    }

    String::from_utf8(message.to_vec()).ok()
}

fn parse_kitty(metadata: &[u8], payload: &[u8]) -> Option<KittyNotification> {
    let mut id = None;
    let mut payload_type = b"title".as_slice();
    let mut done = true;
    let mut encoded = false;
    let mut focus = None;
    let mut occasion = None;
    let mut urgency = None;
    let mut expiry = None;
    let mut sound = None;

    for field in metadata.split(|&byte| byte == b':').filter(|field| !field.is_empty()) {
        let separator = field.iter().position(|&byte| byte == b'=')?;
        let (key, value) = (&field[..separator], &field[separator + 1..]);
        if key.len() != 1 || !key[0].is_ascii_alphabetic() || !valid_metadata_value(value) {
            return None;
        }

        match key[0] {
            b'i' => id = Some(parse_identifier(value)?),
            b'p' => payload_type = value,
            b'd' => done = parse_bool(value)?,
            b'e' => encoded = parse_bool(value)?,
            b'a' => {
                let mut requested_focus = true;
                for action in value.split(|&byte| byte == b',') {
                    match action {
                        b"focus" => requested_focus = true,
                        b"-focus" => requested_focus = false,
                        _ => (),
                    }
                }
                focus = Some(requested_focus);
            },
            b'o' => {
                occasion = Some(match value {
                    b"always" => Occasion::Always,
                    b"unfocused" => Occasion::Unfocused,
                    b"invisible" => Occasion::Invisible,
                    _ => return None,
                });
            },
            b'u' => {
                urgency = Some(match value {
                    b"0" => Urgency::Low,
                    b"1" => Urgency::Normal,
                    b"2" => Urgency::Critical,
                    _ => return None,
                });
            },
            b'w' => {
                let value = std::str::from_utf8(value).ok()?.parse::<i64>().ok()?;
                expiry = Some(if value == -1 {
                    Expiry::Default
                } else if value == 0 {
                    Expiry::Never
                } else if (1..=i32::MAX as i64).contains(&value) {
                    Expiry::Milliseconds(value as u32)
                } else {
                    return None;
                });
            },
            b's' => {
                let decoded = decode_base64(value)?;
                sound = Some(match decoded.as_slice() {
                    b"system" => Sound::System,
                    b"silent" => Sound::Silent,
                    _ => Sound::System,
                });
            },
            // Application/type/icon metadata and PTY callbacks are intentionally unsupported.
            _ => (),
        }
    }

    let payload = match payload_type {
        b"close" => Payload::Close,
        b"?" => Payload::Query,
        b"title" | b"body" => {
            let text = decode_payload(payload, encoded)?;
            if payload_type == b"title" { Payload::Title(text) } else { Payload::Body(text) }
        },
        _ => Payload::Unsupported,
    };

    Some(KittyNotification { id, payload, done, focus, occasion, urgency, expiry, sound })
}

fn parse_bool(value: &[u8]) -> Option<bool> {
    match value {
        b"0" => Some(false),
        b"1" => Some(true),
        _ => None,
    }
}

fn parse_identifier(value: &[u8]) -> Option<String> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value.iter().all(|byte| byte.is_ascii_alphanumeric() || b"_-+.".contains(byte))
    {
        return None;
    }
    String::from_utf8(value.to_vec()).ok()
}

fn valid_metadata_value(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-_/+.,(){}[]*&^%$#@!`~?=".contains(byte))
}

fn decode_payload(payload: &[u8], encoded: bool) -> Option<String> {
    let decoded = if encoded {
        if payload.len() > MAX_ENCODED_PAYLOAD_BYTES {
            return None;
        }
        decode_base64(payload)?
    } else {
        if payload.len() > MAX_PLAIN_PAYLOAD_BYTES || !is_escape_safe(payload) {
            return None;
        }
        payload.to_vec()
    };

    if decoded.len() > MAX_PLAIN_PAYLOAD_BYTES {
        return None;
    }
    let text = String::from_utf8(decoded).ok()?;
    (!text.chars().any(is_control)).then_some(text)
}

fn decode_base64(encoded: &[u8]) -> Option<Vec<u8>> {
    BASE64.decode(encoded).or_else(|_| BASE64_NO_PAD.decode(encoded)).ok()
}

fn is_escape_safe(value: &[u8]) -> bool {
    std::str::from_utf8(value).is_ok_and(|text| !text.chars().any(is_control))
}

fn is_control(character: char) -> bool {
    matches!(character as u32, 0x00..=0x1f | 0x7f..=0x9f)
}

#[derive(Clone, Debug)]
struct Assembly {
    title: String,
    body: String,
    focus: bool,
    occasion: Occasion,
    urgency: Urgency,
    expiry: Expiry,
    sound: Sound,
    updated_at: Option<Instant>,
}

impl Default for Assembly {
    fn default() -> Self {
        Self {
            title: String::new(),
            body: String::new(),
            focus: true,
            occasion: Occasion::default(),
            urgency: Urgency::default(),
            expiry: Expiry::default(),
            sound: Sound::default(),
            updated_at: None,
        }
    }
}

impl Assembly {
    fn append(&mut self, payload: Payload) -> bool {
        let (is_title, text) = match payload {
            Payload::Title(text) => (true, text),
            Payload::Body(text) => (false, text),
            _ => return false,
        };
        if self.title.len() + self.body.len() + text.len() > MAX_ASSEMBLED_BYTES {
            return false;
        }
        if is_title {
            self.title.push_str(&text);
        } else {
            self.body.push_str(&text);
        }
        true
    }

    fn apply_options(&mut self, request: &KittyNotification) {
        if let Some(focus) = request.focus {
            self.focus = focus;
        }
        if let Some(occasion) = request.occasion {
            self.occasion = occasion;
        }
        if let Some(urgency) = request.urgency {
            self.urgency = urgency;
        }
        if let Some(expiry) = request.expiry {
            self.expiry = expiry;
        }
        if let Some(sound) = request.sound {
            self.sound = sound;
        }
        self.updated_at = Some(Instant::now());
    }

    fn display(mut self, id: Option<String>) -> Option<DesktopNotification> {
        if self.title.is_empty() {
            self.title = std::mem::take(&mut self.body);
        }
        if self.title.is_empty() {
            return None;
        }
        Some(DesktopNotification {
            id,
            title: self.title,
            body: self.body,
            focus: self.focus,
            urgency: self.urgency,
            expiry: self.expiry,
            sound: self.sound,
        })
    }
}

#[derive(Clone, Debug)]
struct DesktopNotification {
    id: Option<String>,
    title: String,
    body: String,
    focus: bool,
    urgency: Urgency,
    expiry: Expiry,
    sound: Sound,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowNotificationState {
    pub focused: bool,
    pub visible: bool,
    pub occluded: bool,
    pub headless: bool,
}

pub(crate) struct NotificationController {
    enabled: bool,
    can_focus: bool,
    pending: HashMap<String, Assembly>,
    worker: Box<dyn DesktopBackend>,
    limiter: RateLimiter,
}

impl NotificationController {
    pub(crate) fn new(enabled: bool, can_focus: bool, event_proxy: EventProxy) -> Self {
        Self {
            enabled,
            can_focus,
            pending: HashMap::new(),
            worker: Box::new(NotificationWorker::new(event_proxy)),
            limiter: RateLimiter::default(),
        }
    }

    #[cfg(test)]
    fn with_backend(enabled: bool, can_focus: bool, worker: Box<dyn DesktopBackend>) -> Self {
        Self {
            enabled,
            can_focus,
            pending: HashMap::new(),
            worker,
            limiter: RateLimiter::default(),
        }
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled && !enabled {
            self.pending.clear();
            self.worker.clear();
        }
        self.enabled = enabled;
    }

    pub(crate) fn handle<N: Notify>(
        &mut self,
        notification: OscNotification,
        state: WindowNotificationState,
        notifier: &N,
    ) {
        if !self.enabled {
            return;
        }
        self.expire_pending();

        match notification {
            OscNotification::Legacy(title) => {
                let assembly = Assembly { title, ..Default::default() };
                self.submit(assembly, None, state);
            },
            OscNotification::Kitty(request) => self.handle_kitty(request, state, notifier),
        }
    }

    fn handle_kitty<N: Notify>(
        &mut self,
        request: KittyNotification,
        state: WindowNotificationState,
        notifier: &N,
    ) {
        match &request.payload {
            Payload::Query => {
                notifier.notify(self.query_response(request.id.as_deref()));
                return;
            },
            Payload::Close => {
                if let Some(id) = request.id {
                    self.pending.remove(&id);
                    self.worker.close(id);
                }
                return;
            },
            Payload::Unsupported => return,
            Payload::Title(_) | Payload::Body(_) => (),
        }

        let Some(id) = request.id.clone() else {
            if !request.done {
                return;
            }
            let mut assembly = Assembly::default();
            assembly.apply_options(&request);
            if assembly.append(request.payload) {
                self.submit(assembly, None, state);
            }
            return;
        };

        if !self.pending.contains_key(&id)
            && self.pending.len() >= MAX_PENDING
            && let Some(oldest) = self
                .pending
                .iter()
                .min_by_key(|(_, value)| value.updated_at)
                .map(|(id, _)| id.clone())
        {
            self.pending.remove(&oldest);
        }

        let assembly = self.pending.entry(id.clone()).or_default();
        assembly.apply_options(&request);
        if !assembly.append(request.payload) {
            self.pending.remove(&id);
            return;
        }

        if request.done {
            let assembly = self.pending.remove(&id).unwrap();
            self.submit(assembly, Some(id), state);
        }
    }

    fn submit(&mut self, assembly: Assembly, id: Option<String>, state: WindowNotificationState) {
        if !occasion_matches(assembly.occasion, state) || !self.limiter.take() {
            return;
        }
        if let Some(mut notification) = assembly.display(id) {
            notification.focus &= self.can_focus;
            self.worker.display(notification);
        }
    }

    fn query_response(&self, id: Option<&str>) -> Vec<u8> {
        let capabilities = self.worker.capabilities();
        let mut fields = vec![
            if self.can_focus { "a=focus" } else { "" },
            "o=always,unfocused,invisible",
            if capabilities.update_close { "p=title,body,close" } else { "p=title,body" },
            if capabilities.sound { "s=system,silent" } else { "" },
            if capabilities.urgency { "u=0,1,2" } else { "" },
            if capabilities.expiry { "w=1" } else { "" },
        ];
        fields.retain(|field| !field.is_empty());
        format!("\x1b]99;i={}:p=?;{}\x1b\\", id.unwrap_or("0"), fields.join(":")).into_bytes()
    }

    fn expire_pending(&mut self) {
        let now = Instant::now();
        self.pending.retain(|_, assembly| {
            assembly
                .updated_at
                .is_some_and(|updated| now.duration_since(updated) < ASSEMBLY_TIMEOUT)
        });
    }
}

fn occasion_matches(occasion: Occasion, state: WindowNotificationState) -> bool {
    match occasion {
        Occasion::Always => true,
        Occasion::Unfocused => !state.focused,
        Occasion::Invisible => {
            !state.focused && (state.headless || !state.visible || state.occluded)
        },
    }
}

struct RateLimiter {
    tokens: f64,
    updated_at: Instant,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self { tokens: RATE_BURST, updated_at: Instant::now() }
    }
}

impl RateLimiter {
    fn take(&mut self) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens
            + now.duration_since(self.updated_at).as_secs_f64() * RATE_REFILL_PER_SECOND)
            .min(RATE_BURST);
        self.updated_at = now;
        if self.tokens < 1. {
            return false;
        }
        self.tokens -= 1.;
        true
    }
}

#[derive(Clone, Copy)]
struct BackendCapabilities {
    update_close: bool,
    urgency: bool,
    expiry: bool,
    sound: bool,
}

trait DesktopBackend {
    fn capabilities(&self) -> BackendCapabilities;
    fn display(&self, notification: DesktopNotification);
    fn close(&self, id: String);
    fn clear(&self);
}

struct NotificationWorker {
    sender: SyncSender<WorkerMessage>,
    thread: Option<JoinHandle<()>>,
}

impl NotificationWorker {
    fn new(event_proxy: EventProxy) -> Self {
        let (sender, receiver) = mpsc::sync_channel(WORKER_QUEUE_CAPACITY);
        let task_sender = sender.clone();
        let thread = std::thread::Builder::new()
            .name("desktop notifications".into())
            .spawn(move || worker_loop(receiver, task_sender, event_proxy))
            .ok();
        Self { sender, thread }
    }
}

impl DesktopBackend for NotificationWorker {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            update_close: cfg!(all(unix, not(target_os = "macos"))),
            urgency: true,
            expiry: cfg!(unix),
            sound: cfg!(unix),
        }
    }

    fn display(&self, notification: DesktopNotification) {
        let _ = self.sender.try_send(WorkerMessage::Display(notification));
    }

    fn close(&self, id: String) {
        let _ = self.sender.try_send(WorkerMessage::Close(id));
    }

    fn clear(&self) {
        let _ = self.sender.try_send(WorkerMessage::Clear);
    }
}

impl Drop for NotificationWorker {
    fn drop(&mut self) {
        match self.sender.try_send(WorkerMessage::Shutdown) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => (),
            Err(TrySendError::Full(_)) => {
                // The receiver is still live and bounded; a blocking send is safe during teardown.
                let _ = self.sender.send(WorkerMessage::Shutdown);
            },
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum WorkerMessage {
    Display(DesktopNotification),
    Close(String),
    Clear,
    Done(u64),
    Shutdown,
}

enum TaskCommand {
    #[cfg(all(unix, not(target_os = "macos")))]
    Update(DesktopNotification),
    #[cfg(all(unix, not(target_os = "macos")))]
    Close,
}

struct LiveTask {
    id: Option<String>,
    #[cfg(all(unix, not(target_os = "macos")))]
    sender: async_channel::Sender<TaskCommand>,
}

fn worker_loop(
    receiver: mpsc::Receiver<WorkerMessage>,
    sender: SyncSender<WorkerMessage>,
    event_proxy: EventProxy,
) {
    let mut live = HashMap::<u64, LiveTask>::new();
    let mut by_id = HashMap::<String, u64>::new();
    let mut order = VecDeque::new();
    let mut next_token = 0u64;

    while let Ok(message) = receiver.recv() {
        match message {
            WorkerMessage::Display(notification) => {
                if let Some(token) = notification.id.as_ref().and_then(|id| by_id.get(id)).copied()
                    && let Some(task) = live.get(&token)
                    && try_update_task(task, notification.clone())
                {
                    continue;
                }

                while live.len() >= MAX_PENDING {
                    let Some(token) = order.pop_front() else { break };
                    remove_task(token, &mut live, &mut by_id, true);
                }

                next_token = next_token.wrapping_add(1);
                let token = next_token;
                let (task_sender, task_receiver) = async_channel::bounded(TASK_QUEUE_CAPACITY);
                #[cfg(any(target_os = "macos", windows))]
                let _ = task_sender;
                if let Some(id) = &notification.id {
                    by_id.insert(id.clone(), token);
                }
                live.insert(
                    token,
                    LiveTask {
                        id: notification.id.clone(),
                        #[cfg(all(unix, not(target_os = "macos")))]
                        sender: task_sender,
                    },
                );
                order.push_back(token);
                spawn_notification_task(
                    token,
                    notification,
                    task_receiver,
                    sender.clone(),
                    event_proxy.clone(),
                );
            },
            WorkerMessage::Close(id) => {
                if let Some(token) = by_id.remove(&id) {
                    remove_task(token, &mut live, &mut by_id, true);
                }
            },
            WorkerMessage::Clear => {
                for (_, task) in live.drain() {
                    close_task(&task);
                }
                by_id.clear();
                order.clear();
            },
            WorkerMessage::Done(token) => {
                remove_task(token, &mut live, &mut by_id, false);
                order.retain(|candidate| *candidate != token);
            },
            WorkerMessage::Shutdown => {
                for (_, task) in live.drain() {
                    close_task(&task);
                }
                break;
            },
        }
    }
}

fn remove_task(
    token: u64,
    live: &mut HashMap<u64, LiveTask>,
    by_id: &mut HashMap<String, u64>,
    close: bool,
) {
    if let Some(task) = live.remove(&token) {
        if let Some(id) = &task.id {
            by_id.remove(id);
        }
        if close {
            close_task(&task);
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn close_task(task: &LiveTask) {
    let _ = task.sender.send_blocking(TaskCommand::Close);
}

#[cfg(any(target_os = "macos", windows))]
fn close_task(_task: &LiveTask) {}

#[cfg(all(unix, not(target_os = "macos")))]
fn try_update_task(task: &LiveTask, notification: DesktopNotification) -> bool {
    task.sender.try_send(TaskCommand::Update(notification)).is_ok()
}

#[cfg(any(target_os = "macos", windows))]
fn try_update_task(_task: &LiveTask, _notification: DesktopNotification) -> bool {
    false
}

fn spawn_notification_task(
    token: u64,
    notification: DesktopNotification,
    receiver: async_channel::Receiver<TaskCommand>,
    worker: SyncSender<WorkerMessage>,
    event_proxy: EventProxy,
) {
    let _ = std::thread::Builder::new().name("desktop notification".into()).spawn(move || {
        run_notification_task(notification, receiver, event_proxy);
        let _ = worker.try_send(WorkerMessage::Done(token));
    });
}

#[cfg(all(unix, not(target_os = "macos")))]
fn run_notification_task(
    mut notification: DesktopNotification,
    receiver: async_channel::Receiver<TaskCommand>,
    event_proxy: EventProxy,
) {
    use futures_lite::future::{block_on, race};
    use notify_rust::NotificationResponse;

    let Ok(mut handle) = build_native_notification(&notification).show() else { return };

    block_on(async {
        loop {
            enum Outcome {
                Response(bool),
                Command(Option<TaskCommand>),
                Expired,
            }

            let focus = notification.focus;
            let response = async {
                let mut activated = false;
                handle
                    .wait_for_action_async(|response| {
                        activated = matches!(
                            response,
                            NotificationResponse::Default | NotificationResponse::Action(_)
                        );
                    })
                    .await;
                Outcome::Response(activated && focus)
            };
            let command = async { Outcome::Command(receiver.recv().await.ok()) };
            let expiry = async {
                match notification.expiry {
                    Expiry::Milliseconds(milliseconds) => {
                        futures_timer::Delay::new(Duration::from_millis(milliseconds.into())).await;
                    },
                    Expiry::Default | Expiry::Never => std::future::pending::<()>().await,
                }
                Outcome::Expired
            };

            match race(race(response, expiry), command).await {
                Outcome::Response(activate) => {
                    if activate {
                        event_proxy.send_event(EventType::NotificationActivated);
                    }
                    break;
                },
                Outcome::Command(Some(TaskCommand::Update(update))) => {
                    notification = update;
                    let native_id = handle.id();
                    let mut replacement = build_native_notification(&notification);
                    replacement.id(native_id);
                    *handle = replacement;
                    let _ = handle.update();
                },
                Outcome::Command(Some(TaskCommand::Close) | None) => {
                    handle.close();
                    break;
                },
                Outcome::Expired => {
                    handle.close();
                    break;
                },
            }
        }
    });
}

#[cfg(target_os = "macos")]
fn run_notification_task(
    notification: DesktopNotification,
    _receiver: async_channel::Receiver<TaskCommand>,
    event_proxy: EventProxy,
) {
    if !desktop_notifications_authorized() {
        return;
    }

    let handle =
        match futures_lite::future::block_on(build_macos_notification(&notification).send()) {
            Ok(handle) => handle,
            Err(error) => {
                log::warn!("could not display a desktop notification: {error}");
                return;
            },
        };
    let focus = notification.focus;
    match futures_lite::future::block_on(handle.response()) {
        Ok(response) => {
            if focus && response.is_default_action() {
                event_proxy.send_event(EventType::NotificationActivated);
            }
        },
        Err(error) => {
            log::warn!("could not observe a desktop notification response: {error}");
        },
    }
}

#[cfg(windows)]
fn run_notification_task(
    notification: DesktopNotification,
    _receiver: async_channel::Receiver<TaskCommand>,
    event_proxy: EventProxy,
) {
    use notify_rust::NotificationResponse;

    let handle = match build_native_notification(&notification).show() {
        Ok(handle) => handle,
        Err(error) => {
            log::warn!("could not display a desktop notification: {error}");
            return;
        },
    };
    let focus = notification.focus;
    if let Err(error) = handle.wait_for_response(move |response: &NotificationResponse| {
        if focus
            && matches!(response, NotificationResponse::Default | NotificationResponse::Action(_))
        {
            event_proxy.send_event(EventType::NotificationActivated);
        }
    }) {
        log::warn!("could not observe a desktop notification response: {error}");
    }
}

#[cfg(target_os = "macos")]
fn desktop_notifications_authorized() -> bool {
    static AUTHORIZED: OnceLock<bool> = OnceLock::new();

    *AUTHORIZED.get_or_init(|| match notify_rust::request_auth_blocking() {
        Ok(granted) => granted,
        Err(error) => {
            log::warn!("could not request desktop notification permission: {error}");
            false
        },
    })
}

#[cfg(target_os = "macos")]
fn build_macos_notification(data: &DesktopNotification) -> mac_usernotifications::Notification {
    use mac_usernotifications::InterruptionLevel;

    let mut notification = mac_usernotifications::Notification::new()
        .title(native_text(&data.title))
        .message(native_text(&data.body))
        .interruption_level(match data.urgency {
            Urgency::Low => InterruptionLevel::Passive,
            Urgency::Normal => InterruptionLevel::Active,
            Urgency::Critical => InterruptionLevel::TimeSensitive,
        });
    if matches!(data.sound, Sound::System) {
        notification = notification.default_sound();
    }
    if let Expiry::Milliseconds(milliseconds) = data.expiry {
        notification = notification.timeout(Duration::from_millis(milliseconds.into()));
    }
    notification
}

#[cfg(not(target_os = "macos"))]
fn build_native_notification(data: &DesktopNotification) -> notify_rust::Notification {
    let mut notification = notify_rust::Notification::new();
    apply_native_notification(&mut notification, data);
    notification
}

#[cfg(not(target_os = "macos"))]
fn apply_native_notification(
    notification: &mut notify_rust::Notification,
    data: &DesktopNotification,
) {
    notification
        .appname("Vivido")
        .summary(&native_text(&data.title))
        .body(&native_text(&data.body))
        .urgency(match data.urgency {
            Urgency::Low => notify_rust::Urgency::Low,
            Urgency::Normal => notify_rust::Urgency::Normal,
            Urgency::Critical => notify_rust::Urgency::Critical,
        })
        .timeout(match data.expiry {
            Expiry::Default => notify_rust::Timeout::Default,
            Expiry::Never => notify_rust::Timeout::Never,
            Expiry::Milliseconds(milliseconds) => notify_rust::Timeout::Milliseconds(milliseconds),
        });
    apply_default_action(notification);
    apply_sound(notification, data.sound);
}

#[cfg(all(unix, not(target_os = "macos")))]
fn apply_default_action(notification: &mut notify_rust::Notification) {
    notification.action("default", "Open Vivido");
}

#[cfg(windows)]
fn apply_default_action(_notification: &mut notify_rust::Notification) {}

#[cfg(all(unix, not(target_os = "macos")))]
fn apply_sound(notification: &mut notify_rust::Notification, sound: Sound) {
    if matches!(sound, Sound::Silent) {
        notification.hint(notify_rust::Hint::SuppressSound(true));
    }
}

#[cfg(windows)]
fn apply_sound(_notification: &mut notify_rust::Notification, _sound: Sound) {}

#[cfg(all(unix, not(target_os = "macos")))]
fn native_text(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(any(target_os = "macos", windows))]
fn native_text(text: &str) -> String {
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    use super::*;

    fn parse(bytes: &[u8]) -> Vec<OscNotification> {
        OscNotificationParser::default().advance(bytes)
    }

    #[derive(Default)]
    struct FakeState {
        displayed: Vec<DesktopNotification>,
        closed: Vec<String>,
        clears: usize,
    }

    struct FakeBackend {
        state: Arc<Mutex<FakeState>>,
        capabilities: BackendCapabilities,
    }

    impl DesktopBackend for FakeBackend {
        fn capabilities(&self) -> BackendCapabilities {
            self.capabilities
        }

        fn display(&self, notification: DesktopNotification) {
            self.state.lock().unwrap().displayed.push(notification);
        }

        fn close(&self, id: String) {
            self.state.lock().unwrap().closed.push(id);
        }

        fn clear(&self) {
            self.state.lock().unwrap().clears += 1;
        }
    }

    #[derive(Default)]
    struct TestNotifier(RefCell<Vec<u8>>);

    impl Notify for TestNotifier {
        fn notify<B: Into<Cow<'static, [u8]>>>(&self, bytes: B) {
            self.0.borrow_mut().extend_from_slice(bytes.into().as_ref());
        }
    }

    fn controller() -> (NotificationController, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let backend = FakeBackend {
            state: state.clone(),
            capabilities: BackendCapabilities {
                update_close: true,
                urgency: true,
                expiry: true,
                sound: true,
            },
        };
        (NotificationController::with_backend(true, true, Box::new(backend)), state)
    }

    fn visible_window() -> WindowNotificationState {
        WindowNotificationState { focused: false, visible: true, occluded: false, headless: false }
    }

    #[test]
    fn parses_legacy_bel_and_st() {
        assert!(
            matches!(parse(b"\x1b]9;hello\x07").as_slice(), [OscNotification::Legacy(value)] if value == "hello")
        );
        assert!(
            matches!(parse(b"\x1b]9;hello\x1b\\").as_slice(), [OscNotification::Legacy(value)] if value == "hello")
        );
    }

    #[test]
    fn ignores_numeric_osc9_families() {
        assert!(parse(b"\x1b]9;4;1;50\x1b\\").is_empty());
    }

    #[test]
    fn handles_every_byte_boundary() {
        let bytes = b"\x1b]99;i=test:d=0;title\x1b\\\x1b]99;i=test:p=body;body\x1b\\";
        let mut parser = OscNotificationParser::default();
        let notifications = bytes
            .iter()
            .flat_map(|byte| parser.advance(std::slice::from_ref(byte)))
            .collect::<Vec<_>>();
        assert_eq!(notifications.len(), 2);
    }

    #[test]
    fn preserves_semicolons_in_payload() {
        let notifications = parse(b"\x1b]99;;one;two;three\x1b\\");
        assert!(matches!(
            notifications.as_slice(),
            [OscNotification::Kitty(KittyNotification { payload: Payload::Title(value), .. })]
                if value == "one;two;three"
        ));
    }

    #[test]
    fn ignores_osc_inside_dcs() {
        assert!(parse(b"\x1bPignored\x1b]9;nope\x1b\\\x1b\\").is_empty());
    }

    #[test]
    fn rejects_oversized_sequences_and_resynchronizes() {
        let mut bytes = b"\x1b]9;".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', MAX_OSC_BYTES + 1));
        bytes.extend_from_slice(b"\x1b\\\x1b]9;ok\x07");
        assert!(
            matches!(parse(&bytes).as_slice(), [OscNotification::Legacy(value)] if value == "ok")
        );
    }

    #[test]
    fn parses_base64_and_rejects_controls() {
        let notifications = parse(b"\x1b]99;e=1;aGVsbG8=\x1b\\");
        assert!(matches!(
            notifications.as_slice(),
            [OscNotification::Kitty(KittyNotification { payload: Payload::Title(value), .. })]
                if value == "hello"
        ));
        assert!(parse(b"\x1b]99;;bad\tvalue\x1b\\").is_empty());
        assert_eq!(parse("\x1b]99;;done 🎉\x1b\\".as_bytes()).len(), 1);
    }

    #[test]
    fn validates_identifiers() {
        assert!(parse(b"\x1b]99;i=../../bad;;hello\x1b\\").is_empty());
        assert_eq!(parse(b"\x1b]99;i=good_ID-1.2;;hello\x1b\\").len(), 1);
    }

    #[test]
    fn visibility_policy_matches_protocol() {
        let focused = WindowNotificationState {
            focused: true,
            visible: true,
            occluded: false,
            headless: false,
        };
        let hidden = WindowNotificationState {
            focused: false,
            visible: false,
            occluded: false,
            headless: false,
        };
        assert!(!occasion_matches(Occasion::Unfocused, focused));
        assert!(occasion_matches(Occasion::Invisible, hidden));
    }

    #[test]
    fn rate_limiter_is_bounded() {
        let mut limiter = RateLimiter::default();
        assert!((0..RATE_BURST as usize).all(|_| limiter.take()));
        assert!(!limiter.take());
    }

    #[test]
    fn controller_assembles_updates_and_closes_by_id() {
        let (mut controller, state) = controller();
        let notifier = TestNotifier::default();
        for request in parse(b"\x1b]99;i=shared:d=0;Title\x1b\\\x1b]99;i=shared:p=body;Body\x1b\\")
        {
            controller.handle(request, visible_window(), &notifier);
        }
        controller.handle(
            parse(b"\x1b]99;i=shared;Replacement\x1b\\").pop().unwrap(),
            visible_window(),
            &notifier,
        );
        controller.handle(
            parse(b"\x1b]99;i=shared:p=close;\x1b\\").pop().unwrap(),
            visible_window(),
            &notifier,
        );

        let state = state.lock().unwrap();
        assert_eq!(state.displayed.len(), 2);
        assert_eq!(state.displayed[0].id.as_deref(), Some("shared"));
        assert_eq!(state.displayed[0].title, "Title");
        assert_eq!(state.displayed[0].body, "Body");
        assert!(state.displayed[0].focus);
        assert_eq!(state.closed, ["shared"]);
    }

    #[test]
    fn controller_displays_anonymous_default_title() {
        let (mut controller, state) = controller();
        let notifier = TestNotifier::default();
        let request = parse(b"\x1b]99;;OSC 99 works\x1b\\").pop().unwrap();

        controller.handle(request, visible_window(), &notifier);

        let state = state.lock().unwrap();
        assert_eq!(state.displayed.len(), 1);
        assert_eq!(state.displayed[0].title, "OSC 99 works");
        assert!(state.displayed[0].body.is_empty());
        assert!(state.displayed[0].id.is_none());
    }

    #[test]
    fn capability_query_is_sanitized_and_disabled_is_silent() {
        let (mut controller, state) = controller();
        let notifier = TestNotifier::default();
        let query = parse(b"\x1b]99;i=query_ID:p=?;\x1b\\").pop().unwrap();
        controller.handle(query.clone(), visible_window(), &notifier);
        let response = String::from_utf8(notifier.0.take()).unwrap();
        assert!(response.starts_with("\x1b]99;i=query_ID:p=?;"));
        assert!(response.contains("a=focus"));
        assert!(response.contains("p=title,body,close"));
        assert!(!response.contains("report"));
        assert!(!response.contains("c=1"));

        controller.set_enabled(false);
        controller.handle(query, visible_window(), &notifier);
        assert!(notifier.0.borrow().is_empty());
        assert_eq!(state.lock().unwrap().clears, 1);
    }

    #[test]
    fn same_protocol_id_is_isolated_between_windows() {
        let (mut first, first_state) = controller();
        let (mut second, second_state) = controller();
        let notifier = TestNotifier::default();
        let request = parse(b"\x1b]99;i=same;message\x1b\\").pop().unwrap();
        first.handle(request.clone(), visible_window(), &notifier);
        second.handle(request, visible_window(), &notifier);
        assert_eq!(first_state.lock().unwrap().displayed.len(), 1);
        assert_eq!(second_state.lock().unwrap().displayed.len(), 1);
    }

    #[test]
    fn incomplete_assemblies_expire() {
        let (mut controller, _state) = controller();
        let notifier = TestNotifier::default();
        let request = parse(b"\x1b]99;i=old:d=0;message\x1b\\").pop().unwrap();
        controller.handle(request, visible_window(), &notifier);
        controller.pending.get_mut("old").unwrap().updated_at =
            Some(Instant::now() - ASSEMBLY_TIMEOUT - Duration::from_secs(1));
        controller.expire_pending();
        assert!(!controller.pending.contains_key("old"));
    }
}
