//! Terminal window context.

#[cfg(unix)]
use std::borrow::Cow;
#[cfg(unix)]
use std::cell::RefCell;
#[cfg(unix)]
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::fs::File;
#[cfg(unix)]
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::mem;
#[cfg(not(windows))]
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;
#[cfg(unix)]
use std::time::Duration;
use std::time::Instant;

use log::info;
use serde_json as json;
#[cfg(unix)]
use serde_json::{Value, json as json_value};
#[cfg(unix)]
use winit::dpi::PhysicalPosition;
use winit::event::{
    ElementState, Event as WinitEvent, Modifiers, MouseButton, MouseScrollDelta, TouchPhase,
    WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::WindowId;

use crate::terminal::event::Event as TerminalEvent;
#[cfg(unix)]
use crate::terminal::event::Notify;
#[cfg(unix)]
use crate::terminal::event_loop::EventLoopSendError;
use crate::terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use crate::terminal::grid::{Dimensions, Scroll};
use crate::terminal::index::Direction;
#[cfg(unix)]
use crate::terminal::index::{Column, Line};
use crate::terminal::sync::FairMutex;
use crate::terminal::term::Term;
#[cfg(unix)]
use crate::terminal::term::TermMode;
#[cfg(unix)]
use crate::terminal::term::cell::Flags;
use crate::terminal::term::test::TermSize;
use crate::terminal::tty;
#[cfg(unix)]
use crate::terminal::vte::ansi::{Color, NamedColor};

#[cfg(unix)]
use crate::automation::{AutomationWindowState, Transcript};
#[cfg(unix)]
use crate::cli::{
    IpcKey, IpcMouse, IpcMouseAction, IpcMouseButton, IpcMousePosition, IpcSignalName,
};
use crate::cli::{ParsedOptions, WindowOptions};
use crate::clipboard::Clipboard;
use crate::config::UiConfig;
use crate::display::Display;
#[cfg(unix)]
use crate::display::ScreenshotReadback;
#[cfg(unix)]
use crate::display::color::{DIM_FACTOR, Rgb};
use crate::display::window::Window;
#[cfg(unix)]
use crate::event::EventType;
use crate::event::{ActionContext, Event, EventProxy, Mouse, SearchState, TouchPurpose};
use crate::input;
#[cfg(unix)]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::MessageBuffer;
#[cfg(unix)]
use crate::polling::ipc::{IpcConnection, IpcError};
use crate::scheduler::Scheduler;
#[cfg(unix)]
use crate::scheduler::{TimerId, Topic};
#[cfg(unix)]
use crate::screenshot;
#[cfg(unix)]
use crate::terminal::thread;
use crate::vivid::{DisplayMetrics, VividService};

#[cfg(unix)]
type AutomationResize = (u32, u32, Option<(u16, u16)>);

#[cfg(unix)]
#[derive(Default)]
struct AutomationNotifier(RefCell<Vec<u8>>);

#[cfg(unix)]
impl AutomationNotifier {
    fn into_bytes(self) -> Vec<u8> {
        self.0.into_inner()
    }
}

#[cfg(unix)]
impl Notify for AutomationNotifier {
    fn notify<B: Into<Cow<'static, [u8]>>>(&self, bytes: B) {
        self.0.borrow_mut().extend_from_slice(bytes.into().as_ref());
    }
}

/// Event context for one individual Vivido window.
pub struct WindowContext {
    pub message_buffer: MessageBuffer,
    pub display: Display,
    pub dirty: bool,
    event_queue: Vec<WinitEvent<Event>>,
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    cursor_blink_timed_out: bool,
    prev_bell_cmd: Option<Instant>,
    modifiers: Modifiers,
    search_state: SearchState,
    notifier: Notifier,
    mouse: Mouse,
    touch: TouchPurpose,
    occluded: bool,
    preserve_title: bool,
    #[cfg(not(windows))]
    master_fd: RawFd,
    #[cfg(not(windows))]
    shell_pid: u32,
    window_config: ParsedOptions,
    config: Rc<UiConfig>,
    vivid_service: VividService,
    #[cfg(unix)]
    ipc_window_id: u64,
    #[cfg(unix)]
    screenshot: Option<PendingScreenshot>,
    #[cfg(unix)]
    screenshot_busy: bool,
    #[cfg(unix)]
    pub automation: AutomationWindowState,
}

#[cfg(unix)]
struct PendingScreenshot {
    readback: ScreenshotReadback,
    connection: IpcConnection,
    request_id: u64,
}

#[cfg(unix)]
const SCREENSHOT_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[cfg(unix)]
const SCREENSHOT_READBACK_TIMEOUT: Duration = Duration::from_secs(5);

impl WindowContext {
    /// Create initial window context.
    pub fn initial(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        let window = Window::new(event_loop, &config, &identity, &mut options)?;
        let display = Display::new(window, &config, false)?;

        Self::new(display, config, options, proxy)
    }

    /// Create additional context.
    pub fn additional(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
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

        let window = Window::new(event_loop, &config, &identity, &mut options)?;
        let display = Display::new(window, &config, tabbed)?;

        let mut window_context = Self::new(display, config, options, proxy)?;

        // Set the config overrides at startup.
        //
        // These are already applied to `config`, so no update is necessary.
        window_context.window_config = config_overrides;

        Ok(window_context)
    }

    /// Create a new terminal window context.
    fn new(
        mut display: Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let mut pty_config = config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);

        let preserve_title = options.window_identity.title.is_some();
        #[cfg(unix)]
        let ipc_window_id = options.ipc_window_id.unwrap_or_else(|| display.window.id().into());

        info!(
            "PTY dimensions: {:?} x {:?}",
            display.size_info.screen_lines(),
            display.size_info.columns()
        );

        let event_proxy = EventProxy::new(proxy, display.window.id());

        let vivid_service = {
            let size = display.size_info;
            let service = VividService::start(
                DisplayMetrics {
                    viewport_width: size.width() as u32,
                    viewport_height: size.height() as u32,
                    columns: size.columns() as u32,
                    rows: size.screen_lines() as u32,
                    cell_width: size.cell_width().round() as u32,
                    cell_height: size.cell_height().round() as u32,
                    generation: 1,
                },
                event_proxy.clone(),
            )?;
            pty_config.env.insert("VIVID_ENDPOINT".into(), service.endpoint().into());
            pty_config.env.insert("VIVID_TOKEN".into(), service.token().into());
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

        // Create the PTY.
        //
        // The PTY forks a process to run the shell on the slave side of the
        // pseudoterminal. A file descriptor for the master side is retained for
        // reading/writing to the shell.
        #[cfg(unix)]
        let terminal_window_id = ipc_window_id;
        #[cfg(not(unix))]
        let terminal_window_id = display.window.id().into();
        let pty = tty::new(&pty_config, display.size_info.into(), terminal_window_id)?;

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();

        // Create the pseudoterminal I/O loop.
        //
        // PTY I/O is ran on another thread as to not occupy cycles used by the
        // renderer and input processing. Note that access to the terminal state is
        // synchronized since the I/O loop updates the state, and the display
        // consumes it periodically.
        #[cfg(unix)]
        let transcript = Arc::new(Mutex::new(Transcript::default()));
        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy.clone(),
            pty,
            pty_config.drain_on_exit,
            config.debug.ref_test,
            #[cfg(unix)]
            transcript.clone(),
        )?;

        // The event loop channel allows write requests from the event processor
        // to be sent to the pty loop and ultimately written to the pty.
        let loop_tx = event_loop.channel();

        // Kick off the I/O thread.
        let _io_thread = event_loop.spawn();

        // Start cursor blinking, in case `Focused` isn't sent on startup.
        if config.cursor.style().blinking {
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        // Create context for the Vivido window.
        Ok(WindowContext {
            preserve_title,
            terminal,
            display,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            config,
            notifier: Notifier(loop_tx),
            cursor_blink_timed_out: Default::default(),
            prev_bell_cmd: Default::default(),
            message_buffer: Default::default(),
            window_config: Default::default(),
            search_state: Default::default(),
            event_queue: Default::default(),
            modifiers: Default::default(),
            occluded: Default::default(),
            mouse: Default::default(),
            touch: Default::default(),
            dirty: Default::default(),
            vivid_service,
            #[cfg(unix)]
            ipc_window_id,
            #[cfg(unix)]
            screenshot: None,
            #[cfg(unix)]
            screenshot_busy: false,
            #[cfg(unix)]
            automation: AutomationWindowState::new(0, transcript),
        })
    }

    /// Update the terminal window to the latest config.
    pub fn update_config(&mut self, new_config: Rc<UiConfig>) {
        let old_config = mem::replace(&mut self.config, new_config);

        // Apply ipc config if there are overrides.
        self.config = self.window_config.override_config_rc(self.config.clone());

        self.display.update_config(&self.config);
        self.terminal.lock().set_options(self.config.term_options());

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
    #[cfg(unix)]
    pub fn config(&self) -> &UiConfig {
        &self.config
    }

    /// Clear the window config overrides.
    #[cfg(unix)]
    pub fn reset_window_config(&mut self, config: Rc<UiConfig>) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.clear();

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Add new window config overrides.
    #[cfg(unix)]
    pub fn add_window_config(&mut self, config: Rc<UiConfig>, options: &ParsedOptions) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.extend_from_slice(options);

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Draw the window.
    pub fn draw(&mut self, scheduler: &mut Scheduler) -> bool {
        self.display.window.requested_redraw = false;

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

    /// Process events for this terminal window.
    pub fn handle_event(
        &mut self,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: WinitEvent<Event>,
    ) {
        match event {
            WinitEvent::AboutToWait
            | WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                // Skip further event handling with no staged updates.
                if self.event_queue.is_empty() {
                    return;
                }

                // Continue to process all pending events.
            },
            event => {
                self.event_queue.push(event);
                return;
            },
        }

        let mut terminal = self.terminal.lock();

        let old_is_searching = self.search_state.history_index.is_some();

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
            let size = self.display.size_info;
            self.vivid_service.update_metrics(DisplayMetrics {
                viewport_width: size.width() as u32,
                viewport_height: size.height() as u32,
                columns: size.columns() as u32,
                rows: size.screen_lines() as u32,
                cell_width: size.cell_width().round() as u32,
                cell_height: size.cell_height().round() as u32,
                generation: 0,
            });
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
            && self.display.window.has_frame
            && !self.occluded
            && !matches!(event, WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. })
        {
            self.display.window.request_redraw();
        }
    }

    /// ID of this terminal context.
    pub fn id(&self) -> WindowId {
        self.display.window.id()
    }

    /// Stable external ID used to target this window through IPC.
    #[cfg(unix)]
    pub fn ipc_window_id(&self) -> u64 {
        self.ipc_window_id
    }

    /// Whether this terminal currently has keyboard focus.
    #[cfg(unix)]
    pub fn is_focused(&self) -> bool {
        self.terminal.lock().is_focused
    }

    /// Write bytes and notify the main event loop after the PTY master accepted all of them.
    #[cfg(unix)]
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

    /// Apply the current terminal dimensions to the PTY and report completion.
    #[cfg(unix)]
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
    #[cfg(unix)]
    pub fn text(&self, rows: Option<u16>) -> String {
        let terminal = self.terminal.lock();
        match rows {
            Some(rows) => terminal.latest_text(usize::from(rows)),
            None => terminal.visible_text(),
        }
    }

    /// Build application-directed paste bytes with the same safety filtering as local paste.
    #[cfg(unix)]
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
    #[cfg(unix)]
    pub fn ui_paste(
        &mut self,
        text: &str,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
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
                master_fd: self.master_fd,
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
    #[cfg(unix)]
    pub fn terminal_mode(&self) -> TermMode {
        *self.terminal.lock().mode()
    }

    /// Process a neutral key through Vivido's normal UI input processor.
    #[cfg(unix)]
    pub fn ui_key(
        &mut self,
        key: &IpcKey,
        repeated: bool,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
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
            master_fd: self.master_fd,
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
    #[cfg(unix)]
    pub fn ui_mouse(
        &mut self,
        mouse: &IpcMouse,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
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
            IpcMouseAction::Scroll(action) => &action.position,
        };
        let modifier_override = crate::input::keyboard::ipc_modifier_state(&position.mods)?;
        let (column, row) = self.mouse_cell(position)?;
        let size = self.display.size_info;
        let physical = PhysicalPosition::new(
            f64::from(size.padding_x()) + (column as f64 + 0.5) * f64::from(size.cell_width()),
            f64::from(size.padding_y()) + (row as f64 + 0.5) * f64::from(size.cell_height()),
        );

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
                master_fd: self.master_fd,
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
                IpcMouseAction::Scroll(action) => processor.mouse_wheel_input(
                    MouseScrollDelta::LineDelta(action.horizontal as f32, action.vertical as f32),
                    TouchPhase::Moved,
                ),
            }
        }
        Ok(notifier.into_bytes())
    }

    /// Encode one application mouse action without entering Vivido's UI input path.
    #[cfg(unix)]
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
            IpcMouseAction::Scroll(action) => {
                (&action.position, MouseEncodingAction::Scroll(action.vertical, action.horizontal))
            },
        };
        let (column, row) = self.mouse_cell(position)?;
        let modifiers = mouse_modifier_code(&position.mods)?;
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
                append_mouse_report(&mut output, mode, column, row, 35 + modifiers, true)?;
            },
            MouseEncodingAction::Click(button, count) => {
                let code = button_code(button) + modifiers;
                for _ in 0..count {
                    append_mouse_report(&mut output, mode, column, row, code, true)?;
                    append_mouse_report(&mut output, mode, column, row, code, false)?;
                }
            },
            MouseEncodingAction::Down(button) => {
                append_mouse_report(
                    &mut output,
                    mode,
                    column,
                    row,
                    button_code(button) + modifiers,
                    true,
                )?;
            },
            MouseEncodingAction::Up(button) => {
                append_mouse_report(
                    &mut output,
                    mode,
                    column,
                    row,
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
                    column,
                    row,
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
                        column,
                        row,
                        vertical_code + modifiers,
                        true,
                    )?;
                }
                for _ in 0..horizontal_count {
                    append_mouse_report(
                        &mut output,
                        mode,
                        column,
                        row,
                        horizontal_code + modifiers,
                        true,
                    )?;
                }
            },
        }
        Ok(output)
    }

    #[cfg(unix)]
    fn mouse_cell(&self, position: &IpcMousePosition) -> Result<(usize, usize), IpcError> {
        let size = self.display.size_info;
        let (column, row) = match (position.cell_column, position.cell_row, position.x, position.y)
        {
            (Some(column), Some(row), None, None) => (column as usize, row as usize),
            (None, None, Some(x), Some(y)) if x.is_finite() && y.is_finite() => {
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
                (column, row)
            },
            _ => {
                return Err(IpcError::new(
                    "invalid_params",
                    "mouse requires exactly one cell or pixel coordinate pair",
                ));
            },
        };
        if column >= size.columns() || row >= size.screen_lines() {
            return Err(IpcError::new(
                "invalid_params",
                "mouse coordinate is outside the terminal grid",
            ));
        }
        Ok((column, row))
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

    /// Request an exact client-area size.
    #[cfg(unix)]
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
        self.display.window.request_inner_size(winit::dpi::PhysicalSize::new(width, height));
        Ok((width, height, grid))
    }

    #[cfg(unix)]
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
    #[cfg(unix)]
    pub fn request_automation_focus(&self) {
        self.display.window.focus_window();
    }

    /// Coalesce terminal-model mutations into one semantic screen sequence change.
    #[cfg(unix)]
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
                format!("{:?}", cell.fg).hash(&mut hasher);
                format!("{:?}", cell.bg).hash(&mut hasher);
                format!("{:?}", cell.underline_color()).hash(&mut hasher);
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

    /// Summary used by deterministic window discovery.
    #[cfg(unix)]
    pub fn automation_summary(&self) -> Value {
        let terminal = self.terminal.lock();
        self.automation_summary_with_terminal(&terminal)
    }

    #[cfg(unix)]
    fn automation_summary_with_terminal(&self, terminal: &Term<EventProxy>) -> Value {
        let size = self.display.size_info;
        let pixels = self.display.window.inner_size();
        json_value!({
            "window_id": self.ipc_window_id,
            "creation_index": self.automation.creation_index,
            "title": self.display.window.title(),
            "focused": terminal.is_focused,
            "occluded": self.occluded,
            "hold": self.display.window.hold,
            "grid": {"columns": size.columns(), "rows": size.screen_lines()},
            "pixels": {"width": pixels.width, "height": pixels.height},
            "process": exit_status_json(self.automation.exit_status.as_ref()),
            "sequences": {
                "screen": self.automation.screen_sequence,
                "frame": self.automation.frame_sequence,
                "output": self.automation.transcript.lock().unwrap().end_offset(),
            },
        })
    }

    /// Detailed, secret-free terminal/window inspection.
    #[cfg(unix)]
    pub fn automation_inspect(&self, event_sequence: u64) -> Value {
        let terminal = self.terminal.lock();
        let grid = terminal.grid();
        let size = self.display.size_info;
        let cursor = grid.cursor.point;
        let selection =
            terminal.selection.as_ref().and_then(|selection| selection.to_range(&terminal));
        let foreground_pgid = unsafe { libc::tcgetpgrp(self.master_fd) };
        let foreground_pgid = (foreground_pgid > 0).then_some(foreground_pgid);
        let executable = foreground_pgid.and_then(foreground_executable_basename);
        let current_directory =
            crate::daemon::foreground_process_path(self.master_fd, self.shell_pid)
                .ok()
                .map(|path| path.to_string_lossy().into_owned());
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
        let echo = unsafe {
            (libc::tcgetattr(self.master_fd, attributes.as_mut_ptr()) == 0)
                .then(|| attributes.assume_init().c_lflag & libc::ECHO != 0)
        };

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
            "limits": {
                "transcript_bytes": crate::automation::TRANSCRIPT_CAPACITY,
                "screen_history": crate::automation::SCREEN_HISTORY_COUNT,
                "grid_rows": 1000,
                "reply_bytes": crate::polling::ipc::MAX_REPLY_FRAME_BYTES,
            },
        })
    }

    /// Structured physical-cell grid snapshot or current-state delta.
    #[cfg(unix)]
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

    #[cfg(unix)]
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
    #[cfg(unix)]
    pub fn request_screenshot(
        &mut self,
        connection: IpcConnection,
        request_id: u64,
        scheduler: &mut Scheduler,
    ) -> Result<(), String> {
        if self.screenshot_busy {
            return Err(String::from("a screenshot is already in progress for this window"));
        }

        let readback = self.display.begin_screenshot().map_err(|err| err.to_string())?;
        self.screenshot = Some(PendingScreenshot { readback, connection, request_id });
        self.screenshot_busy = true;

        let window_id = self.id();
        let timer_id = TimerId::new(Topic::ScreenshotReadback, window_id);
        let event = Event::new(EventType::ScreenshotReadback, window_id);
        scheduler.schedule(event, SCREENSHOT_POLL_INTERVAL, true, timer_id);
        Ok(())
    }

    /// Poll screenshot readback and move PNG encoding off the event-loop thread.
    #[cfg(unix)]
    pub fn poll_screenshot(
        &mut self,
        scheduler: &mut Scheduler,
        event_proxy: &EventLoopProxy<Event>,
    ) {
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
                            Some(path) => pending
                                .connection
                                .reply(pending.request_id, serde_json::json!({"path": path})),
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
    #[cfg(unix)]
    pub fn complete_screenshot(&mut self) {
        self.screenshot_busy = false;
    }

    /// Forget asynchronous work owned by a disconnected IPC client.
    #[cfg(unix)]
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
    #[cfg(unix)]
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

impl Drop for WindowContext {
    fn drop(&mut self) {
        // Shutdown the terminal's PTY.
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

#[cfg(unix)]
fn selection_json(selection: crate::terminal::selection::SelectionRange) -> Value {
    json_value!({
        "start": {"line": selection.start.line.0, "column": selection.start.column.0},
        "end": {"line": selection.end.line.0, "column": selection.end.column.0},
        "block": selection.is_block,
    })
}

#[cfg(unix)]
fn exit_status_json(status: Option<&std::process::ExitStatus>) -> Value {
    use std::os::unix::process::ExitStatusExt;

    match status {
        Some(status) => json_value!({
            "state": "exited",
            "code": status.code(),
            "signal": status.signal(),
            "core_dumped": status.core_dumped(),
        }),
        None => json_value!({"state": "running"}),
    }
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

#[cfg(unix)]
fn terminal_mode_names(mode: TermMode) -> Vec<&'static str> {
    let modes = [
        (TermMode::SHOW_CURSOR, "show_cursor"),
        (TermMode::APP_CURSOR, "application_cursor"),
        (TermMode::APP_KEYPAD, "application_keypad"),
        (TermMode::MOUSE_REPORT_CLICK, "mouse_click"),
        (TermMode::BRACKETED_PASTE, "bracketed_paste"),
        (TermMode::SGR_MOUSE, "sgr_mouse"),
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
#[derive(Copy, Clone)]
enum MouseEncodingAction {
    Move,
    Click(IpcMouseButton, usize),
    Down(IpcMouseButton),
    Up(IpcMouseButton),
    Drag(IpcMouseButton),
    Scroll(f64, f64),
}

#[cfg(unix)]
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

#[cfg(unix)]
fn append_mouse_report(
    output: &mut Vec<u8>,
    mode: TermMode,
    column: usize,
    row: usize,
    button: u8,
    pressed: bool,
) -> Result<(), IpcError> {
    if mode.contains(TermMode::SGR_MOUSE) {
        let terminator = if pressed { 'M' } else { 'm' };
        output.extend_from_slice(
            format!("\x1b[<{button};{};{}{terminator}", column + 1, row + 1).as_bytes(),
        );
        return Ok(());
    }

    let button = if pressed { button } else { 3 + (button & (4 | 8 | 16)) };
    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    let maximum = if utf8 { 2015 } else { 223 };
    if column >= maximum || row >= maximum {
        return Err(IpcError::new(
            "limit_exceeded",
            "mouse coordinate exceeds the active terminal mouse protocol",
        ));
    }
    output.extend_from_slice(&[b'\x1b', b'[', b'M', 32 + button]);
    append_legacy_mouse_coordinate(output, column, utf8);
    append_legacy_mouse_coordinate(output, row, utf8);
    Ok(())
}

#[cfg(unix)]
fn append_legacy_mouse_coordinate(output: &mut Vec<u8>, coordinate: usize, utf8: bool) {
    let encoded = coordinate + 33;
    if utf8 && coordinate >= 95 {
        output.push((0xc0 + encoded / 64) as u8);
        output.push((0x80 + (encoded & 63)) as u8);
    } else {
        output.push(encoded as u8);
    }
}
