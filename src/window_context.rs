//! Terminal window context.

#[cfg(any(unix, windows))]
use std::borrow::Cow;
#[cfg(any(unix, windows))]
use std::cell::RefCell;
#[cfg(any(unix, windows))]
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::fs::File;
#[cfg(any(unix, windows))]
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::mem;
#[cfg(not(windows))]
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
#[cfg(any(unix, windows))]
use std::sync::Mutex;
#[cfg(windows)]
use std::sync::atomic::AtomicBool;
#[cfg(any(unix, windows))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(any(unix, windows))]
use base64::Engine;
use log::info;
use serde_json as json;
#[cfg(any(unix, windows))]
use serde_json::{Value, json as json_value};
#[cfg(any(unix, windows))]
use winit::dpi::PhysicalPosition;
#[cfg(any(unix, windows))]
use winit::event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase};
use winit::event::{Event as WinitEvent, Modifiers, WindowEvent};
#[cfg(target_os = "macos")]
use winit::event_loop::ActiveEventLoop;
use winit::window::WindowId;

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use crate::accessibility::{AccessibilitySnapshot, AccessibilityState, terminal_document_enabled};
use crate::terminal::event::Event as TerminalEvent;
#[cfg(any(unix, windows))]
use crate::terminal::event::Notify;
#[cfg(any(unix, windows))]
use crate::terminal::event_loop::EventLoopSendError;
use crate::terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use crate::terminal::grid::{Dimensions, Scroll};
use crate::terminal::index::Direction;
#[cfg(any(unix, windows))]
use crate::terminal::index::{Column, Line};
use crate::terminal::sync::FairMutex;
use crate::terminal::term::Term;
#[cfg(any(unix, windows))]
use crate::terminal::term::TermMode;
#[cfg(any(unix, windows))]
use crate::terminal::term::cell::Flags;
use crate::terminal::term::test::TermSize;
use crate::terminal::tty;
#[cfg(any(unix, windows))]
use crate::terminal::vte::ansi::{Color, NamedColor};

#[cfg(any(unix, windows))]
use crate::automation::{AutomationWindowState, Transcript};
#[cfg(any(unix, windows))]
use crate::cli::{
    IpcKey, IpcMouse, IpcMouseAction, IpcMouseButton, IpcMousePath, IpcMousePoint,
    IpcMousePosition, IpcSignalName,
};
use crate::cli::{ParsedOptions, VividTarget, WindowOptions};
use crate::client_fault::{ClientFault, ClientHealth};
use crate::clipboard::Clipboard;
use crate::config::UiConfig;
use crate::display::Display;
#[cfg(any(unix, windows))]
use crate::display::ScreenshotReadback;
#[cfg(any(unix, windows))]
use crate::display::color::{DIM_FACTOR, Rgb};
use crate::event::{
    ActionContext, Event, EventProxy, EventSink, EventType, LoopHandle, Mouse, SearchState,
    TouchPurpose,
};
use crate::input;
#[cfg(any(unix, windows))]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::MessageBuffer;
use crate::osc_notification::{NotificationController, OscNotification, WindowNotificationState};
#[cfg(any(unix, windows))]
use crate::polling::ipc::{IpcConnection, IpcError};
use crate::scheduler::{Scheduler, TimerId, Topic};
#[cfg(any(unix, windows))]
use crate::screenshot;
#[cfg(any(unix, windows))]
use crate::terminal::thread;
use crate::vivid::VividService;
#[cfg(any(unix, windows))]
use crate::vivid::scene::TrackWaitEvaluation;

#[cfg(any(unix, windows))]
type AutomationResize = (u32, u32, Option<(u16, u16)>);

type PtyWorker =
    JoinHandle<(PtyEventLoop<tty::Pty, EventProxy>, crate::terminal::event_loop::State)>;

const VIVID_RESIZE_SETTLE_DELAY: Duration = Duration::from_millis(120);

/// Maximum delay between directly presented frames during continuous Windows input or PTY output.
#[cfg(windows)]
const LATENCY_SENSITIVE_FRAME_INTERVAL: Duration = Duration::from_millis(16);

#[cfg(any(unix, windows))]
#[derive(Default)]
struct AutomationNotifier(RefCell<Vec<u8>>);

#[cfg(any(unix, windows))]
impl AutomationNotifier {
    fn into_bytes(self) -> Vec<u8> {
        self.0.into_inner()
    }
}

#[cfg(any(unix, windows))]
impl Notify for AutomationNotifier {
    fn notify<B: Into<Cow<'static, [u8]>>>(&self, bytes: B) -> Result<(), EventLoopSendError> {
        self.0.borrow_mut().extend_from_slice(bytes.into().as_ref());
        Ok(())
    }
}

/// Event context for one individual Vivido window.
pub struct WindowContext {
    pub message_buffer: MessageBuffer,
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    accessibility: Option<AccessibilityState>,
    pub display: Display,
    pub dirty: bool,
    event_queue: Vec<WinitEvent<Event>>,
    #[cfg(windows)]
    last_latency_sensitive_draw: Option<Instant>,
    #[cfg(windows)]
    latency_sensitive_frame_timer: LatencySensitiveFrameTimer,
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    event_proxy: EventProxy,
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    cursor_blink_timed_out: bool,
    prev_bell_cmd: Option<Instant>,
    notifications: NotificationController,
    modifiers: Modifiers,
    search_state: SearchState,
    notifier: Notifier,
    mouse: Mouse,
    touch: TouchPurpose,
    occluded: bool,
    preserve_title: bool,
    #[cfg(not(windows))]
    master_fd: RawFd,
    shell_pid: u32,
    window_config: ParsedOptions,
    config: Rc<UiConfig>,
    vivid_service: VividService,
    vivid_target: VividTarget,
    restart_pty_config: tty::Options,
    io_thread: Option<PtyWorker>,
    vivid_resize_settled: Option<u64>,
    #[cfg(any(unix, windows))]
    ipc_window_id: u64,
    #[cfg(any(unix, windows))]
    screenshot: Option<PendingScreenshot>,
    #[cfg(any(unix, windows))]
    screenshot_busy: bool,
    #[cfg(any(unix, windows))]
    pub automation: AutomationWindowState,
    client_health: ClientHealth,
    last_client_fault: Option<ClientFault>,
}

/// The next public window ID this process will hand out.
///
/// Small and monotonic, rather than the winit window ID this used to be. A public window ID is
/// also an agent-mesh address segment, and an address index is a `u32`; winit IDs on Wayland start
/// at 2^63, so every pane published an address that could not parse and no agent could bind from
/// inside one. Nothing converts this value back into a `WindowId` — every lookup searches for it —
/// so its only requirements are that it is unique within the process and never reused.
#[cfg(any(unix, windows))]
static NEXT_IPC_WINDOW_ID: AtomicU64 = AtomicU64::new(1);

/// Resolve a window's public ID, honoring one the caller named.
#[cfg(any(unix, windows))]
fn assign_ipc_window_id(requested: Option<u64>) -> u64 {
    match requested {
        // A caller that names its own ID keeps it, and the counter steps past it so a later
        // automatic ID cannot collide with one that was claimed explicitly.
        Some(id) => {
            NEXT_IPC_WINDOW_ID.fetch_max(id.saturating_add(1), Ordering::Relaxed);
            id
        },
        None => NEXT_IPC_WINDOW_ID.fetch_add(1, Ordering::Relaxed),
    }
}

/// Active Windows wake for the final update accumulated by the direct-draw rate limiter.
///
/// The ordinary scheduler advances from winit's `AboutToWait` callback. ConPTY and native input
/// can keep the Windows message queue busy enough that callback never arrives, so a timer stored
/// only in the scheduler can leave the last prompt or selection change invisible indefinitely.
/// This worker owns a bounded one-slot queue and coalesces all requests until the main loop
/// acknowledges the corresponding event.
#[cfg(windows)]
struct LatencySensitiveFrameTimer {
    sender: SyncSender<Duration>,
    pending: Arc<AtomicBool>,
}

#[cfg(windows)]
impl LatencySensitiveFrameTimer {
    fn new(event_sink: EventSink, window_id: WindowId) -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        let pending = Arc::new(AtomicBool::new(false));
        let worker_pending = Arc::clone(&pending);
        let _worker = thread::spawn_named("latency-sensitive frame timer", move || {
            while let Ok(delay) = receiver.recv() {
                std::thread::sleep(delay);
                if event_sink
                    .send_event(Event::new(EventType::LatencySensitiveFrame, window_id))
                    .is_err()
                {
                    worker_pending.store(false, Ordering::Relaxed);
                    break;
                }
            }
        });

        Self { sender, pending }
    }

    fn schedule(&self, delay: Duration) {
        if self.pending.compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed).is_err()
        {
            return;
        }

        match self.sender.try_send(delay) {
            Ok(()) | Err(TrySendError::Full(_)) => (),
            Err(TrySendError::Disconnected(_)) => {
                self.pending.store(false, Ordering::Relaxed);
            },
        }
    }

    fn acknowledge(&self) {
        self.pending.store(false, Ordering::Relaxed);
    }
}

#[cfg(any(unix, windows))]
struct PendingScreenshot {
    readback: ScreenshotReadback,
    connection: IpcConnection,
    request_id: u64,
    metadata: serde_json::Value,
}

#[cfg(any(unix, windows))]
const SCREENSHOT_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[cfg(any(unix, windows))]
const SCREENSHOT_READBACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether an event must bypass batching and frame-token gating.
///
/// Wheel input is intentionally latency-sensitive: a freely spinning wheel can keep the native
/// message queue busy indefinitely, so neither `AboutToWait` nor a scheduled `Frame` user event is
/// guaranteed to run promptly. Windows keyboard, IME, and pointer events must also flush
/// immediately. Staged keyboard events have not reached the child yet, while staged pointer events
/// have not updated selection state. The native redraw request still coalesces outstanding paints.
pub(crate) fn is_latency_sensitive_window_event(event: &WindowEvent) -> bool {
    match event {
        WindowEvent::MouseWheel { .. } => cfg!(any(target_os = "linux", target_os = "windows")),
        #[cfg(windows)]
        WindowEvent::KeyboardInput { is_synthetic: false, .. }
        | WindowEvent::Ime(_)
        | WindowEvent::MouseInput { .. }
        | WindowEvent::CursorMoved { .. } => true,
        _ => false,
    }
}

fn is_latency_sensitive_input(event: &WinitEvent<Event>) -> bool {
    match event {
        WinitEvent::WindowEvent { event, .. } => is_latency_sensitive_window_event(event),
        _ => false,
    }
}

/// Whether an event must flush input already staged for this window.
fn flushes_staged_input(event: &WinitEvent<Event>) -> bool {
    matches!(
        event,
        WinitEvent::AboutToWait
            | WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. }
    ) || is_latency_sensitive_input(event)
}

#[cfg(windows)]
fn latency_sensitive_draw_delay(last_draw: Option<Instant>, now: Instant) -> Option<Duration> {
    last_draw.and_then(|last_draw| {
        LATENCY_SENSITIVE_FRAME_INTERVAL
            .checked_sub(now.saturating_duration_since(last_draw))
            .filter(|delay| !delay.is_zero())
    })
}

impl WindowContext {
    /// Close this terminal even when its configured hold policy is enabled.
    pub fn request_close(&mut self) {
        self.display.window.hold = false;
        self.terminal.lock().exit();
    }

    pub(crate) fn acknowledge_vivid_frame(&mut self) {
        self.vivid_service.acknowledge_frame_wake();
        self.display.mark_vivid_frame();
    }

    pub(crate) fn retry_renderer(&mut self, scheduler: &mut Scheduler) {
        if self.display.retry_renderer(&self.config, scheduler) {
            self.dirty = true;
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            }
        }
    }

    /// Create initial window context.
    pub fn initial(
        event_loop: LoopHandle<'_>,
        proxy: EventSink,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        let window = event_loop.create_window(&config, &identity, &mut options)?;
        let display = Display::new(window, &config)?;

        Self::new(event_loop, display, config, options, proxy, false)
    }

    /// Create additional context.
    pub fn additional(
        event_loop: LoopHandle<'_>,
        proxy: EventSink,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        config_overrides: ParsedOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Check if new window will be opened as a tab.
        // This must be done before `Window::new()`, which unsets `window_tabbing_id`.
        #[cfg(target_os = "macos")]
        let tabbed = options.window_tabbing_id.is_some();
        #[cfg(not(target_os = "macos"))]
        let tabbed = false;

        let window = event_loop.create_window(&config, &identity, &mut options)?;
        let display = Display::new(window, &config)?;

        let mut window_context = Self::new(event_loop, display, config, options, proxy, tabbed)?;

        // Set the config overrides at startup.
        //
        // These are already applied to `config`, so no update is necessary.
        window_context.window_config = config_overrides;

        Ok(window_context)
    }

    /// Create a new terminal window context.
    fn new(
        #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
        event_loop_handle: LoopHandle<'_>,
        mut display: Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventSink,
        tabbed: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let mut pty_config = config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);
        let restart_pty_config = pty_config.clone();
        let vivid_target = options.vivid_target;

        let preserve_title = options.window_identity.title.is_some();
        #[cfg(any(unix, windows))]
        let ipc_window_id = assign_ipc_window_id(options.ipc_window_id);

        info!(
            "PTY dimensions: {:?} x {:?}",
            display.size_info.screen_lines(),
            display.size_info.columns()
        );

        #[cfg(windows)]
        let latency_sensitive_frame_timer =
            LatencySensitiveFrameTimer::new(proxy.clone(), display.window.id());
        let event_proxy = EventProxy::new(proxy, display.window.id());
        let notifications = NotificationController::new(
            config.terminal.osc_notifications,
            !display.window.is_headless() && !display.window.is_embedded(),
            event_proxy.clone(),
        );

        let vivid_service = {
            let service = match options.vivid_target {
                VividTarget::Terminal => VividService::start(
                    display.size_info.into(),
                    event_proxy.clone(),
                    config.file_drop.paste_remote_path,
                )?,
                VividTarget::Desktop => {
                    VividService::start_desktop(display.size_info.into(), event_proxy.clone())?
                },
            };
            configure_vivid_pty_environment(
                &mut pty_config.env,
                service.control_endpoint(),
                service.root_secret(),
                ipc_window_id,
            );
            display.set_vivid_scene(service.scene());
            service
        };

        // Create the terminal.
        //
        // This object contains all of the state about what's being displayed. It's
        // wrapped in a clonable mutex since both the I/O loop and display need to
        // access it.
        let terminal = Term::new(config.term_options(), &display.size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        let accessibility = if terminal_document_enabled() {
            let snapshot = AccessibilitySnapshot::new(
                &terminal.lock(),
                display.size_info,
                display.window.title(),
            );
            #[cfg(target_os = "macos")]
            let state = (!display.window.is_headless() && !display.window.is_embedded())
                .then(|| AccessibilityState::new(&display.window, options.vivid_target, snapshot));
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            let state = event_loop_handle.winit().and_then(|event_loop| {
                display.window.winit_window().map(|window| {
                    AccessibilityState::new(event_loop, window, options.vivid_target, snapshot)
                })
            });
            state
        } else {
            None
        };

        // Map only after any enabled native accessibility adapter has been installed.
        display.map_window(&config, tabbed, options.no_activate);

        // Create the PTY.
        //
        // The PTY forks a process to run the shell on the slave side of the
        // pseudoterminal. A file descriptor for the master side is retained for
        // reading/writing to the shell.
        #[cfg(any(unix, windows))]
        let terminal_window_id = ipc_window_id;
        #[cfg(not(any(unix, windows)))]
        let terminal_window_id = display.window.id().into();
        let pty = tty::new(&pty_config, display.size_info.into(), terminal_window_id)?;

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();
        #[cfg(windows)]
        let shell_pid = pty.child_watcher().pid().map_or(0, std::num::NonZeroU32::get);

        // Create the pseudoterminal I/O loop.
        //
        // PTY I/O is ran on another thread as to not occupy cycles used by the
        // renderer and input processing. Note that access to the terminal state is
        // synchronized since the I/O loop updates the state, and the display
        // consumes it periodically.
        #[cfg(any(unix, windows))]
        let transcript = Arc::new(Mutex::new(Transcript::default()));
        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy.clone(),
            pty,
            pty_config.drain_on_exit,
            config.debug.ref_test,
            #[cfg(any(unix, windows))]
            transcript.clone(),
        )?;

        // The event loop channel allows write requests from the event processor
        // to be sent to the pty loop and ultimately written to the pty.
        let loop_tx = event_loop.channel();

        // Kick off the I/O thread.
        let io_thread = event_loop.spawn();

        // Start cursor blinking, in case `Focused` isn't sent on startup.
        if config.cursor.style().blinking {
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        // Create context for the Vivido window.
        Ok(WindowContext {
            preserve_title,
            terminal,
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            event_proxy,
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            accessibility,
            display,
            #[cfg(not(windows))]
            master_fd,
            shell_pid,
            config,
            notifier: Notifier(loop_tx),
            cursor_blink_timed_out: Default::default(),
            prev_bell_cmd: Default::default(),
            notifications,
            message_buffer: Default::default(),
            window_config: Default::default(),
            search_state: Default::default(),
            event_queue: Default::default(),
            #[cfg(windows)]
            last_latency_sensitive_draw: None,
            #[cfg(windows)]
            latency_sensitive_frame_timer,
            modifiers: Default::default(),
            occluded: Default::default(),
            mouse: Default::default(),
            touch: Default::default(),
            dirty: Default::default(),
            vivid_service,
            vivid_target,
            restart_pty_config,
            io_thread: Some(io_thread),
            vivid_resize_settled: None,
            #[cfg(any(unix, windows))]
            ipc_window_id,
            #[cfg(any(unix, windows))]
            screenshot: None,
            #[cfg(any(unix, windows))]
            screenshot_busy: false,
            #[cfg(any(unix, windows))]
            automation: AutomationWindowState::new(0, transcript),
            client_health: ClientHealth::Healthy,
            last_client_fault: None,
        })
    }

    /// Update the terminal window to the latest config.
    pub fn update_config(&mut self, new_config: Rc<UiConfig>) {
        let old_config = mem::replace(&mut self.config, new_config);

        // Apply ipc config if there are overrides.
        self.config = self.window_config.override_config_rc(self.config.clone());

        self.display.update_config(&self.config);
        self.terminal.lock().set_options(self.config.term_options());
        self.notifications.set_enabled(self.config.terminal.osc_notifications);
        self.vivid_service.set_remote_drop_paste(self.config.file_drop.paste_remote_path);

        // Reload cursor if its thickness has changed.
        if (old_config.cursor.thickness() - self.config.cursor.thickness()).abs() > f32::EPSILON {
            self.display.pending_update.set_cursor_dirty();
        }

        if old_config.font != self.config.font {
            let scale_factor = self.display.window.scale_factor as f32;
            // Do not update font size if it has been changed at runtime.
            if self.display.font_size == old_config.font.size().scale(scale_factor) {
                self.display.font_size = self.config.font.size().scale(scale_factor);
            }

            let font = self.config.font.clone().with_size(self.display.font_size);
            self.display.pending_update.set_font(font);
        }

        // Always reload the theme to account for auto-theme switching.
        self.display.window.set_theme(self.config.window.theme());

        // Update display if either padding options or resize increments were changed.
        let window_config = &old_config.window;
        if window_config.padding(1.) != self.config.window.padding(1.)
            || window_config.dynamic_padding != self.config.window.dynamic_padding
            || window_config.resize_increments != self.config.window.resize_increments
        {
            self.display.pending_update.dirty = true;
        }

        // Update title on config reload according to the following table.
        //
        // │cli │ dynamic_title │ current_title == old_config ││ set_title │
        // │ Y  │       _       │              _              ││     N     │
        // │ N  │       Y       │              Y              ││     Y     │
        // │ N  │       Y       │              N              ││     N     │
        // │ N  │       N       │              _              ││     Y     │
        if !self.preserve_title
            && (!self.config.window.dynamic_title
                || self.display.window.title() == old_config.window.identity.title)
        {
            self.display.window.set_title(self.config.window.identity.title.clone());
        }

        let opaque = self.config.window_opacity() >= 1.;

        // Disable shadows for transparent windows on macOS.
        #[cfg(target_os = "macos")]
        self.display.window.set_has_shadow(opaque);

        #[cfg(target_os = "macos")]
        self.display.window.set_option_as_alt(self.config.window.option_as_alt());

        // Change opacity and blur state.
        self.display.window.set_transparent(!opaque);
        self.display.window.set_blur(self.config.window.blur);

        // Update hint keys.
        self.display.hint_state.update_alphabet(self.config.hints.alphabet());

        // Update cursor blinking.
        let event = Event::new(TerminalEvent::CursorBlinkingChange.into(), None);
        self.event_queue.push(event.into());

        self.dirty = true;
    }

    /// Get reference to the window's configuration.
    #[cfg(any(unix, windows))]
    pub fn config(&self) -> &UiConfig {
        &self.config
    }

    /// Clear the window config overrides.
    #[cfg(any(unix, windows))]
    pub fn reset_window_config(&mut self, config: Rc<UiConfig>) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);
        self.display.pending_update.dirty = true;

        self.window_config.clear();

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Add new window config overrides.
    #[cfg(any(unix, windows))]
    pub fn add_window_config(&mut self, config: Rc<UiConfig>, options: &ParsedOptions) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);
        self.display.pending_update.dirty = true;

        self.window_config.extend_from_slice(options);

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Draw the window.
    pub fn draw(&mut self, scheduler: &mut Scheduler) -> bool {
        self.display.window.requested_redraw = false;
        self.vivid_service.flush_display_change(self.vivid_resize_settled.take());

        if self.occluded {
            return false;
        }

        self.dirty = false;

        // Force the display to process any pending display update.
        self.display.process_renderer_update();

        // Request immediate re-draw if visual bell animation is not finished yet.
        if !self.display.visual_bell.completed() {
            // We can get an OS redraw which bypasses Vivido's frame throttling, thus
            // marking the window as dirty when we don't have frame yet.
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            } else {
                self.dirty = true;
            }
        }

        // Redraw the window.
        let terminal = self.terminal.lock();
        self.display.draw(
            terminal,
            scheduler,
            &self.message_buffer,
            &self.config,
            &mut self.search_state,
        )
    }

    /// Present latency-sensitive state without waiting for Windows to synthesize `WM_PAINT`.
    ///
    /// `WM_PAINT` has lower priority than posted input and PTY wakeups, so a continuous stream can
    /// otherwise update the terminal model for seconds without presenting it. The timestamp is
    /// recorded after the draw, allowing subsequent updates to accumulate for one frame interval
    /// instead of rendering each event and falling behind the stream.
    #[cfg(windows)]
    pub fn draw_latency_sensitive(&mut self, scheduler: &mut Scheduler) -> Option<bool> {
        if !self.dirty
            || self.occluded
            || self.display.window.is_headless()
            || self.display.window.is_visible() == Some(false)
        {
            return None;
        }

        let now = Instant::now();
        if let Some(delay) = latency_sensitive_draw_delay(self.last_latency_sensitive_draw, now) {
            self.latency_sensitive_frame_timer.schedule(delay);
            return None;
        }

        let presented = self.draw(scheduler);
        self.last_latency_sensitive_draw = Some(Instant::now());
        Some(presented)
    }

    /// Present the accumulated state when the Windows frame timer expires.
    ///
    /// The latency-sensitive path deliberately accumulates updates for one frame interval. Its
    /// timer must finish that interval with a direct presentation; falling back to `WM_PAINT`
    /// recreates the starvation this path exists to avoid and can leave the final typed bytes
    /// invisible until another mouse event arrives.
    #[cfg(windows)]
    pub fn draw_scheduled_frame(&mut self, scheduler: &mut Scheduler) -> Option<bool> {
        if !self.dirty
            || self.occluded
            || self.display.window.is_headless()
            || self.display.window.is_visible() == Some(false)
        {
            return None;
        }

        let presented = self.draw(scheduler);
        self.last_latency_sensitive_draw = Some(Instant::now());
        Some(presented)
    }

    /// Acknowledge the active Windows tail-frame wake so another interval can be queued.
    #[cfg(windows)]
    pub fn acknowledge_latency_sensitive_frame(&self) {
        self.latency_sensitive_frame_timer.acknowledge();
    }

    /// Open the coalescing gate for another terminal-model notification.
    #[cfg(windows)]
    pub fn acknowledge_terminal_wakeup(&self) {
        self.event_proxy.acknowledge_terminal_wakeup();
    }

    /// Take the complete transcript span accumulated behind one Windows UI notification.
    #[cfg(windows)]
    pub fn take_pty_output(&self, fallback: (u64, u64)) -> (u64, u64) {
        self.event_proxy.take_pty_output(fallback)
    }

    /// Take the ordered Vivid terminal-position updates represented by one Windows notification.
    #[cfg(windows)]
    pub fn take_pending_vivid_terminal_events(&self) -> std::collections::VecDeque<TerminalEvent> {
        self.event_proxy.take_pending_vivid_terminal_events()
    }

    /// Process events for this terminal window.
    pub fn handle_event(
        &mut self,
        #[cfg(target_os = "macos")] event_loop: Option<&ActiveEventLoop>,
        event_proxy: &EventSink,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: WinitEvent<Event>,
    ) {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        if let WinitEvent::WindowEvent { event, .. } = &event
            && let (Some(accessibility), Some(window)) =
                (&mut self.accessibility, self.display.window.winit_window())
        {
            accessibility.process_event(window, event);
        }

        let redraw_requested =
            matches!(&event, WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. });
        let focus_lost =
            matches!(&event, WinitEvent::WindowEvent { event: WindowEvent::Focused(false), .. });
        let latency_sensitive_input = is_latency_sensitive_input(&event);
        let flush_staged_input = flushes_staged_input(&event);

        match event {
            WinitEvent::AboutToWait
            | WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                // Skip further event handling with no staged updates.
                if self.event_queue.is_empty() {
                    return;
                }

                // Continue to process all pending events.
            },
            // Windows keyboard, IME, and pointer input and a freely spinning wheel can keep the
            // platform message queue non-empty, preventing `AboutToWait` from arriving. Flush all
            // staged input on each latency-sensitive event so it takes effect without an idle turn.
            event if flush_staged_input => {
                self.event_queue.push(event);
            },
            event => {
                self.event_queue.push(event);
                return;
            },
        }

        let mut terminal = self.terminal.lock();

        let old_is_searching = self.search_state.history_index.is_some();

        // Desktop §4: focus loss revokes the effective input grant, and nothing reinstates it
        // when focus returns — the producer must issue a strictly greater epoch.
        if focus_lost {
            self.vivid_service.revoke_all_input(vivid_protocol::grant::reason::FOCUS_LOSS);
        }

        let context = ActionContext {
            cursor_blink_timed_out: &mut self.cursor_blink_timed_out,
            prev_bell_cmd: &mut self.prev_bell_cmd,
            message_buffer: &mut self.message_buffer,
            search_state: &mut self.search_state,
            modifiers: &mut self.modifiers,
            notifier: &mut self.notifier,
            display: &mut self.display,
            mouse: &mut self.mouse,
            touch: &mut self.touch,
            dirty: &mut self.dirty,
            occluded: &mut self.occluded,
            terminal: &mut terminal,
            #[cfg(not(windows))]
            master_fd: self.master_fd,
            #[cfg(not(windows))]
            shell_pid: self.shell_pid,
            preserve_title: self.preserve_title,
            vivid_service: &self.vivid_service,
            config: &self.config,
            event_proxy,
            #[cfg(target_os = "macos")]
            event_loop,
            clipboard,
            scheduler,
        };
        let mut processor = input::Processor::new(context);

        for event in self.event_queue.drain(..) {
            processor.handle_event(event);
        }

        // Process DisplayUpdate events.
        if self.display.pending_update.dirty {
            // Compute cursor positions before resize.
            let num_lines = terminal.screen_lines();
            let cursor_at_bottom = terminal.grid().cursor.point.line + 1 == num_lines;
            let origin_at_bottom = self.search_state.direction == Direction::Left;

            self.display.handle_update(
                &mut terminal,
                &self.vivid_service,
                &mut self.notifier,
                &self.message_buffer,
                &mut self.search_state,
                &self.config,
            );

            let new_is_searching = self.search_state.history_index.is_some();
            if !old_is_searching && new_is_searching {
                // Scroll on search start to make sure origin is visible with minimal viewport
                // motion.
                let display_offset = terminal.grid().display_offset();
                if display_offset == 0 && cursor_at_bottom && !origin_at_bottom {
                    terminal.scroll_display(Scroll::Delta(1));
                } else if display_offset != 0 && origin_at_bottom {
                    terminal.scroll_display(Scroll::Delta(-1));
                }
            }

            self.dirty = true;
            let changed = self.vivid_service.update_metrics(self.display.size_info.into());
            if let Some(generation) = changed {
                let window_id = self.id();
                let timer_id = TimerId::new(Topic::VividResizeSettled, window_id);
                scheduler.unschedule(timer_id);
                scheduler.schedule(
                    Event::new(EventType::VividResizeSettled(generation), window_id),
                    VIVID_RESIZE_SETTLE_DELAY,
                    false,
                    timer_id,
                );
            }
        }

        if self.dirty || self.mouse.hint_highlight_dirty {
            self.dirty |= self.display.update_highlighted_hints(
                &terminal,
                &self.config,
                &self.mouse,
                self.modifiers.state(),
            );
            self.mouse.hint_highlight_dirty = false;
        }

        // Don't call `request_redraw` when event is `RedrawRequested` since the `dirty` flag
        // represents the current frame, but redraw is for the next frame.
        if self.dirty
            && (self.display.window.has_frame || latency_sensitive_input)
            && !self.occluded
            && !redraw_requested
        {
            self.display.window.request_redraw();
        }
    }

    pub fn settle_vivid_resize(&mut self, generation: u64) {
        self.vivid_resize_settled = Some(generation);
        self.dirty = true;
        self.display.window.request_redraw();
    }

    /// ID of this terminal context.
    pub fn id(&self) -> WindowId {
        self.display.window.id()
    }

    /// Current terminal window title.
    pub fn title(&self) -> &str {
        self.display.window.title()
    }

    /// Working directory of the local shell: its OSC 7 report when available, otherwise the
    /// foreground process's.
    pub fn current_directory(&self) -> Option<PathBuf> {
        self.terminal
            .lock()
            .working_directory()
            .and_then(reported_working_directory)
            .or_else(|| self.probed_working_directory())
    }

    /// Immutable accessibility state for composition by a containing shell.
    #[cfg(target_os = "linux")]
    pub(crate) fn accessibility_snapshot(&self) -> AccessibilitySnapshot {
        AccessibilitySnapshot::new(
            &self.terminal.lock(),
            self.display.size_info,
            self.display.window.title(),
        )
    }

    /// Current terminal content size in physical pixels.
    #[cfg(any(target_os = "linux", windows))]
    pub(crate) fn terminal_content_size(&self) -> winit::dpi::PhysicalSize<u32> {
        self.display.window.inner_size()
    }

    /// Working directory of the foreground process, when the platform can report it.
    fn probed_working_directory(&self) -> Option<PathBuf> {
        #[cfg(not(windows))]
        {
            crate::daemon::foreground_process_path(self.master_fd, self.shell_pid).ok()
        }

        #[cfg(windows)]
        {
            None
        }
    }

    /// Stable external ID used to target this window through IPC.
    #[cfg(any(unix, windows))]
    pub fn ipc_window_id(&self) -> u64 {
        self.ipc_window_id
    }

    /// Whether this terminal currently has keyboard focus.
    #[cfg(any(unix, windows))]
    pub fn is_focused(&self) -> bool {
        self.terminal.lock().is_focused
    }

    /// Health of the untrusted client currently attached to this pane.
    #[cfg(any(unix, windows))]
    pub fn client_health(&self) -> ClientHealth {
        self.client_health
    }

    /// Write bytes and notify the main event loop after the PTY master accepted all of them.
    #[cfg(any(unix, windows))]
    pub fn write_to_pty_with_completion(
        &self,
        bytes: Vec<u8>,
        completion: u64,
    ) -> Result<(), EventLoopSendError> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.notifier.0.send(Msg::Input { bytes: bytes.into(), completion: Some(completion) })
    }

    /// Write automation bytes without creating a correlated completion response.
    #[cfg(any(unix, windows))]
    pub fn write_automation_bytes(&self, bytes: Vec<u8>) -> Result<(), EventLoopSendError> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.notifier.0.send(Msg::Input { bytes: bytes.into(), completion: None })
    }

    /// Apply the current terminal dimensions to the PTY and report completion.
    #[cfg(any(unix, windows))]
    pub fn write_pty_resize_with_completion(
        &self,
        completion: u64,
    ) -> Result<(), EventLoopSendError> {
        self.notifier.0.send(Msg::Resize {
            window_size: self.display.size_info.into(),
            completion: Some(completion),
        })
    }

    /// Capture terminal grid text without styling or display overlays.
    #[cfg(any(unix, windows))]
    pub fn text(&self, rows: Option<u16>) -> String {
        let terminal = self.terminal.lock();
        match rows {
            Some(rows) => terminal.latest_text(usize::from(rows)),
            None => terminal.visible_text(),
        }
    }

    /// Build application-directed paste bytes with the same safety filtering as local paste.
    #[cfg(any(unix, windows))]
    pub fn application_paste(&self, text: &str) -> Vec<u8> {
        let bracketed = self.terminal.lock().mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            let filtered = text.replace(['\x1b', '\x03'], "");
            let mut bytes = Vec::with_capacity(filtered.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(filtered.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
        }
    }

    /// Process paste through search/UI state, returning tagged PTY bytes when it reaches the app.
    #[cfg(any(unix, windows))]
    pub fn ui_paste(
        &mut self,
        text: &str,
        #[cfg(target_os = "macos")] event_loop: Option<&ActiveEventLoop>,
        event_proxy: &EventSink,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
    ) -> Vec<u8> {
        if self.search_state.regex().is_none() {
            return self.application_paste(text);
        }

        let mut notifier = AutomationNotifier::default();
        {
            let mut terminal = self.terminal.lock();
            let context = ActionContext {
                cursor_blink_timed_out: &mut self.cursor_blink_timed_out,
                prev_bell_cmd: &mut self.prev_bell_cmd,
                message_buffer: &mut self.message_buffer,
                search_state: &mut self.search_state,
                modifiers: &mut self.modifiers,
                notifier: &mut notifier,
                display: &mut self.display,
                mouse: &mut self.mouse,
                touch: &mut self.touch,
                dirty: &mut self.dirty,
                occluded: &mut self.occluded,
                terminal: &mut terminal,
                #[cfg(not(windows))]
                master_fd: self.master_fd,
                #[cfg(not(windows))]
                shell_pid: self.shell_pid,
                preserve_title: self.preserve_title,
                vivid_service: &self.vivid_service,
                config: &self.config,
                event_proxy,
                #[cfg(target_os = "macos")]
                event_loop,
                clipboard,
                scheduler,
            };
            let mut processor = input::Processor::new(context);
            input::ActionContext::paste(&mut processor.ctx, text, true);
        }
        notifier.into_bytes()
    }

    /// Active terminal modes used by application key encoding.
    #[cfg(any(unix, windows))]
    pub fn terminal_mode(&self) -> TermMode {
        *self.terminal.lock().mode()
    }

    /// Queue a deterministic reset of parser and client-controlled terminal state.
    #[cfg(any(unix, windows))]
    pub fn reset_terminal_client(&mut self, completion: u64) -> Result<(), IpcError> {
        if self.io_thread.as_ref().is_none_or(JoinHandle::is_finished) {
            return Err(IpcError::new("pty_closed", "terminal PTY worker has exited"));
        }
        self.notifier.0.send(Msg::ResetClient { completion }).map_err(|error| {
            IpcError::new("pty_closed", format!("failed to reset terminal: {error}"))
        })?;
        self.vivid_service.disconnect_clients();
        self.client_health = ClientHealth::Recovering;
        Ok(())
    }

    /// Replace this pane's PTY and Vivid service without changing its window or stable IPC ID.
    #[cfg(any(unix, windows))]
    pub fn restart_terminal_client(&mut self) -> Result<(), IpcError> {
        let mut pty_config = self.restart_pty_config.clone();
        let new_service = match self.vivid_target {
            VividTarget::Terminal => VividService::start(
                self.display.size_info.into(),
                self.event_proxy.clone(),
                self.config.file_drop.paste_remote_path,
            ),
            VividTarget::Desktop => {
                VividService::start_desktop(self.display.size_info.into(), self.event_proxy.clone())
            },
        }
        .map_err(|error| {
            IpcError::new("invalid_state", format!("failed to restart Vivid service: {error}"))
        })?;
        configure_vivid_pty_environment(
            &mut pty_config.env,
            new_service.control_endpoint(),
            new_service.root_secret(),
            self.ipc_window_id,
        );
        let pty = tty::new(&pty_config, self.display.size_info.into(), self.ipc_window_id)
            .map_err(|error| {
                IpcError::new("invalid_state", format!("failed to restart PTY: {error}"))
            })?;
        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();
        #[cfg(windows)]
        let shell_pid = pty.child_watcher().pid().map_or(0, std::num::NonZeroU32::get);
        let event_loop = PtyEventLoop::new(
            Arc::clone(&self.terminal),
            self.event_proxy.clone(),
            pty,
            pty_config.drain_on_exit,
            self.config.debug.ref_test,
            self.automation.transcript.clone(),
        )
        .map_err(|error| {
            IpcError::new("invalid_state", format!("failed to restart PTY worker: {error}"))
        })?;

        let _ = self.notifier.0.send(Msg::Shutdown);
        if let Some(worker) = self.io_thread.take() {
            let _ = worker.join();
        }
        self.terminal.lock().reset_client_state();
        self.display.set_vivid_scene(new_service.scene());
        self.vivid_service = new_service;
        #[cfg(not(windows))]
        {
            self.master_fd = master_fd;
        }
        self.shell_pid = shell_pid;
        self.notifier = Notifier(event_loop.channel());
        self.io_thread = Some(event_loop.spawn());
        self.automation.exit_status = None;
        self.complete_client_reset();
        Ok(())
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn record_client_fault(&mut self, fault: ClientFault, quarantined: bool) {
        if quarantined {
            self.client_health = ClientHealth::Quarantined;
        }
        self.last_client_fault = Some(fault);
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn complete_client_reset(&mut self) {
        self.client_health = ClientHealth::Healthy;
        self.dirty = true;
    }

    /// Process a neutral key through Vivido's normal UI input processor.
    #[cfg(any(unix, windows))]
    pub fn ui_key(
        &mut self,
        key: &IpcKey,
        repeated: bool,
        #[cfg(target_os = "macos")] event_loop: Option<&ActiveEventLoop>,
        event_proxy: &EventSink,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
    ) -> Result<Vec<u8>, IpcError> {
        let mut notifier = AutomationNotifier::default();
        let mut terminal = self.terminal.lock();
        let context = ActionContext {
            cursor_blink_timed_out: &mut self.cursor_blink_timed_out,
            prev_bell_cmd: &mut self.prev_bell_cmd,
            message_buffer: &mut self.message_buffer,
            search_state: &mut self.search_state,
            modifiers: &mut self.modifiers,
            notifier: &mut notifier,
            display: &mut self.display,
            mouse: &mut self.mouse,
            touch: &mut self.touch,
            dirty: &mut self.dirty,
            occluded: &mut self.occluded,
            terminal: &mut terminal,
            #[cfg(not(windows))]
            master_fd: self.master_fd,
            #[cfg(not(windows))]
            shell_pid: self.shell_pid,
            preserve_title: self.preserve_title,
            vivid_service: &self.vivid_service,
            config: &self.config,
            event_proxy,
            #[cfg(target_os = "macos")]
            event_loop,
            clipboard,
            scheduler,
        };
        let encoded =
            input::Processor::new(context).ipc_key_input(&key.key, &key.mods, repeated)?;
        drop(terminal);
        let mut bytes = notifier.into_bytes();
        if let Some(encoded) = encoded {
            bytes.extend(encoded);
        }
        Ok(bytes)
    }

    /// Process mouse actions through Vivido's normal UI mouse processor.
    #[cfg(any(unix, windows))]
    pub fn ui_mouse(
        &mut self,
        mouse: &IpcMouse,
        #[cfg(target_os = "macos")] event_loop: Option<&ActiveEventLoop>,
        event_proxy: &EventSink,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
    ) -> Result<Vec<u8>, IpcError> {
        let position = match &mouse.action {
            IpcMouseAction::Move(position) => position,
            IpcMouseAction::Click(action)
            | IpcMouseAction::DoubleClick(action)
            | IpcMouseAction::Down(action)
            | IpcMouseAction::Up(action)
            | IpcMouseAction::Drag(action) => &action.position,
            IpcMouseAction::Path(_) => {
                return Err(IpcError::new(
                    "invalid_params",
                    "mouse paths require the path input handler",
                ));
            },
            IpcMouseAction::Scroll(action) => &action.position,
        };
        let modifier_override = crate::input::keyboard::ipc_modifier_state(&position.mods)?;
        let physical = self.resolve_mouse_position(position)?.physical;

        let mut notifier = AutomationNotifier::default();
        {
            let mut terminal = self.terminal.lock();
            let context = ActionContext {
                cursor_blink_timed_out: &mut self.cursor_blink_timed_out,
                prev_bell_cmd: &mut self.prev_bell_cmd,
                message_buffer: &mut self.message_buffer,
                search_state: &mut self.search_state,
                modifiers: &mut self.modifiers,
                notifier: &mut notifier,
                display: &mut self.display,
                mouse: &mut self.mouse,
                touch: &mut self.touch,
                dirty: &mut self.dirty,
                occluded: &mut self.occluded,
                terminal: &mut terminal,
                #[cfg(not(windows))]
                master_fd: self.master_fd,
                #[cfg(not(windows))]
                shell_pid: self.shell_pid,
                preserve_title: self.preserve_title,
                vivid_service: &self.vivid_service,
                config: &self.config,
                event_proxy,
                #[cfg(target_os = "macos")]
                event_loop,
                clipboard,
                scheduler,
            };
            let mut processor = input::Processor::new(context);
            processor.set_modifier_override(modifier_override);
            if !matches!(mouse.action, IpcMouseAction::Drag(_)) {
                processor.mouse_moved(physical);
            }
            let button = |button| match button {
                IpcMouseButton::Left => MouseButton::Left,
                IpcMouseButton::Middle => MouseButton::Middle,
                IpcMouseButton::Right => MouseButton::Right,
            };
            match &mouse.action {
                IpcMouseAction::Move(_) => (),
                IpcMouseAction::Click(action) => {
                    let button = button(action.button);
                    processor.mouse_input(ElementState::Pressed, button);
                    processor.mouse_input(ElementState::Released, button);
                },
                IpcMouseAction::DoubleClick(action) => {
                    let button = button(action.button);
                    for _ in 0..2 {
                        processor.mouse_input(ElementState::Pressed, button);
                        processor.mouse_input(ElementState::Released, button);
                    }
                },
                IpcMouseAction::Down(action) => {
                    processor.mouse_input(ElementState::Pressed, button(action.button));
                },
                IpcMouseAction::Up(action) => {
                    processor.mouse_input(ElementState::Released, button(action.button));
                },
                IpcMouseAction::Drag(action) => {
                    processor.mouse_input(ElementState::Pressed, button(action.button));
                    processor.mouse_moved(physical);
                },
                IpcMouseAction::Path(_) => unreachable!("mouse paths use ui_mouse_path"),
                IpcMouseAction::Scroll(action) => processor.mouse_wheel_input(
                    MouseScrollDelta::LineDelta(action.horizontal as f32, action.vertical as f32),
                    TouchPhase::Moved,
                ),
            }
        }
        Ok(notifier.into_bytes())
    }

    /// Process one bounded physical-pixel gesture through Vivido's UI mouse processor.
    #[cfg(any(unix, windows))]
    pub fn ui_mouse_path(
        &mut self,
        path: &IpcMousePath,
        #[cfg(target_os = "macos")] event_loop: Option<&ActiveEventLoop>,
        event_proxy: &EventSink,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
    ) -> Result<Vec<u8>, IpcError> {
        validate_mouse_path(path)?;
        let modifier_override = crate::input::keyboard::ipc_modifier_state(&path.mods)?;
        let points = path
            .points
            .iter()
            .map(|point| self.resolve_pixel_point(*point).map(|position| position.physical))
            .collect::<Result<Vec<_>, _>>()?;
        let button = match path.button {
            IpcMouseButton::Left => MouseButton::Left,
            IpcMouseButton::Middle => MouseButton::Middle,
            IpcMouseButton::Right => MouseButton::Right,
        };

        let mut notifier = AutomationNotifier::default();
        {
            let mut terminal = self.terminal.lock();
            let context = ActionContext {
                cursor_blink_timed_out: &mut self.cursor_blink_timed_out,
                prev_bell_cmd: &mut self.prev_bell_cmd,
                message_buffer: &mut self.message_buffer,
                search_state: &mut self.search_state,
                modifiers: &mut self.modifiers,
                notifier: &mut notifier,
                display: &mut self.display,
                mouse: &mut self.mouse,
                touch: &mut self.touch,
                dirty: &mut self.dirty,
                occluded: &mut self.occluded,
                terminal: &mut terminal,
                #[cfg(not(windows))]
                master_fd: self.master_fd,
                #[cfg(not(windows))]
                shell_pid: self.shell_pid,
                preserve_title: self.preserve_title,
                vivid_service: &self.vivid_service,
                config: &self.config,
                event_proxy,
                #[cfg(target_os = "macos")]
                event_loop,
                clipboard,
                scheduler,
            };
            let mut processor = input::Processor::new(context);
            processor.set_modifier_override(modifier_override);
            processor.mouse_moved(points[0]);
            processor.mouse_input(ElementState::Pressed, button);
            for point in points.iter().skip(1) {
                processor.mouse_moved(*point);
            }
            processor.mouse_input(ElementState::Released, button);
        }
        Ok(notifier.into_bytes())
    }

    /// Encode one application mouse action without entering Vivido's UI input path.
    #[cfg(any(unix, windows))]
    pub fn application_mouse(&self, mouse: &IpcMouse) -> Result<Vec<u8>, IpcError> {
        let terminal = self.terminal.lock();
        let mode = *terminal.mode();
        if !mode.intersects(TermMode::MOUSE_MODE) {
            return Err(IpcError::new(
                "unsupported",
                "terminal application has not enabled mouse reporting",
            ));
        }
        if terminal.grid().display_offset() != 0 {
            return Err(IpcError::new(
                "invalid_state",
                "application mouse input requires the live-bottom viewport",
            ));
        }

        let (position, action) = match &mouse.action {
            IpcMouseAction::Move(position) => (position, MouseEncodingAction::Move),
            IpcMouseAction::Click(action) => {
                (&action.position, MouseEncodingAction::Click(action.button, 1))
            },
            IpcMouseAction::DoubleClick(action) => {
                (&action.position, MouseEncodingAction::Click(action.button, 2))
            },
            IpcMouseAction::Down(action) => {
                (&action.position, MouseEncodingAction::Down(action.button))
            },
            IpcMouseAction::Up(action) => {
                (&action.position, MouseEncodingAction::Up(action.button))
            },
            IpcMouseAction::Drag(action) => {
                (&action.position, MouseEncodingAction::Drag(action.button))
            },
            IpcMouseAction::Path(_) => {
                return Err(IpcError::new(
                    "invalid_params",
                    "mouse paths require the path input handler",
                ));
            },
            IpcMouseAction::Scroll(action) => {
                (&action.position, MouseEncodingAction::Scroll(action.vertical, action.horizontal))
            },
        };
        let modifiers = mouse_modifier_code(&position.mods)?;
        let position = self.resolve_mouse_position(position)?;
        let mut output = Vec::new();
        let button_code = |button| match button {
            IpcMouseButton::Left => 0,
            IpcMouseButton::Middle => 1,
            IpcMouseButton::Right => 2,
        };
        match action {
            MouseEncodingAction::Move => {
                if !mode.contains(TermMode::MOUSE_MOTION) {
                    return Err(IpcError::new(
                        "unsupported",
                        "terminal application has not enabled mouse motion reporting",
                    ));
                }
                append_mouse_report(&mut output, mode, position, 35 + modifiers, true)?;
            },
            MouseEncodingAction::Click(button, count) => {
                let code = button_code(button) + modifiers;
                for _ in 0..count {
                    append_mouse_report(&mut output, mode, position, code, true)?;
                    append_mouse_report(&mut output, mode, position, code, false)?;
                }
            },
            MouseEncodingAction::Down(button) => {
                append_mouse_report(
                    &mut output,
                    mode,
                    position,
                    button_code(button) + modifiers,
                    true,
                )?;
            },
            MouseEncodingAction::Up(button) => {
                append_mouse_report(
                    &mut output,
                    mode,
                    position,
                    button_code(button) + modifiers,
                    false,
                )?;
            },
            MouseEncodingAction::Drag(button) => {
                if !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
                    return Err(IpcError::new(
                        "unsupported",
                        "terminal application has not enabled mouse drag reporting",
                    ));
                }
                append_mouse_report(
                    &mut output,
                    mode,
                    position,
                    32 + button_code(button) + modifiers,
                    true,
                )?;
            },
            MouseEncodingAction::Scroll(vertical, horizontal) => {
                if !vertical.is_finite() || !horizontal.is_finite() {
                    return Err(IpcError::new("invalid_params", "scroll amounts must be finite"));
                }
                let vertical_count = vertical.abs().ceil() as usize;
                let horizontal_count = horizontal.abs().ceil() as usize;
                if vertical_count.checked_add(horizontal_count).is_none_or(|total| total > 1000) {
                    return Err(IpcError::new(
                        "limit_exceeded",
                        "one mouse scroll request is limited to 1000 reports",
                    ));
                }
                let vertical_code = if vertical >= 0.0 { 64 } else { 65 };
                let horizontal_code = if horizontal >= 0.0 { 66 } else { 67 };
                for _ in 0..vertical_count {
                    append_mouse_report(
                        &mut output,
                        mode,
                        position,
                        vertical_code + modifiers,
                        true,
                    )?;
                }
                for _ in 0..horizontal_count {
                    append_mouse_report(
                        &mut output,
                        mode,
                        position,
                        horizontal_code + modifiers,
                        true,
                    )?;
                }
            },
        }
        Ok(output)
    }

    /// Encode one complete physical-pixel application gesture into one PTY write.
    #[cfg(any(unix, windows))]
    pub fn application_mouse_path(&self, path: &IpcMousePath) -> Result<Vec<u8>, IpcError> {
        validate_mouse_path(path)?;
        let terminal = self.terminal.lock();
        let mode = *terminal.mode();
        if !mode.intersects(TermMode::MOUSE_MODE) {
            return Err(IpcError::new(
                "unsupported",
                "terminal application has not enabled mouse reporting",
            ));
        }
        if terminal.grid().display_offset() != 0 {
            return Err(IpcError::new(
                "invalid_state",
                "application mouse input requires the live-bottom viewport",
            ));
        }
        if !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
            return Err(IpcError::new(
                "unsupported",
                "terminal application has not enabled mouse drag reporting",
            ));
        }

        let positions = path
            .points
            .iter()
            .map(|point| self.resolve_pixel_point(*point))
            .collect::<Result<Vec<_>, _>>()?;
        let modifiers = mouse_modifier_code(&path.mods)?;
        let button = match path.button {
            IpcMouseButton::Left => 0,
            IpcMouseButton::Middle => 1,
            IpcMouseButton::Right => 2,
        };
        let mut output = Vec::with_capacity(positions.len().saturating_add(2) * 24);
        append_mouse_report(&mut output, mode, positions[0], button + modifiers, true)?;
        for position in positions.iter().copied().skip(1) {
            append_mouse_report(&mut output, mode, position, 32 + button + modifiers, true)?;
        }
        append_mouse_report(
            &mut output,
            mode,
            *positions.last().expect("validated path has at least two points"),
            button + modifiers,
            false,
        )?;
        Ok(output)
    }

    #[cfg(any(unix, windows))]
    fn resolve_mouse_position(
        &self,
        position: &IpcMousePosition,
    ) -> Result<ResolvedMousePosition, IpcError> {
        let size = self.display.size_info;
        match (
            position.cell_column,
            position.cell_row,
            position.x,
            position.y,
            position.relative_x,
            position.relative_y,
        ) {
            (Some(column), Some(row), None, None, None, None) => {
                let column = column as usize;
                let row = row as usize;
                if column >= size.columns() || row >= size.screen_lines() {
                    return Err(IpcError::new(
                        "invalid_params",
                        "mouse coordinate is outside the terminal grid",
                    ));
                }
                let x = (f64::from(size.padding_x())
                    + (column as f64 + 0.5) * f64::from(size.cell_width()))
                .min(f64::from(size.width()) - 1.0);
                let y = (f64::from(size.padding_y())
                    + (row as f64 + 0.5) * f64::from(size.cell_height()))
                .min(f64::from(size.height()) - 1.0);
                Ok(ResolvedMousePosition {
                    column,
                    row,
                    pixel_x: x.floor() as usize,
                    pixel_y: y.floor() as usize,
                    physical: PhysicalPosition::new(x, y),
                })
            },
            (None, None, Some(x), Some(y), None, None) if x.is_finite() && y.is_finite() => {
                if x < 0.0
                    || y < 0.0
                    || x >= f64::from(size.width())
                    || y >= f64::from(size.height())
                {
                    return Err(IpcError::new(
                        "invalid_params",
                        "mouse pixel coordinate is outside the client area",
                    ));
                }
                let column = ((x - f64::from(size.padding_x())).max(0.0)
                    / f64::from(size.cell_width())) as usize;
                let row = ((y - f64::from(size.padding_y())).max(0.0)
                    / f64::from(size.cell_height())) as usize;
                if column >= size.columns() || row >= size.screen_lines() {
                    return Err(IpcError::new(
                        "invalid_params",
                        "mouse coordinate is outside the terminal grid",
                    ));
                }
                Ok(ResolvedMousePosition {
                    column,
                    row,
                    pixel_x: x.floor() as usize,
                    pixel_y: y.floor() as usize,
                    physical: PhysicalPosition::new(x, y),
                })
            },
            (None, None, None, None, Some(relative_x), Some(relative_y))
                if relative_x.is_finite()
                    && relative_y.is_finite()
                    && (0.0..=1.0).contains(&relative_x)
                    && (0.0..=1.0).contains(&relative_y) =>
            {
                let x = relative_x * f64::from((size.width() - 1.0).max(0.0));
                let y = relative_y * f64::from((size.height() - 1.0).max(0.0));
                let column = ((x - f64::from(size.padding_x())).max(0.0)
                    / f64::from(size.cell_width())) as usize;
                let row = ((y - f64::from(size.padding_y())).max(0.0)
                    / f64::from(size.cell_height())) as usize;
                Ok(ResolvedMousePosition {
                    column: column.min(size.columns().saturating_sub(1)),
                    row: row.min(size.screen_lines().saturating_sub(1)),
                    pixel_x: x.floor() as usize,
                    pixel_y: y.floor() as usize,
                    physical: PhysicalPosition::new(x, y),
                })
            },
            _ => Err(IpcError::new(
                "invalid_params",
                "mouse requires exactly one cell, pixel, or relative coordinate pair",
            )),
        }
    }

    #[cfg(any(unix, windows))]
    fn resolve_pixel_point(&self, point: IpcMousePoint) -> Result<ResolvedMousePosition, IpcError> {
        self.resolve_mouse_position(&IpcMousePosition {
            x: Some(point.x),
            y: Some(point.y),
            ..IpcMousePosition::default()
        })
    }

    /// Send an explicit signal to the foreground process group, falling back to the child group.
    #[cfg(unix)]
    pub fn signal_process_group(&self, signal: IpcSignalName) -> Result<i32, IpcError> {
        let signal = match signal {
            IpcSignalName::Int => libc::SIGINT,
            IpcSignalName::Term => libc::SIGTERM,
            IpcSignalName::Hup => libc::SIGHUP,
            IpcSignalName::Quit => libc::SIGQUIT,
            IpcSignalName::Tstp => libc::SIGTSTP,
            IpcSignalName::Cont => libc::SIGCONT,
            IpcSignalName::Winch => libc::SIGWINCH,
            IpcSignalName::Kill => libc::SIGKILL,
            IpcSignalName::Stop => libc::SIGSTOP,
        };
        let foreground = unsafe { libc::tcgetpgrp(self.master_fd) };
        let process_group = if foreground > 0 { foreground } else { self.shell_pid as i32 };
        if unsafe { libc::killpg(process_group, signal) } == -1 {
            return Err(IpcError::new(
                "unsupported",
                format!("failed to signal process group: {}", std::io::Error::last_os_error()),
            ));
        }
        Ok(process_group)
    }

    /// Windows has no POSIX process group. Forceful termination is supported for the ConPTY
    /// child; console-signal delivery is unavailable because the child is attached to a
    /// pseudoconsole rather than the caller's console.
    #[cfg(windows)]
    pub fn signal_process_group(&self, signal: IpcSignalName) -> Result<i32, IpcError> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };

        if !matches!(signal, IpcSignalName::Term | IpcSignalName::Kill) {
            return Err(IpcError::new(
                "unsupported",
                "this signal cannot be delivered through a Windows pseudoconsole",
            ));
        }
        let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, self.shell_pid) };
        if process.is_null() {
            return Err(IpcError::new(
                "unsupported",
                format!("failed to open child process: {}", std::io::Error::last_os_error()),
            ));
        }
        let result = unsafe { TerminateProcess(process, 1) };
        unsafe { CloseHandle(process) };
        if result == 0 {
            return Err(IpcError::new(
                "unsupported",
                format!("failed to terminate child process: {}", std::io::Error::last_os_error()),
            ));
        }
        i32::try_from(self.shell_pid)
            .map_err(|_| IpcError::new("invalid_state", "child process ID is out of range"))
    }

    /// Request an exact client-area size.
    #[cfg(any(unix, windows))]
    pub fn request_automation_resize(
        &self,
        columns: Option<u16>,
        rows: Option<u16>,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<AutomationResize, IpcError> {
        let (width, height, grid) = match (columns, rows, width, height) {
            (Some(columns), Some(rows), None, None) if columns >= 2 && rows >= 1 => {
                let size = self.display.size_info;
                let width = f64::from(size.padding_x()) * 2.0
                    + f64::from(size.cell_width()) * f64::from(columns);
                let height = f64::from(size.padding_y()) * 2.0
                    + f64::from(size.cell_height()) * f64::from(rows);
                if width > f64::from(u32::MAX) || height > f64::from(u32::MAX) {
                    return Err(IpcError::new("limit_exceeded", "requested resize is too large"));
                }
                (width.ceil() as u32, height.ceil() as u32, Some((columns, rows)))
            },
            (None, None, Some(width), Some(height)) if width > 0 && height > 0 => {
                (width, height, None)
            },
            _ => {
                return Err(IpcError::new(
                    "invalid_params",
                    "resize requires either a valid grid pair or pixel pair",
                ));
            },
        };
        if !self.display.supports_render_size(width, height) {
            return Err(IpcError::new(
                "limit_exceeded",
                "requested resize exceeds the renderer texture limit",
            ));
        }
        let size = self.display.size_info;
        let available_width = (f64::from(width) - f64::from(size.padding_x()) * 2.0).max(0.0);
        let available_height = (f64::from(height) - f64::from(size.padding_y()) * 2.0).max(0.0);
        let actual_columns = (available_width / f64::from(size.cell_width())).floor();
        let actual_rows = (available_height / f64::from(size.cell_height())).floor();
        if actual_columns < 2.0
            || actual_rows < 1.0
            || actual_columns > f64::from(u16::MAX)
            || actual_rows > f64::from(u16::MAX)
        {
            return Err(IpcError::new(
                "invalid_params",
                "requested client size must produce a 2x1 through 65535x65535 PTY grid",
            ));
        }
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        if self.display.window.is_hosted() {
            self.event_proxy.send_event(EventType::ShellAction(
                crate::shell::ShellAction::Resize { width, height },
            ));
        } else {
            self.display.window.request_inner_size(winit::dpi::PhysicalSize::new(width, height));
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        self.display.window.request_inner_size(winit::dpi::PhysicalSize::new(width, height));
        Ok((width, height, grid))
    }

    #[cfg(any(unix, windows))]
    pub fn automation_size_matches(
        &self,
        columns: Option<u16>,
        rows: Option<u16>,
        width: u32,
        height: u32,
    ) -> bool {
        let size = self.display.size_info;
        let pixels = self.display.window.inner_size();
        let grid_matches = columns.is_none_or(|columns| {
            size.columns() == usize::from(columns)
                && rows.is_some_and(|rows| size.screen_lines() == usize::from(rows))
        });
        grid_matches && pixels.width == width && pixels.height == height
    }

    /// Ask the window system to activate this window.
    #[cfg(any(unix, windows))]
    pub fn request_automation_focus(&self) {
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        if self.display.window.is_hosted() {
            self.event_proxy
                .send_event(EventType::ShellAction(crate::shell::ShellAction::Activate));
        } else {
            self.display.window.focus_window();
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        self.display.window.focus_window();
    }

    /// Apply a bounded OSC notification request using the latest window visibility state.
    pub(crate) fn handle_desktop_notification(&mut self, notification: OscNotification) {
        let state = WindowNotificationState {
            focused: self.terminal.lock().is_focused,
            visible: self.display.window.is_visible() != Some(false),
            occluded: self.occluded,
            headless: self.display.window.is_headless(),
        };
        self.notifications.handle(notification, state, &self.notifier);
    }

    /// Focus the live originating native window after its notification is activated.
    pub(crate) fn activate_desktop_notification(&self) {
        if !self.display.window.is_headless() && !self.display.window.is_embedded() {
            self.display.window.focus_window();
        }
    }

    /// Move and optionally resize the window's outer frame.
    ///
    /// Position and size are applied without waiting for the windowing system to acknowledge
    /// either one: a caller driving a layout issues these continuously while dragging, and a
    /// per-request handshake would serialize the drag against the compositor. Callers that need
    /// confirmation subscribe to `moved` and `resized`.
    #[cfg(any(unix, windows))]
    pub fn request_automation_geometry(
        &self,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<Value, IpcError> {
        if self.display.window.is_headless() && (x.is_some() || y.is_some()) {
            return Err(IpcError::new(
                "unsupported",
                "a headless window has no screen to be positioned on",
            ));
        }

        match (width, height) {
            (Some(width), Some(height)) => {
                self.request_automation_resize(None, None, Some(width), Some(height))?;
            },
            (None, None) => (),
            _ => {
                return Err(IpcError::new(
                    "invalid_params",
                    "set_geometry requires both width and height, or neither",
                ));
            },
        }

        match (x, y) {
            (Some(x), Some(y)) => {
                if self.display.window.is_hosted() {
                    self.event_proxy.send_event(EventType::ShellAction(
                        crate::shell::ShellAction::SetPosition { x, y },
                    ));
                } else {
                    self.display.window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
                }
            },
            (None, None) => (),
            _ => {
                return Err(IpcError::new(
                    "invalid_params",
                    "set_geometry requires both x and y, or neither",
                ));
            },
        }

        if x.is_none() && width.is_none() {
            return Err(IpcError::new(
                "invalid_params",
                "set_geometry requires a position, a size, or both",
            ));
        }

        let pixels = self.display.window.inner_size();
        let position = self.display.window.outer_position();
        Ok(json_value!({
            "x": position.map(|position| position.x),
            "y": position.map(|position| position.y),
            "width": pixels.width,
            "height": pixels.height,
        }))
    }

    /// Map or unmap the window.
    ///
    /// Mapping deliberately does not take the keyboard: an external layout owner reveals a pane
    /// while its own window stays key.
    #[cfg(any(unix, windows))]
    pub fn request_automation_visible(&mut self, visible: bool) {
        if self.display.window.is_hosted() {
            self.event_proxy
                .send_event(EventType::ShellAction(crate::shell::ShellAction::SetVisible(visible)));
            return;
        }
        self.set_automation_visible(visible);
    }

    /// Apply visibility selected by this window's native or embedded host.
    #[cfg(any(unix, windows))]
    pub fn set_automation_visible(&mut self, visible: bool) {
        #[cfg(target_os = "macos")]
        let mapped_without_focus = visible && !self.display.window.is_headless();
        #[cfg(not(target_os = "macos"))]
        let mapped_without_focus = false;

        if mapped_without_focus {
            #[cfg(target_os = "macos")]
            self.display.window.order_front_without_focus();
        } else {
            self.display.window.set_visible(visible);
        }

        // A window an external layout owner just revealed is on screen now, but an automation show
        // carries no focus, resize, or occlusion event to mark it dirty — and `draw` suppresses a
        // frame entirely while the window is still flagged occluded (it was created behind the
        // host). Clear that flag and ask for the one frame, so a revealed pane paints on show
        // rather than only on the first click into it.
        if visible && !self.display.window.is_headless() {
            self.occluded = false;
            self.dirty = true;
            self.display.window.request_redraw();
        }
    }

    /// Set the window's stacking level.
    #[cfg(any(unix, windows))]
    pub fn set_automation_level(&self, level: crate::cli::IpcWindowLevel) {
        let level = level.into();
        if self.display.window.is_hosted() {
            self.event_proxy
                .send_event(EventType::ShellAction(crate::shell::ShellAction::SetLevel(level)));
        } else {
            self.display.window.set_window_level(level);
        }
    }

    /// Coalesce terminal-model mutations into one semantic screen sequence change.
    ///
    /// This runs on every event-loop turn, so it hashes cells directly out of the grid. Feeding the
    /// hasher through `format!` instead cost three heap allocations per cell, which during heavy
    /// output meant tens of thousands of allocations per turn while holding the terminal lock
    /// against the PTY reader.
    #[cfg(any(unix, windows))]
    pub fn sync_automation_screen(&mut self) -> Option<(u64, Option<Vec<u16>>)> {
        let terminal = self.terminal.lock();
        let grid = terminal.grid();
        let rows = terminal.screen_lines();
        let columns = terminal.columns();
        let display_offset = grid.display_offset();
        let selection =
            terminal.selection.as_ref().and_then(|selection| selection.to_range(&terminal));
        let cursor = grid.cursor.point;

        let mut metadata = DefaultHasher::new();
        rows.hash(&mut metadata);
        columns.hash(&mut metadata);
        display_offset.hash(&mut metadata);
        (terminal.mode().bits() & !TermMode::URGENCY_HINTS.bits()).hash(&mut metadata);
        let metadata = metadata.finish();

        let mut row_hashes = Vec::with_capacity(rows);
        for viewport_row in 0..rows {
            let line = Line(viewport_row as i32 - display_offset as i32);
            let mut hasher = DefaultHasher::new();
            for column in 0..columns {
                let point = crate::terminal::index::Point::new(line, Column(column));
                let cell = &grid[point];
                cell.c.hash(&mut hasher);
                cell.flags.bits().hash(&mut hasher);
                hash_color(Some(cell.fg), &mut hasher);
                hash_color(Some(cell.bg), &mut hasher);
                hash_color(cell.underline_color(), &mut hasher);
                cell.zerowidth().unwrap_or_default().hash(&mut hasher);
                if let Some(link) = cell.hyperlink() {
                    link.id().hash(&mut hasher);
                    link.uri().hash(&mut hasher);
                }
                (point == cursor).hash(&mut hasher);
                selection.is_some_and(|selection| selection.contains(point)).hash(&mut hasher);
                cell.flags.contains(Flags::WRAPLINE).hash(&mut hasher);
            }
            row_hashes.push(hasher.finish());
        }
        drop(terminal);

        let first = self.automation.row_hashes.is_empty();
        let full = first || self.automation.screen_metadata_hash != metadata;
        let changed_rows: Vec<u16> = self
            .automation
            .row_hashes
            .iter()
            .zip(&row_hashes)
            .enumerate()
            .filter_map(|(row, (old, new))| (old != new).then_some(row as u16))
            .collect();
        if !full && changed_rows.is_empty() {
            return None;
        }

        self.automation.row_hashes = row_hashes;
        self.automation.screen_metadata_hash = metadata;
        let rows = (!full).then_some(changed_rows);
        let sequence = self.automation.record_screen_change(rows.clone());
        Some((sequence, rows))
    }

    /// Publish a coalesced read-only accessibility snapshot.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    pub fn sync_accessibility(&mut self) {
        if !terminal_document_enabled() {
            return;
        }
        let Some(accessibility) = &mut self.accessibility else { return };

        // A retained terminal document carries per-cell text geometry for the entire scrollback.
        // On Windows, rebuilding it without a UI Automation client made every event-loop turn
        // proportional to the history size. AccessKit's Windows adapter also does not report
        // deactivation, so a hidden tab that was inspected once must be excluded explicitly.
        #[cfg(windows)]
        if !accessibility.should_sync(self.display.window.is_visible() != Some(false)) {
            return;
        }

        let terminal = self.terminal.lock();
        let snapshot = AccessibilitySnapshot::new(
            &terminal,
            self.display.size_info,
            self.display.window.title(),
        );
        drop(terminal);
        accessibility.update(snapshot);
    }

    /// Summary used by deterministic window discovery.
    #[cfg(any(unix, windows))]
    pub fn automation_summary(&self) -> Value {
        let terminal = self.terminal.lock();
        self.automation_summary_with_terminal(&terminal)
    }

    #[cfg(any(unix, windows))]
    fn automation_summary_with_terminal(&self, terminal: &Term<EventProxy>) -> Value {
        let size = self.display.size_info;
        let pixels = self.display.window.inner_size();
        let position = self.display.window.outer_position();
        json_value!({
            "window_id": self.ipc_window_id,
            "creation_index": self.automation.creation_index,
            "title": self.display.window.title(),
            "focused": terminal.is_focused,
            "occluded": self.occluded,
            "visible": self.display.window.is_visible(),
            "hold": self.display.window.hold,
            "grid": {"columns": size.columns(), "rows": size.screen_lines()},
            "pixels": {"width": pixels.width, "height": pixels.height},
            // Where the grid starts inside the client area. Not derivable from the values above:
            // with `dynamic_padding` off (the default) the sub-cell remainder collects at the
            // right and bottom rather than being split, so the obvious
            // `(width - columns * cell_width) / 2` over-estimates it by half the remainder.
            "padding": {"x": size.padding_x(), "y": size.padding_y()},
            "position": position.map(|position| json_value!({"x": position.x, "y": position.y})),
            "process": exit_status_json(self.automation.exit_status.as_ref()),
            "client_health": self.client_health.as_str(),
            "last_client_fault": self.last_client_fault.as_ref().map(client_fault_json),
            "sequences": {
                "screen": self.automation.screen_sequence,
                "frame": self.automation.frame_sequence,
                "output": self.automation.transcript.lock().unwrap().end_offset(),
            },
        })
    }

    /// Detailed, secret-free terminal/window inspection.
    #[cfg(any(unix, windows))]
    pub fn automation_inspect(&self, event_sequence: u64) -> Value {
        let terminal = self.terminal.lock();
        let grid = terminal.grid();
        let size = self.display.size_info;
        let cursor = grid.cursor.point;
        let selection =
            terminal.selection.as_ref().and_then(|selection| selection.to_range(&terminal));
        #[cfg(unix)]
        let foreground_pgid = unsafe { libc::tcgetpgrp(self.master_fd) };
        #[cfg(unix)]
        let foreground_pgid = (foreground_pgid > 0).then_some(foreground_pgid);
        #[cfg(windows)]
        let foreground_pgid = None::<i32>;
        #[cfg(unix)]
        let executable = foreground_pgid.and_then(foreground_executable_basename);
        #[cfg(windows)]
        let executable = None::<String>;
        // The terminal lock is already held here and not reentrant, so this resolves the same
        // preference as [`Self::current_directory`] inline instead of calling it.
        let current_directory = terminal
            .working_directory()
            .map(PathBuf::from)
            .or_else(|| self.probed_working_directory())
            .map(|path| path.to_string_lossy().into_owned());
        #[cfg(unix)]
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
        #[cfg(unix)]
        let echo = unsafe {
            (libc::tcgetattr(self.master_fd, attributes.as_mut_ptr()) == 0)
                .then(|| attributes.assume_init().c_lflag & libc::ECHO != 0)
        };
        #[cfg(windows)]
        let echo = None::<bool>;
        let (text_scene_builds, cached_scene_frames, media_metrics) =
            self.display.optimization_metrics();

        json_value!({
            "window": self.automation_summary_with_terminal(&terminal),
            "cell": {"width": size.cell_width(), "height": size.cell_height()},
            "scale_factor": self.display.window.scale_factor,
            "scrollback_size": grid.history_size(),
            "display_offset": grid.display_offset(),
            "screen": if terminal.mode().contains(TermMode::ALT_SCREEN) { "alternate" } else { "primary" },
            "terminal_modes": terminal_mode_names(*terminal.mode()),
            "cursor": {"line": cursor.line.0, "column": cursor.column.0},
            "selection": selection.map(selection_json),
            "shell_pid": self.shell_pid,
            "foreground_process_group_id": foreground_pgid,
            "executable": executable,
            "current_directory": current_directory,
            "echo": echo,
            "exit_status": exit_status_json(self.automation.exit_status.as_ref()),
            "event_sequence": event_sequence,
            "client_health": self.client_health.as_str(),
            "last_client_fault": self.last_client_fault.as_ref().map(client_fault_json),
            "vivid_streaming": self.vivid_service.automation_streaming_metrics(),
            "render_optimization": {
                "text_scene_builds": text_scene_builds,
                "cached_scene_frames": cached_scene_frames,
                "media_passes": media_metrics.media_passes,
                "media_skipped_passes": media_metrics.skipped_passes,
                "uploaded_frames": media_metrics.frames,
                "uploaded_pixels": media_metrics.uploaded_pixels,
                "full_frame_pixels": media_metrics.full_frame_pixels,
            },
            "limits": {
                "transcript_bytes": crate::automation::TRANSCRIPT_CAPACITY,
                "screen_history": crate::automation::SCREEN_HISTORY_COUNT,
                "grid_rows": 1000,
                "reply_bytes": crate::polling::ipc::MAX_REPLY_FRAME_BYTES,
            },
        })
    }

    #[cfg(any(unix, windows))]
    pub fn automation_vivid_sessions(&self) -> Value {
        let sessions = self
            .vivid_service
            .automation_sessions()
            .into_iter()
            .map(|identity| {
                json_value!({
                    "session_id": identity.session_id,
                    "presenter_instance_id": hex_bytes(&identity.presenter.0),
                })
            })
            .collect::<Vec<_>>();
        json_value!({"window_id": self.ipc_window_id, "sessions": sessions})
    }

    #[cfg(any(unix, windows))]
    pub fn automation_vivid_surfaces(&self) -> Value {
        let surfaces = self
            .vivid_service
            .automation_surface_keys()
            .into_iter()
            .filter_map(|identity| {
                self.vivid_service
                    .automation_surface_status(identity)
                    .map(|status| vivid_surface_status_json(self.ipc_window_id, &status))
            })
            .collect::<Vec<_>>();
        json_value!({"window_id": self.ipc_window_id, "surfaces": surfaces})
    }

    #[cfg(any(unix, windows))]
    pub fn automation_vivid_surface(
        &self,
        session_id: u64,
        context_id: u64,
        surface_id: u64,
    ) -> Result<Value, IpcError> {
        self.vivid_service
            .automation_surface_keys()
            .into_iter()
            .find(|identity| {
                identity.context.session.session_id == session_id
                    && identity.context.context_id == context_id
                    && identity.surface_id == surface_id
            })
            .and_then(|identity| self.vivid_service.automation_surface_status(identity))
            .map(|status| vivid_surface_status_json(self.ipc_window_id, &status))
            .ok_or_else(|| IpcError::new("surface_not_found", "Vivid surface does not exist"))
    }

    #[cfg(any(unix, windows))]
    pub fn automation_vivid_tracks(&self) -> Value {
        let tracks = self
            .vivid_service
            .automation_track_keys()
            .into_iter()
            .filter_map(|identity| {
                self.vivid_service
                    .automation_track_status(identity)
                    .map(|status| vivid_track_status_json(self.ipc_window_id, &status))
            })
            .collect::<Vec<_>>();
        json_value!({"window_id": self.ipc_window_id, "tracks": tracks})
    }

    #[cfg(any(unix, windows))]
    pub fn automation_vivid_track(
        &self,
        session_id: u64,
        context_id: u64,
        surface_id: u64,
        track_id: u64,
    ) -> Result<Value, IpcError> {
        self.vivid_service
            .automation_track_keys()
            .into_iter()
            .find(|identity| {
                identity.surface.context.session.session_id == session_id
                    && identity.surface.context.context_id == context_id
                    && identity.surface.surface_id == surface_id
                    && identity.track_id == track_id
            })
            .and_then(|identity| self.vivid_service.automation_track_status(identity))
            .map(|status| vivid_track_status_json(self.ipc_window_id, &status))
            .ok_or_else(|| IpcError::new("track_not_found", "Vivid track does not exist"))
    }

    #[cfg(any(unix, windows))]
    pub fn automation_vivid_scene(
        &self,
        session_id: u64,
        maximum_nodes: u64,
    ) -> Result<Value, IpcError> {
        if maximum_nodes == 0 || maximum_nodes > 256 {
            return Err(IpcError::new("invalid_params", "maximum_nodes must be 1 through 256"));
        }
        let session = self
            .vivid_service
            .automation_sessions()
            .into_iter()
            .find(|identity| identity.session_id == session_id)
            .ok_or_else(|| IpcError::new("session_not_found", "Vivid session does not exist"))?;
        let status = self.vivid_service.automation_scene_status(session, maximum_nodes);
        let nodes = status
            .nodes
            .into_iter()
            .map(|node| {
                json_value!({
                    "presenter_instance_id": hex_bytes(&node.identity.context.session.presenter.0),
                    "node_id": node.node.node_id,
                    "owning_context_id": node.node.owning_context_id,
                    "surface_context_id": node.node.surface_context_id,
                    "surface_id": node.node.surface_id,
                    "geometry": cbor_map_json(&node.node.geometry),
                    "z_index": node.node.z_index,
                    "visible": node.node.visible,
                    "opacity": node.node.opacity,
                    "clip": node.node.clip.as_ref().map(|clip| cbor_map_json(clip)),
                })
            })
            .collect::<Vec<_>>();
        Ok(json_value!({
            "window_id": self.ipc_window_id,
            "presenter_instance_id": hex_bytes(&status.session.presenter.0),
            "session_id": session_id,
            "scene_revision": status.revision.get(),
            "target_generation": status.target_generation.get(),
            "total_nodes": nodes.len(),
            "nodes": nodes,
            "truncated": false,
        }))
    }

    #[cfg(any(unix, windows))]
    pub fn automation_vivid_trace(
        &self,
        selection: crate::vivid::trace::TraceSelection,
        limit: u16,
        filter: crate::vivid::trace::TraceFilter,
    ) -> Value {
        serde_json::to_value(self.vivid_service.automation_trace(selection, limit, filter))
            .unwrap_or_else(|_| json_value!({"schema_version": 1, "events": []}))
    }

    #[cfg(any(unix, windows))]
    pub fn automation_diagnose(&self, event_sequence: u64, trace_limit: u16) -> Value {
        let sessions = self.automation_vivid_sessions();
        let surfaces = self.automation_vivid_surfaces();
        let tracks = self.automation_vivid_tracks();
        let scenes = self
            .vivid_service
            .automation_sessions()
            .into_iter()
            .filter_map(|identity| {
                self.automation_vivid_scene(
                    identity.session_id,
                    crate::vivid::MAX_SCENE_NODES as u64,
                )
                .ok()
            })
            .collect::<Vec<_>>();
        json_value!({
            "schema_version": 1,
            "capture": {
                "event_sequence": event_sequence,
                "screen_sequence": self.automation.screen_sequence,
                "frame_sequence": self.automation.frame_sequence,
            },
            "window": self.automation_inspect(event_sequence),
            "renderer": {
                "frame_sequence": self.automation.frame_sequence,
                "has_presented_frame": self.automation.frame_sequence != 0,
                "headless": self.display.window.is_headless(),
            },
            "presenter": {
                "sessions": sessions.get("sessions").cloned().unwrap_or_else(|| json_value!([])),
                "surfaces": surfaces.get("surfaces").cloned().unwrap_or_else(|| json_value!([])),
                "tracks": tracks.get("tracks").cloned().unwrap_or_else(|| json_value!([])),
                "scenes": scenes,
                "streaming": self.vivid_service.automation_streaming_metrics(),
                "trace": self.automation_vivid_trace(
                    crate::vivid::trace::TraceSelection::Tail,
                    trace_limit,
                    crate::vivid::trace::TraceFilter::default(),
                ),
            },
        })
    }

    #[cfg(any(unix, windows))]
    #[allow(clippy::too_many_arguments)]
    pub fn automation_vivid_wait(
        &self,
        session_id: u64,
        context_id: u64,
        surface_id: u64,
        track_id: u64,
        channel_generation: u64,
        condition: u64,
        value: Option<u64>,
    ) -> TrackWaitEvaluation {
        let identity = self.vivid_service.automation_track_keys().into_iter().find(|identity| {
            identity.surface.context.session.session_id == session_id
                && identity.surface.context.context_id == context_id
                && identity.surface.surface_id == surface_id
                && identity.track_id == track_id
        });
        let Some(identity) = identity else {
            return TrackWaitEvaluation::NotFound;
        };
        self.vivid_service.automation_evaluate_wait(
            identity,
            vivid_protocol::revision::ChannelGeneration::new(channel_generation),
            condition,
            value,
        )
    }

    /// Structured physical-cell grid snapshot or current-state delta.
    #[cfg(any(unix, windows))]
    pub fn automation_grid(
        &self,
        start_line: Option<i32>,
        row_count: Option<u16>,
        since_screen: Option<u64>,
    ) -> Result<Value, IpcError> {
        if start_line.is_some() != row_count.is_some() {
            return Err(IpcError::new(
                "invalid_params",
                "start_line and row_count must be specified together",
            ));
        }
        if start_line.is_some() && since_screen.is_some() {
            return Err(IpcError::new(
                "invalid_params",
                "scrollback ranges and since_screen are mutually exclusive",
            ));
        }

        let terminal = self.terminal.lock();
        let grid = terminal.grid();
        let screen_lines = terminal.screen_lines();
        let columns = terminal.columns();
        let viewport_start = -(grid.display_offset() as i32);
        let viewport_end = viewport_start + screen_lines as i32 - 1;
        let top = grid.topmost_line().0;
        let bottom = grid.bottommost_line().0;

        let mut full = since_screen.is_none();
        let mut gap = None;
        let mut viewport_rows = None;
        if let Some(since) = since_screen {
            if since > self.automation.screen_sequence {
                return Err(IpcError::new(
                    "invalid_params",
                    format!("screen sequence {since} is in the future"),
                ));
            }
            let oldest = self
                .automation
                .screen_history
                .front()
                .map_or(self.automation.screen_sequence, |change| change.sequence);
            if since < oldest.saturating_sub(1) {
                full = true;
                gap = Some(json_value!({
                    "requested_sequence": since,
                    "oldest_sequence": oldest,
                    "current_sequence": self.automation.screen_sequence,
                }));
            } else {
                let mut changed = std::collections::BTreeSet::new();
                for change in
                    self.automation.screen_history.iter().filter(|change| change.sequence > since)
                {
                    match &change.rows {
                        Some(rows) => changed.extend(rows.iter().copied()),
                        None => {
                            full = true;
                            break;
                        },
                    }
                }
                if !full {
                    viewport_rows = Some(changed.into_iter().collect::<Vec<_>>());
                }
            }
        }

        let lines: Vec<i32> = if let (Some(start), Some(count)) = (start_line, row_count) {
            if count == 0 || count > 1000 {
                return Err(IpcError::new("invalid_params", "row_count must be 1 through 1000"));
            }
            let end = start
                .checked_add(i32::from(count) - 1)
                .ok_or_else(|| IpcError::new("invalid_params", "grid line range overflows"))?;
            if start < top || end > bottom {
                return Err(IpcError::new(
                    "invalid_params",
                    format!("grid range must be within {top}..={bottom}"),
                ));
            }
            (start..=end).collect()
        } else if let Some(rows) = viewport_rows {
            rows.into_iter().map(|row| viewport_start + i32::from(row)).collect()
        } else {
            (viewport_start..=viewport_end).collect()
        };

        let selection =
            terminal.selection.as_ref().and_then(|selection| selection.to_range(&terminal));
        let cursor = grid.cursor.point;
        let mut styles = Vec::<Value>::new();
        let mut style_ids = std::collections::HashMap::<String, u32>::new();
        let mut rows = Vec::with_capacity(lines.len());

        for line in &lines {
            let line = Line(*line);
            let mut cells = Vec::with_capacity(columns);
            for column in 0..columns {
                let cell = &grid[line][Column(column)];
                let style = self.cell_style(cell);
                let key = serde_json::to_string(&style).map_err(|error| {
                    IpcError::new("unsupported", format!("style serialization failed: {error}"))
                })?;
                let style_id = match style_ids.get(&key) {
                    Some(style_id) => *style_id,
                    None => {
                        let style_id = styles.len() as u32;
                        styles.push(style);
                        style_ids.insert(key, style_id);
                        style_id
                    },
                };
                let (width, kind) = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    (0, "continuation")
                } else if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    (0, "leading_wide_spacer")
                } else if cell.flags.contains(Flags::WIDE_CHAR) {
                    (2, "character")
                } else {
                    (1, "character")
                };
                let mut text = if width == 0 { String::new() } else { cell.c.to_string() };
                text.extend(cell.zerowidth().into_iter().flatten());
                cells.push(json_value!({
                    "text": text,
                    "width": width,
                    "kind": kind,
                    "style": style_id,
                }));
            }
            let viewport_row = (line.0 >= viewport_start && line.0 <= viewport_end)
                .then_some(line.0 - viewport_start);
            let wrapped =
                columns > 0 && grid[line][Column(columns - 1)].flags.contains(Flags::WRAPLINE);
            rows.push(json_value!({
                "grid_line": line.0,
                "viewport_row": viewport_row,
                "wrapped": wrapped,
                "cells": cells,
            }));
        }

        let result = json_value!({
            "window_id": self.ipc_window_id,
            "screen_sequence": self.automation.screen_sequence,
            "full": full,
            "gap": gap,
            "grid": {"columns": columns, "rows": screen_lines},
            "returned_lines": {
                "start": lines.first(),
                "end": lines.last(),
            },
            "history_size": grid.history_size(),
            "display_offset": grid.display_offset(),
            "cursor": {"line": cursor.line.0, "column": cursor.column.0},
            "selection": selection.map(selection_json),
            "screen": if terminal.mode().contains(TermMode::ALT_SCREEN) { "alternate" } else { "primary" },
            "terminal_modes": terminal_mode_names(*terminal.mode()),
            "styles": styles,
            "rows": rows,
        });
        let encoded = serde_json::to_vec(&result).map_err(|error| {
            IpcError::new("unsupported", format!("grid serialization failed: {error}"))
        })?;
        if encoded.len() > crate::polling::ipc::MAX_REPLY_FRAME_BYTES {
            Err(IpcError::new("limit_exceeded", "encoded grid reply exceeds 16 MiB"))
        } else {
            Ok(result)
        }
    }

    #[cfg(any(unix, windows))]
    fn cell_style(&self, cell: &crate::terminal::term::cell::Cell) -> Value {
        let mut foreground =
            resolve_color(&self.display.colors, cell.fg, cell.flags, true, &self.config);
        let mut background =
            resolve_color(&self.display.colors, cell.bg, cell.flags, false, &self.config);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut foreground, &mut background);
        }
        let underline = cell.underline_color().map(|color| {
            resolve_color(&self.display.colors, color, cell.flags, true, &self.config)
        });
        let background_alpha = if cell.bg == Color::Named(NamedColor::Background) {
            (self.config.window_opacity() * 255.0).round() as u8
        } else {
            255
        };
        json_value!({
            "foreground": [foreground.r, foreground.g, foreground.b, 255],
            "background": [background.r, background.g, background.b, background_alpha],
            "underline_color": underline.map(|color| [color.r, color.g, color.b, 255]),
            "attributes": style_attribute_names(cell.flags),
            "hyperlink": cell.hyperlink().map(|link| json_value!({"id": link.id(), "uri": link.uri()})),
        })
    }

    /// Start reading back the last successfully presented frame.
    #[cfg(any(unix, windows))]
    pub fn request_screenshot(
        &mut self,
        connection: IpcConnection,
        request_id: u64,
        scheduler: &mut Scheduler,
    ) -> Result<(), String> {
        if self.screenshot_busy {
            return Err(String::from("a screenshot is already in progress for this window"));
        }

        // A headless window has no compositor asking it to paint, so the retained frame may not
        // exist yet or may predate the terminal state the caller wants captured. Paint first so a
        // screenshot always reflects what `get-text` would report at the same moment.
        if self.display.window.is_headless() && (self.dirty || !self.display.has_rendered_frame()) {
            self.draw(scheduler);
        }

        let readback = self.display.begin_screenshot().map_err(|err| err.to_string())?;
        let pixels = self.display.window.inner_size();
        let size = self.display.size_info;
        let metadata = serde_json::json!({
            "window_id": self.ipc_window_id(),
            "frame_sequence": self.automation.frame_sequence,
            "width": pixels.width,
            "height": pixels.height,
            "scale_factor": self.display.window.scale_factor,
            "cell": {"width": size.cell_width(), "height": size.cell_height()},
            // Origin of the terminal grid within this capture. See the note in
            // `automation_summary_with_terminal`: it cannot be derived from width/height/cell.
            "padding": {"x": size.padding_x(), "y": size.padding_y()},
        });
        self.screenshot = Some(PendingScreenshot { readback, connection, request_id, metadata });
        self.screenshot_busy = true;

        let window_id = self.id();
        let timer_id = TimerId::new(Topic::ScreenshotReadback, window_id);
        let event = Event::new(EventType::ScreenshotReadback, window_id);
        scheduler.schedule(event, SCREENSHOT_POLL_INTERVAL, true, timer_id);
        Ok(())
    }

    /// Poll screenshot readback and move PNG encoding off the event-loop thread.
    #[cfg(any(unix, windows))]
    pub fn poll_screenshot(&mut self, scheduler: &mut Scheduler, event_proxy: &EventSink) {
        let Some(pending) = self.screenshot.as_ref() else {
            return;
        };

        let result = if pending.readback.started.elapsed() >= SCREENSHOT_READBACK_TIMEOUT {
            Err(String::from("screenshot readback timed out"))
        } else {
            match self.display.poll_screenshot(&pending.readback) {
                Ok(Some(pixels)) => Ok(Some(pixels)),
                Ok(None) => Ok(None),
                Err(err) => Err(err.to_string()),
            }
        };

        let completed = match result {
            Ok(None) => return,
            Ok(Some(pixels)) => {
                let pending = self.screenshot.take().unwrap();
                let proxy = event_proxy.clone();
                let window_id = self.id();
                thread::spawn_named("screenshot encoder", move || {
                    match screenshot::save(pixels) {
                        Ok(path) => match path.to_str() {
                            Some(path) => {
                                let mut result = pending.metadata;
                                result["path"] = serde_json::Value::String(path.to_owned());
                                pending.connection.reply(pending.request_id, result);
                            },
                            None => pending.connection.error(
                                pending.request_id,
                                IpcError::new(
                                    "unsupported",
                                    "temporary screenshot path is not valid UTF-8",
                                ),
                            ),
                        },
                        Err(err) => pending.connection.error(
                            pending.request_id,
                            IpcError::new(
                                "unsupported",
                                format!("failed to save screenshot: {err}"),
                            ),
                        ),
                    }
                    let _ = proxy.send_event(Event::new(EventType::ScreenshotComplete, window_id));
                });
                true
            },
            Err(message) => {
                if let Some(pending) = self.screenshot.take() {
                    pending
                        .connection
                        .error(pending.request_id, IpcError::new("unsupported", message));
                }
                self.screenshot_busy = false;
                true
            },
        };

        if completed {
            scheduler.unschedule(TimerId::new(Topic::ScreenshotReadback, self.id()));
        }
    }

    /// Allow another screenshot after background PNG persistence completes.
    #[cfg(any(unix, windows))]
    pub fn complete_screenshot(&mut self) {
        self.screenshot_busy = false;
    }

    /// Forget asynchronous work owned by a disconnected IPC client.
    #[cfg(any(unix, windows))]
    pub fn cancel_automation_connection(&mut self, connection_id: u64) -> bool {
        self.automation.pending_writes.retain(|pending| pending.connection.id() != connection_id);
        self.automation.waiters.retain(|waiter| waiter.connection.id() != connection_id);
        if self.screenshot.as_ref().is_some_and(|pending| pending.connection.id() == connection_id)
        {
            self.screenshot = None;
            self.screenshot_busy = false;
            true
        } else {
            false
        }
    }

    /// Complete every outstanding IPC operation before the window disappears.
    #[cfg(any(unix, windows))]
    pub fn fail_automation_requests(&mut self, code: &str, message: &str) {
        for pending in self.automation.pending_writes.drain(..) {
            pending.connection.error(pending.request_id, IpcError::new(code, message));
        }
        for waiter in self.automation.waiters.drain(..) {
            waiter.connection.error(waiter.request_id, IpcError::new(code, message));
        }
        if let Some(pending) = self.screenshot.take() {
            pending.connection.error(pending.request_id, IpcError::new(code, message));
        }
        self.screenshot_busy = false;
    }

    /// Write the ref test results to the disk.
    pub fn write_ref_test_results(&self) {
        // Dump grid state.
        let mut grid = self.terminal.lock().grid().clone();
        grid.initialize_all();
        grid.truncate();

        let serialized_grid = json::to_string(&grid).expect("serialize grid");

        let size_info = &self.display.size_info;
        let size = TermSize::new(size_info.columns(), size_info.screen_lines());
        let serialized_size = json::to_string(&size).expect("serialize size");

        let serialized_config = format!("{{\"history_size\":{}}}", grid.history_size());

        File::create("./grid.json")
            .and_then(|mut f| f.write_all(serialized_grid.as_bytes()))
            .expect("write grid.json");

        File::create("./size.json")
            .and_then(|mut f| f.write_all(serialized_size.as_bytes()))
            .expect("write size.json");

        File::create("./config.json")
            .and_then(|mut f| f.write_all(serialized_config.as_bytes()))
            .expect("write config.json");
    }
}

fn configure_vivid_pty_environment(
    environment: &mut std::collections::HashMap<String, String>,
    control_endpoint: &str,
    root_secret: &str,
    window_id: u64,
) {
    environment.insert("VIVID_ENDPOINT_CONTROL".into(), control_endpoint.into());
    environment.insert("VIVID_ROOT_SECRET".into(), root_secret.into());
    environment.insert("VIVIDO_WINDOW_ID".into(), window_id.to_string());
    // Ambient agent-mesh coordinates, so an agent started in this window can address agents in
    // other windows and other runtimes. Vivido links no mesh crate and opens no store; these three
    // strings are the whole integration. A window id survives being moved, which is what makes it
    // the addressable part (`w`) rather than any position in a tab strip.
    environment.insert("AGENT_MESH_RUNTIME".into(), crate::session::runtime_kind().into());
    if let Some(instance) = crate::session::instance_name() {
        environment.insert("AGENT_MESH_INSTANCE".into(), instance.into());
    }
    // An address index is a one-based `u32`. Ids this process assigns always fit, but a caller may
    // name any `u64` with `--ipc-window-id`. Publish nothing rather than an address that cannot
    // parse: a window with no position still binds and is still reachable by alias, whereas an
    // unparsable address fails the bind outright and takes the whole mailbox with it.
    if (1..=u64::from(u32::MAX)).contains(&window_id) {
        environment.insert("AGENT_MESH_ADDRESS".into(), format!("w{window_id}"));
    }
    environment.insert(
        "VIVIDO_INPUT_TRANSPORT".into(),
        if cfg!(windows) { "win32-console" } else { "pty-bytes" }.into(),
    );
    // ConPTY strips APC control strings before they reach Vivido's terminal parser. Producers
    // must therefore emit the bounded printable marker form that the Windows PTY scanner removes
    // and authenticates before ordinary terminal parsing.
    #[cfg(windows)]
    {
        environment.insert("VIVID_ANCHOR_TRANSPORT".into(), "conpty".into());

        // WSL only imports arbitrary Windows environment variables named by WSLENV. Use `/u` so
        // these per-window discovery values flow from Win32 into WSL, but are not exported back to
        // every Windows process subsequently launched from Linux. The latter matters especially
        // for the root secret. Preserve unrelated user entries while taking ownership of the
        // exact uppercase names Vivido supplies.
        let inherited = environment
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("WSLENV"))
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var("WSLENV").ok())
            .unwrap_or_default();
        environment.retain(|name, _| !name.eq_ignore_ascii_case("WSLENV"));
        environment.insert("WSLENV".into(), vivid_wslenv(&inherited));
    }
}

#[cfg(windows)]
fn vivid_wslenv(inherited: &str) -> String {
    // Remove every endpoint name Vivido owns. In particular, retaining an old optional lane in
    // WSLENV makes WSL create that variable with an empty value when this window does not offer
    // the lane. Producers correctly reject a present-but-empty endpoint as malformed instead of
    // applying the missing-lane fallback.
    const MANAGED: [&str; 8] = [
        "VIVID_ENDPOINT_CONTROL",
        "VIVID_ENDPOINT_INTERACTIVE",
        "VIVID_ENDPOINT_REALTIME",
        "VIVID_ENDPOINT_BULK",
        "VIVID_ROOT_SECRET",
        "VIVID_ANCHOR_TRANSPORT",
        "VIVIDO_WINDOW_ID",
        "VIVIDO_INPUT_TRANSPORT",
    ];
    const EXPORTED: [&str; 5] = [
        "VIVID_ENDPOINT_CONTROL",
        "VIVID_ROOT_SECRET",
        "VIVID_ANCHOR_TRANSPORT",
        "VIVIDO_WINDOW_ID",
        "VIVIDO_INPUT_TRANSPORT",
    ];

    let mut entries = inherited
        .split(':')
        .filter(|entry| !entry.is_empty())
        .filter(|entry| {
            let name = entry.split_once('/').map_or(*entry, |(name, _)| name);
            !MANAGED.contains(&name)
        })
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    entries.extend(EXPORTED.map(|name| format!("{name}/u")));
    entries.join(":")
}

impl Drop for WindowContext {
    fn drop(&mut self) {
        // Shutdown the terminal's PTY.
        let _ = self.notifier.0.send(Msg::Shutdown);
        if let Some(worker) = self.io_thread.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(any(unix, windows))]
fn selection_json(selection: crate::terminal::selection::SelectionRange) -> Value {
    json_value!({
        "start": {"line": selection.start.line.0, "column": selection.start.column.0},
        "end": {"line": selection.end.line.0, "column": selection.end.column.0},
        "block": selection.is_block,
    })
}

#[cfg(any(unix, windows))]
fn exit_status_json(status: Option<&std::process::ExitStatus>) -> Value {
    match status {
        Some(status) => json_value!({
            "state": "exited",
            "code": status.code(),
            "signal": exit_signal(status),
            "core_dumped": exit_core_dumped(status),
        }),
        None => json_value!({"state": "running"}),
    }
}

#[cfg(any(unix, windows))]
fn client_fault_json(fault: &ClientFault) -> Value {
    json_value!({
        "fault_id": fault.id,
        "class": fault.class.as_str(),
        "diagnostic": fault.diagnostic,
    })
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(windows)]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(unix)]
fn exit_core_dumped(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.core_dumped()
}

#[cfg(windows)]
fn exit_core_dumped(_status: &std::process::ExitStatus) -> bool {
    false
}

#[cfg(any(unix, windows))]
fn vivid_surface_status_json(window_id: u64, status: &crate::vivid::scene::SurfaceStatus) -> Value {
    let descriptor = &status.definition.descriptor;
    json_value!({
        "window_id": window_id,
        "session_id": status.identity.context.session.session_id,
        "context_id": status.identity.context.context_id,
        "surface_id": status.identity.surface_id,
        "surface_revision": status.revision.get(),
        "surface_generation": status.generation.get(),
        "semantic_profile": status.definition.semantic_profile,
        "coordinate_model": status.definition.coordinate_model as u64,
        "logical_width": status.definition.logical_width,
        "logical_height": status.definition.logical_height,
        "scale_numerator": status.definition.scale_numerator,
        "scale_denominator": status.definition.scale_denominator,
        "rotation": status.definition.rotation,
        "policy": status.definition.policy,
        "descriptor": {
            "role": descriptor.role as u64,
            "title": descriptor.title,
            "semantic_content_revision": descriptor.semantic_content_revision,
            "semantic_availability": descriptor.semantic_availability,
            "locator_hint": descriptor.locator_hint,
        },
        "active_slots": status.active_slots,
        "profile_parameters": cbor_map_json(&status.definition.profile_parameters),
        "lifecycle": status.lifecycle,
    })
}

#[cfg(any(unix, windows))]
fn vivid_track_status_json(window_id: u64, status: &crate::vivid::scene::TrackStatus) -> Value {
    json_value!({
        "window_id": window_id,
        "session_id": status.identity.surface.context.session.session_id,
        "context_id": status.identity.surface.context.context_id,
        "surface_id": status.identity.surface.surface_id,
        "track_id": status.identity.track_id,
        "track_revision": status.state.revision.get(),
        "channel_generation": status.state.channel_generation.get(),
        "kind": crate::vivid::scene::track_kind_name(&status.configuration),
        "slot": status.configuration.slot,
        "mode": status.configuration.mode as u64,
        "lane": status.configuration.lane as u64,
        "lifecycle": status.lifecycle,
        "milestones": status.state.milestones,
        "media_epoch": status.state.media_epoch,
        "last_media_id": status.state.last_media_id,
        "last_decoded_pts_us": status.last_decoded_pts_us,
        "last_presented_pts_us": status.last_presented_pts_us,
        "last_presentation_id": status.last_presentation_id,
        "flow": {
            "cumulative_body_bytes": status.state.flow.sent_body_bytes,
            "cumulative_media_records": status.state.flow.sent_media_records,
            "maximum_body_bytes": status.maximum_channel_bytes,
            "maximum_media_records": status.maximum_channel_records,
        },
        "streaming": {
            "decoded_frames": status.metrics.decoded_frames,
            "discarded_late_frames": status.metrics.discarded_late_frames,
            "latency_keyframe_requests": status.metrics.latency_keyframe_requests,
            "audio_rebases": status.metrics.audio_rebases,
        },
        "playback": {
            "state": if status.configuration.mode as u64 == 2 { "timed" } else { "live" },
            "media_epoch": status.state.media_epoch,
        },
    })
}

#[cfg(any(unix, windows))]
fn cbor_map_json(map: &[(u64, vivid_protocol::cbor::Value)]) -> Value {
    Value::Object(map.iter().map(|(key, value)| (key.to_string(), cbor_json(value))).collect())
}

#[cfg(any(unix, windows))]
fn cbor_json(value: &vivid_protocol::cbor::Value) -> Value {
    use vivid_protocol::cbor::Value as Cbor;
    match value {
        Cbor::Unsigned(value) => Value::from(*value),
        Cbor::Negative(value) => Value::from(*value),
        Cbor::Bytes(value) => Value::from(base64::engine::general_purpose::STANDARD.encode(value)),
        Cbor::Text(value) => Value::from(value.clone()),
        Cbor::Array(value) => Value::Array(value.iter().map(cbor_json).collect()),
        Cbor::Map(value) => cbor_map_json(value),
        Cbor::Bool(value) => Value::from(*value),
        Cbor::Null => Value::Null,
    }
}

#[cfg(any(unix, windows))]
fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(all(unix, target_os = "linux"))]
fn foreground_executable_basename(pid: libc::pid_t) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()?
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(all(unix, target_os = "macos"))]
fn foreground_executable_basename(pid: libc::pid_t) -> Option<String> {
    use std::ffi::CStr;

    let mut buffer = [0_u8; 4096];
    let length = unsafe {
        libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len().try_into().ok()?)
    };
    if length <= 0 {
        return None;
    }
    let path = unsafe { CStr::from_ptr(buffer.as_ptr().cast()) };
    std::path::Path::new(path.to_str().ok()?)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn foreground_executable_basename(_pid: libc::pid_t) -> Option<String> {
    None
}

#[cfg(any(unix, windows))]
fn terminal_mode_names(mode: TermMode) -> Vec<&'static str> {
    let modes = [
        (TermMode::SHOW_CURSOR, "show_cursor"),
        (TermMode::APP_CURSOR, "application_cursor"),
        (TermMode::APP_KEYPAD, "application_keypad"),
        (TermMode::MOUSE_REPORT_CLICK, "mouse_click"),
        (TermMode::BRACKETED_PASTE, "bracketed_paste"),
        (TermMode::SGR_MOUSE, "sgr_mouse"),
        (TermMode::SGR_PIXEL_MOUSE, "sgr_pixel_mouse"),
        (TermMode::MOUSE_MOTION, "mouse_motion"),
        (TermMode::LINE_WRAP, "line_wrap"),
        (TermMode::LINE_FEED_NEW_LINE, "line_feed_new_line"),
        (TermMode::ORIGIN, "origin"),
        (TermMode::INSERT, "insert"),
        (TermMode::FOCUS_IN_OUT, "focus_reporting"),
        (TermMode::ALT_SCREEN, "alternate_screen"),
        (TermMode::MOUSE_DRAG, "mouse_drag"),
        (TermMode::UTF8_MOUSE, "utf8_mouse"),
        (TermMode::ALTERNATE_SCROLL, "alternate_scroll"),
        (TermMode::DISAMBIGUATE_ESC_CODES, "kitty_disambiguate"),
        (TermMode::REPORT_EVENT_TYPES, "kitty_event_types"),
        (TermMode::REPORT_ALTERNATE_KEYS, "kitty_alternate_keys"),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, "kitty_all_keys"),
        (TermMode::REPORT_ASSOCIATED_TEXT, "kitty_associated_text"),
    ];
    modes.into_iter().filter_map(|(flag, name)| mode.contains(flag).then_some(name)).collect()
}

#[cfg(any(unix, windows))]
fn resolve_color(
    colors: &crate::display::color::List,
    color: Color,
    flags: Flags,
    foreground: bool,
    config: &UiConfig,
) -> Rgb {
    match color {
        Color::Spec(rgb) if foreground && flags.contains(Flags::DIM) => Rgb::from(rgb) * DIM_FACTOR,
        Color::Spec(rgb) => rgb.into(),
        Color::Named(named) if foreground => {
            let index =
                if config.colors.draw_bold_text_with_bright_colors && flags.contains(Flags::BOLD) {
                    named.to_bright() as usize
                } else if flags.contains(Flags::DIM) {
                    named.to_dim() as usize
                } else {
                    named as usize
                };
            colors[index]
        },
        Color::Named(named) => colors[named],
        Color::Indexed(index) if foreground => {
            let index = if config.colors.draw_bold_text_with_bright_colors
                && flags.contains(Flags::BOLD)
                && index <= 7
            {
                usize::from(index) + 8
            } else if flags.contains(Flags::DIM) && index <= 7 {
                NamedColor::DimBlack as usize + usize::from(index)
            } else {
                usize::from(index)
            };
            colors[index]
        },
        Color::Indexed(index) => colors[usize::from(index)],
    }
}

#[cfg(any(unix, windows))]
fn style_attribute_names(flags: Flags) -> Vec<&'static str> {
    let attributes = [
        (Flags::BOLD, "bold"),
        (Flags::ITALIC, "italic"),
        (Flags::DIM, "dim"),
        (Flags::HIDDEN, "hidden"),
        (Flags::STRIKEOUT, "strikeout"),
        (Flags::UNDERLINE, "underline"),
        (Flags::DOUBLE_UNDERLINE, "double_underline"),
        (Flags::UNDERCURL, "undercurl"),
        (Flags::DOTTED_UNDERLINE, "dotted_underline"),
        (Flags::DASHED_UNDERLINE, "dashed_underline"),
        (Flags::INVERSE, "inverse"),
    ];
    attributes.into_iter().filter_map(|(flag, name)| flags.contains(flag).then_some(name)).collect()
}

#[cfg(any(unix, windows))]
#[derive(Copy, Clone)]
enum MouseEncodingAction {
    Move,
    Click(IpcMouseButton, usize),
    Down(IpcMouseButton),
    Up(IpcMouseButton),
    Drag(IpcMouseButton),
    Scroll(f64, f64),
}

#[cfg(any(unix, windows))]
#[derive(Copy, Clone, Debug, PartialEq)]
struct ResolvedMousePosition {
    column: usize,
    row: usize,
    pixel_x: usize,
    pixel_y: usize,
    physical: PhysicalPosition<f64>,
}

#[cfg(any(unix, windows))]
fn validate_mouse_path(path: &IpcMousePath) -> Result<(), IpcError> {
    if !(2..=1000).contains(&path.points.len()) {
        return Err(IpcError::new(
            "limit_exceeded",
            "mouse path must contain 2 through 1000 points",
        ));
    }
    if path.points.iter().any(|point| !point.x.is_finite() || !point.y.is_finite()) {
        return Err(IpcError::new("invalid_params", "mouse path coordinates must be finite"));
    }
    if path.duration.is_some_and(|duration| !(1..=30_000).contains(&duration)) {
        return Err(IpcError::new(
            "invalid_params",
            "paced mouse path duration must be 1 ms through 30 seconds",
        ));
    }
    if path.wait_frame && !(1..=86_400_000).contains(&path.timeout) {
        return Err(IpcError::new(
            "invalid_params",
            "mouse path timeout must be 1 ms through 24 hours",
        ));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn mouse_modifier_code(modifiers: &[String]) -> Result<u8, IpcError> {
    let mut code = 0;
    for modifier in modifiers {
        match modifier.to_ascii_lowercase().as_str() {
            "shift" => code |= 4,
            "alt" | "option" => code |= 8,
            "ctrl" | "control" => code |= 16,
            "super" | "command" | "cmd" => {
                return Err(IpcError::new(
                    "unsupported",
                    "the active terminal mouse protocols cannot encode the Super modifier",
                ));
            },
            _ => {
                return Err(IpcError::new(
                    "invalid_params",
                    format!("unknown modifier {modifier:?}"),
                ));
            },
        }
    }
    Ok(code)
}

#[cfg(any(unix, windows))]
fn append_mouse_report(
    output: &mut Vec<u8>,
    mode: TermMode,
    position: ResolvedMousePosition,
    button: u8,
    pressed: bool,
) -> Result<(), IpcError> {
    if mode.contains(TermMode::SGR_MOUSE) {
        let terminator = if pressed { 'M' } else { 'm' };
        let (x, y) = if mode.contains(TermMode::SGR_PIXEL_MOUSE) {
            (position.pixel_x, position.pixel_y)
        } else {
            (position.column, position.row)
        };
        output.extend_from_slice(
            format!("\x1b[<{button};{};{}{terminator}", x + 1, y + 1).as_bytes(),
        );
        return Ok(());
    }

    let button = if pressed { button } else { 3 + (button & (4 | 8 | 16)) };
    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    let maximum = if utf8 { 2015 } else { 223 };
    if position.column >= maximum || position.row >= maximum {
        return Err(IpcError::new(
            "limit_exceeded",
            "mouse coordinate exceeds the active terminal mouse protocol",
        ));
    }
    output.extend_from_slice(&[b'\x1b', b'[', b'M', 32 + button]);
    append_legacy_mouse_coordinate(output, position.column, utf8);
    append_legacy_mouse_coordinate(output, position.row, utf8);
    Ok(())
}

#[cfg(any(unix, windows))]
fn append_legacy_mouse_coordinate(output: &mut Vec<u8>, coordinate: usize, utf8: bool) {
    let encoded = coordinate + 33;
    if utf8 && coordinate >= 95 {
        output.push((0xc0 + encoded / 64) as u8);
        output.push((0x80 + (encoded & 63)) as u8);
    } else {
        output.push(encoded as u8);
    }
}

/// Turn a shell-reported working directory into a path the local process can inherit.
///
/// OSC 7 describes the shell's namespace. On Windows a WSL shell therefore reports a Linux path,
/// even though `CreateProcessW` requires a Windows directory. Drive mounts and Windows file-URI
/// paths have lossless lexical translations; other WSL paths have no stable native spelling
/// without the distribution name, so they fall back to Vivido's own working directory.
pub(crate) fn reported_working_directory(path: &str) -> Option<PathBuf> {
    #[cfg(not(windows))]
    {
        Some(PathBuf::from(path))
    }

    #[cfg(windows)]
    {
        let native = PathBuf::from(path);
        if native.is_dir() {
            return Some(native);
        }

        windows_path_from_shell_report(path).filter(|path| path.is_dir())
    }
}

#[cfg(windows)]
fn windows_path_from_shell_report(path: &str) -> Option<PathBuf> {
    let (drive, tail) = if let Some(rest) = path.strip_prefix("/mnt/") {
        let (drive, tail) = rest.split_once('/').unwrap_or((rest, ""));
        (drive, tail)
    } else {
        let rest = path.strip_prefix('/')?;
        let (drive, tail) = rest.split_once(':')?;
        (drive, tail.strip_prefix('/').unwrap_or(tail))
    };

    if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }

    let drive = char::from(drive.as_bytes()[0]).to_ascii_uppercase();
    let mut converted = PathBuf::from(format!("{drive}:\\"));
    converted.extend(tail.split('/').filter(|component| !component.is_empty()));
    Some(converted)
}

/// Feed one cell color into a screen-change hash.
///
/// The hash only has to separate colors that differ within a single run, so the variant tag plus
/// the variant's own bytes is enough; it never leaves the process and is never persisted.
#[cfg(any(unix, windows))]
fn hash_color<H: Hasher>(color: Option<Color>, hasher: &mut H) {
    match color {
        None => 0u8.hash(hasher),
        Some(Color::Named(named)) => {
            1u8.hash(hasher);
            (named as u16).hash(hasher);
        },
        Some(Color::Spec(rgb)) => {
            2u8.hash(hasher);
            rgb.r.hash(hasher);
            rgb.g.hash(hasher);
            rgb.b.hash(hasher);
        },
        Some(Color::Indexed(index)) => {
            3u8.hash(hasher);
            index.hash(hasher);
        },
    }
}

#[cfg(test)]
mod vivid_environment_tests {
    #[cfg(any(unix, windows))]
    use super::assign_ipc_window_id;
    use super::configure_vivid_pty_environment;
    #[cfg(windows)]
    use super::vivid_wslenv;
    #[cfg(windows)]
    use super::{
        LATENCY_SENSITIVE_FRAME_INTERVAL, LatencySensitiveFrameTimer, latency_sensitive_draw_delay,
        windows_path_from_shell_report,
    };
    #[cfg(any(unix, windows))]
    use super::{ResolvedMousePosition, append_mouse_report};
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    use super::{flushes_staged_input, is_latency_sensitive_input};
    use std::collections::HashMap;
    #[cfg(windows)]
    use std::path::PathBuf;
    #[cfg(any(unix, windows))]
    use winit::dpi::PhysicalPosition;
    #[cfg(windows)]
    use winit::event::Ime;
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    use winit::event::{DeviceId, Event as WinitEvent, MouseScrollDelta, TouchPhase, WindowEvent};
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    use winit::window::WindowId;

    #[cfg(any(unix, windows))]
    #[test]
    fn automation_sgr_pixel_mouse_preserves_physical_coordinates() {
        let position = ResolvedMousePosition {
            column: 92,
            row: 20,
            pixel_x: 1013,
            pixel_y: 485,
            physical: PhysicalPosition::new(1013.0, 485.0),
        };
        let mut output = Vec::new();

        append_mouse_report(
            &mut output,
            crate::terminal::term::TermMode::SGR_MOUSE
                | crate::terminal::term::TermMode::SGR_PIXEL_MOUSE,
            position,
            0,
            true,
        )
        .unwrap();

        assert_eq!(output, b"\x1b[<0;1014;486M");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn automation_legacy_sgr_mouse_uses_cell_coordinates() {
        let position = ResolvedMousePosition {
            column: 92,
            row: 20,
            pixel_x: 1013,
            pixel_y: 485,
            physical: PhysicalPosition::new(1013.0, 485.0),
        };
        let mut output = Vec::new();

        append_mouse_report(
            &mut output,
            crate::terminal::term::TermMode::SGR_MOUSE,
            position,
            0,
            true,
        )
        .unwrap();

        assert_eq!(output, b"\x1b[<0;93;21M");
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn mouse_wheel_flushes_staged_input_without_waiting_for_idle() {
        let device_id = DeviceId::dummy();
        let window_id = WindowId::dummy();
        let event = WinitEvent::WindowEvent {
            window_id,
            event: WindowEvent::MouseWheel {
                device_id,
                delta: MouseScrollDelta::LineDelta(0., 1.),
                phase: TouchPhase::Moved,
            },
        };

        assert!(flushes_staged_input(&event));
        assert!(is_latency_sensitive_input(&event));
    }

    #[cfg(windows)]
    #[test]
    fn windows_text_input_flushes_staged_input_without_waiting_for_idle() {
        let event = WinitEvent::WindowEvent {
            window_id: WindowId::dummy(),
            event: WindowEvent::Ime(Ime::Commit("echo hello".into())),
        };

        assert!(flushes_staged_input(&event));
        assert!(is_latency_sensitive_input(&event));
    }

    #[cfg(windows)]
    #[test]
    fn windows_drag_motion_flushes_staged_selection_without_waiting_for_idle() {
        let event = WinitEvent::WindowEvent {
            window_id: WindowId::dummy(),
            event: WindowEvent::CursorMoved {
                device_id: DeviceId::dummy(),
                position: winit::dpi::PhysicalPosition::new(120., 240.),
            },
        };

        assert!(flushes_staged_input(&event));
        assert!(is_latency_sensitive_input(&event));
    }

    #[cfg(windows)]
    #[test]
    fn latency_sensitive_draws_are_bounded_to_one_per_frame_interval() {
        let start = std::time::Instant::now();

        assert_eq!(latency_sensitive_draw_delay(None, start), None);
        assert_eq!(
            latency_sensitive_draw_delay(
                Some(start),
                start + LATENCY_SENSITIVE_FRAME_INTERVAL - std::time::Duration::from_nanos(1)
            ),
            Some(std::time::Duration::from_nanos(1))
        );
        assert_eq!(
            latency_sensitive_draw_delay(Some(start), start + LATENCY_SENSITIVE_FRAME_INTERVAL),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn latency_sensitive_tail_wake_is_active_and_coalesced() {
        let (sink, receiver) = crate::event::EventSink::headless();
        let timer = LatencySensitiveFrameTimer::new(sink, WindowId::dummy());

        timer.schedule(std::time::Duration::from_millis(1));
        timer.schedule(std::time::Duration::from_millis(1));
        receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("the active timer wakes without an event-loop idle callback");
        assert!(receiver.try_recv().is_err(), "concurrent wake requests are coalesced");

        timer.acknowledge();
        timer.schedule(std::time::Duration::from_millis(1));
        receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("acknowledgement permits the next tail wake");
    }

    #[cfg(windows)]
    #[test]
    fn shell_reports_translate_wsl_mounts_and_windows_file_uri_paths() {
        assert_eq!(
            windows_path_from_shell_report("/mnt/f/Github/vivido-private/vivido"),
            Some(PathBuf::from(r"F:\Github\vivido-private\vivido"))
        );
        assert_eq!(
            windows_path_from_shell_report("/C:/Users/example/My Files"),
            Some(PathBuf::from(r"C:\Users\example\My Files"))
        );
        assert_eq!(windows_path_from_shell_report("/home/example"), None);
    }

    #[test]
    fn child_receives_the_platform_marker_transport() {
        let mut environment = HashMap::new();
        configure_vivid_pty_environment(&mut environment, "tcp:127.0.0.1:1", "secret", 42);

        assert_eq!(
            environment.get("VIVID_ENDPOINT_CONTROL").map(String::as_str),
            Some("tcp:127.0.0.1:1")
        );
        assert_eq!(environment.get("VIVID_ROOT_SECRET").map(String::as_str), Some("secret"));
        assert_eq!(environment.get("VIVIDO_WINDOW_ID").map(String::as_str), Some("42"));
        assert_eq!(
            environment.get("VIVIDO_INPUT_TRANSPORT").map(String::as_str),
            Some(if cfg!(windows) { "win32-console" } else { "pty-bytes" })
        );
        #[cfg(windows)]
        assert_eq!(environment.get("VIVID_ANCHOR_TRANSPORT").map(String::as_str), Some("conpty"));
        #[cfg(not(windows))]
        assert!(!environment.contains_key("VIVID_ANCHOR_TRANSPORT"));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn public_window_ids_are_small_and_monotonic() {
        // The counter is process-global and other tests share it, so compare, never assert values.
        let first = assign_ipc_window_id(None);
        let second = assign_ipc_window_id(None);

        assert!(second > first, "ids advance: {first} then {second}");
        assert!(
            u32::try_from(second).is_ok(),
            "an id must fit an agent-mesh address index, got {second}"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_claimed_window_id_is_kept_and_never_handed_out_again() {
        let claimed = assign_ipc_window_id(None) + 5_000;

        assert_eq!(assign_ipc_window_id(Some(claimed)), claimed, "a named id is honored");
        assert!(
            assign_ipc_window_id(None) > claimed,
            "the counter steps past a claimed id so it cannot be assigned twice"
        );
    }

    #[test]
    fn child_receives_a_mesh_address_only_when_the_id_can_be_one() {
        let mut environment = HashMap::new();
        configure_vivid_pty_environment(&mut environment, "tcp:127.0.0.1:1", "secret", 7);
        assert_eq!(environment.get("AGENT_MESH_ADDRESS").map(String::as_str), Some("w7"));

        // Winit ids used to land here, and an address index is a one-based `u32`. Publishing one
        // that cannot parse failed `vvagent bind` outright rather than costing only the position.
        for unaddressable in [0, u64::from(u32::MAX) + 1, 9_223_372_036_854_775_808] {
            let mut environment = HashMap::new();
            configure_vivid_pty_environment(
                &mut environment,
                "tcp:127.0.0.1:1",
                "secret",
                unaddressable,
            );

            assert_eq!(
                environment.get("VIVIDO_WINDOW_ID").map(String::as_str),
                Some(unaddressable.to_string().as_str()),
                "the window stays addressable by automation"
            );
            assert!(
                !environment.contains_key("AGENT_MESH_ADDRESS"),
                "{unaddressable} cannot be an address index, so no address is published"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn wsl_receives_vivid_discovery_without_overwriting_user_entries() {
        assert_eq!(
            vivid_wslenv(
                "GOPATH/p:VIVID_ROOT_SECRET/w:VIVID_ENDPOINT_BULK/u::CARGO_HOME/p:\
                 VIVID_ENDPOINT_CONTROL/l:VIVIDO_WINDOW_ID/w"
            ),
            "GOPATH/p:CARGO_HOME/p:VIVID_ENDPOINT_CONTROL/u:VIVID_ROOT_SECRET/u:\
             VIVID_ANCHOR_TRANSPORT/u:VIVIDO_WINDOW_ID/u:VIVIDO_INPUT_TRANSPORT/u"
        );
    }
}
