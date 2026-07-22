//! Process window events.

use crate::ConfigMonitor;
use std::borrow::Cow;
use std::cmp::min;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::ffi::OsStr;
use std::fmt::Debug;
#[cfg(not(windows))]
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use std::{env, f32, mem};

use ahash::RandomState;
use log::{debug, error, info, warn};
#[cfg(unix)]
use serde::de::DeserializeOwned;
use winit::application::ApplicationHandler;
use winit::event::{
    ElementState, Event as WinitEvent, Ime, Modifiers, MouseButton, StartCause,
    Touch as TouchEvent, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::WindowId;

use crate::terminal::event::{Event as TerminalEvent, EventListener, Notify};
use crate::terminal::event_loop::Notifier;
use crate::terminal::grid::{Dimensions, Scroll};
use crate::terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use crate::terminal::selection::{Selection, SelectionType};
use crate::terminal::term::search::{Match, RegexSearch};
use crate::terminal::term::{self, ClipboardType, Term, TermMode};
use crate::terminal::vte::ansi::NamedColor;

#[cfg(unix)]
use crate::automation::{AutomationHub, SubscriptionRequest};
#[cfg(unix)]
use crate::automation::{PendingWrite, WaitKind, Waiter};
#[cfg(unix)]
use crate::cli::ParsedOptions;
use crate::cli::{Options as CliOptions, WindowOptions};
use crate::clipboard::Clipboard;
use crate::config::font::FontSize;
use crate::config::ui_config::{HintAction, HintInternalAction};
use crate::config::{self, UiConfig};
#[cfg(not(windows))]
use crate::daemon::foreground_process_path;
use crate::daemon::spawn_daemon;
use crate::display::color::Rgb;
use crate::display::hint::HintMatch;
use crate::display::window::{ImeInhibitor, Window};
use crate::display::{Display, Preedit, SizeInfo};
use crate::input::{self, ActionContext as _, FONT_SIZE_STEP};
use crate::logging::{LOG_TARGET_CONFIG, LOG_TARGET_WINIT};
use crate::message_bar::{Message, MessageBuffer};
#[cfg(unix)]
use crate::polling::ipc::IpcRequest;
#[cfg(unix)]
use crate::polling::ipc::{IpcError, MAX_INPUT_BYTES, MAX_IPC_TEXT_BYTES};
use crate::scheduler::{Scheduler, TimerId, Topic};
use crate::vivid::VividService;
use crate::window_context::WindowContext;

/// Duration after the last user input until an unlimited search is performed.
pub const TYPING_SEARCH_DELAY: Duration = Duration::from_millis(500);

/// Maximum number of lines for the blocking search while still typing the search regex.
const MAX_SEARCH_WHILE_TYPING: Option<usize> = Some(1000);

/// Maximum number of search terms stored in the history.
const MAX_SEARCH_HISTORY_SIZE: usize = 255;

/// Touch zoom speed.
const TOUCH_ZOOM_FACTOR: f32 = 0.01;

/// Cooldown between invocations of the bell command.
const BELL_CMD_COOLDOWN: Duration = Duration::from_millis(100);

/// The event processor.
///
/// Stores some state from received events and dispatches actions when they are
/// triggered.
pub struct Processor {
    pub config_monitor: Option<ConfigMonitor>,

    clipboard: Clipboard,
    scheduler: Scheduler,
    initial_window_options: Option<WindowOptions>,
    initial_window_error: Option<Box<dyn Error>>,
    windows: HashMap<WindowId, WindowContext, RandomState>,
    proxy: EventLoopProxy<Event>,
    #[cfg(unix)]
    global_ipc_options: ParsedOptions,
    #[cfg(unix)]
    automation: AutomationHub,
    cli_options: CliOptions,
    config: Rc<UiConfig>,
}

impl Processor {
    /// Create a new event processor.
    pub fn new(
        config: UiConfig,
        cli_options: CliOptions,
        event_loop: &EventLoop<Event>,
    ) -> Processor {
        let proxy = event_loop.create_proxy();
        let scheduler = Scheduler::new(proxy.clone());
        let initial_window_options = Some(cli_options.window_options.clone());

        // Disable all device events, since we don't care about them.
        event_loop.listen_device_events(DeviceEvents::Never);

        // SAFETY: Since this takes a pointer to the winit event loop, it MUST be dropped first,
        // which is done in `loop_exiting`.
        let clipboard = unsafe { Clipboard::new(event_loop.display_handle().unwrap().as_raw()) };

        // Create a config monitor.
        //
        // The monitor watches the config file for changes and reloads it. Pending
        // config changes are processed in the main loop.
        let mut config_monitor = None;
        if config.live_config_reload() {
            config_monitor =
                ConfigMonitor::new(config.config_paths.clone(), event_loop.create_proxy());
        }

        Processor {
            initial_window_options,
            initial_window_error: None,
            cli_options,
            proxy,
            scheduler,
            config: Rc::new(config),
            clipboard,
            windows: Default::default(),
            #[cfg(unix)]
            global_ipc_options: Default::default(),
            #[cfg(unix)]
            automation: Default::default(),
            config_monitor,
        }
    }

    /// Create the initial window and its Vello/wgpu surface.
    pub fn create_initial_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_options: WindowOptions,
    ) -> Result<u64, Box<dyn Error>> {
        let mut window_context = WindowContext::initial(
            event_loop,
            self.proxy.clone(),
            self.config.clone(),
            window_options,
        )?;

        #[cfg(unix)]
        {
            window_context.automation.creation_index = self.automation.next_creation_index();
        }
        let platform_id = window_context.id();
        #[cfg(unix)]
        let ipc_window_id = window_context.ipc_window_id();
        #[cfg(not(unix))]
        let ipc_window_id = u64::from(platform_id);
        self.windows.insert(platform_id, window_context);

        #[cfg(unix)]
        self.automation.emit(
            Some(ipc_window_id),
            "window_created",
            serde_json::json!({"window_id": ipc_window_id}),
        );

        Ok(ipc_window_id)
    }

    /// Create a new terminal window.
    pub fn create_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        options: WindowOptions,
    ) -> Result<u64, Box<dyn Error>> {
        #[cfg(unix)]
        if let Some(ipc_window_id) = options.ipc_window_id
            && self
                .windows
                .values()
                .any(|window_context| window_context.ipc_window_id() == ipc_window_id)
        {
            return Err(std::io::Error::other(format!(
                "IPC window ID {ipc_window_id} is already in use"
            ))
            .into());
        }

        // Override config with CLI/IPC options.
        let mut config_overrides = options.config_overrides();
        #[cfg(unix)]
        config_overrides.extend_from_slice(&self.global_ipc_options);
        let mut config = self.config.clone();
        config = config_overrides.override_config_rc(config);

        let mut window_context = WindowContext::additional(
            event_loop,
            self.proxy.clone(),
            config,
            options,
            config_overrides,
        )?;

        #[cfg(unix)]
        if self
            .windows
            .values()
            .any(|existing| existing.ipc_window_id() == window_context.ipc_window_id())
        {
            return Err(std::io::Error::other(format!(
                "IPC window ID {} is already in use",
                window_context.ipc_window_id()
            ))
            .into());
        }

        #[cfg(unix)]
        {
            window_context.automation.creation_index = self.automation.next_creation_index();
        }
        let platform_id = window_context.id();
        #[cfg(unix)]
        let ipc_window_id = window_context.ipc_window_id();
        #[cfg(not(unix))]
        let ipc_window_id = u64::from(platform_id);
        self.windows.insert(platform_id, window_context);
        #[cfg(unix)]
        self.automation.emit(
            Some(ipc_window_id),
            "window_created",
            serde_json::json!({"window_id": ipc_window_id}),
        );
        Ok(ipc_window_id)
    }

    /// Run the event loop.
    ///
    /// The result is exit code generate from the loop.
    pub fn run(&mut self, event_loop: EventLoop<Event>) -> Result<(), Box<dyn Error>> {
        let result = event_loop.run_app(self);
        match self.initial_window_error.take() {
            Some(initial_window_error) => Err(initial_window_error),
            _ => result.map_err(Into::into),
        }
    }

    /// Check if an event is irrelevant and can be skipped.
    fn skip_window_event(event: &WindowEvent) -> bool {
        matches!(
            event,
            WindowEvent::KeyboardInput { is_synthetic: true, .. }
                | WindowEvent::ActivationTokenDone { .. }
                | WindowEvent::DoubleTapGesture { .. }
                | WindowEvent::TouchpadPressure { .. }
                | WindowEvent::RotationGesture { .. }
                | WindowEvent::CursorEntered { .. }
                | WindowEvent::PinchGesture { .. }
                | WindowEvent::AxisMotion { .. }
                | WindowEvent::PanGesture { .. }
                | WindowEvent::HoveredFileCancelled
                | WindowEvent::Destroyed
                | WindowEvent::ThemeChanged(_)
                | WindowEvent::HoveredFile(_)
                | WindowEvent::Moved(_)
        )
    }

    /// Resolve the public stable window ID or focused-window fallback.
    #[cfg(unix)]
    fn resolve_ipc_target(&self, requested: Option<u64>) -> Result<WindowId, IpcError> {
        match requested {
            Some(requested) => self
                .windows
                .iter()
                .find_map(|(id, window)| (window.ipc_window_id() == requested).then_some(*id))
                .ok_or_else(|| {
                    IpcError::new(
                        "window_not_found",
                        format!("no Vivido window with ID {requested}"),
                    )
                }),
            None => self
                .windows
                .iter()
                .find_map(|(id, window)| window.is_focused().then_some(*id))
                .ok_or_else(|| IpcError::new("no_focused_window", "no focused Vivido window")),
        }
    }

    #[cfg(unix)]
    fn handle_ipc_request(&mut self, event_loop: &ActiveEventLoop, request: IpcRequest) {
        use crate::cli::{
            IpcConfig, IpcGetConfig, IpcGetGrid, IpcGetText, IpcInputRoute, IpcKey, IpcMouse,
            IpcPaste, IpcResize, IpcScreenshot, IpcSignal, IpcSubscribe, IpcTarget, IpcTranscript,
            IpcTyping, IpcWaitCommon, IpcWaitFrame, IpcWaitOutput, IpcWaitSequence, IpcWaitStable,
            IpcWaitText, WindowOptions,
        };

        let result = match request.method.as_str() {
            "ping" => {
                request.connection.reply(request.id, serde_json::json!({"pong": true}));
                return;
            },
            "unsubscribe" => {
                #[derive(serde::Deserialize)]
                struct Params {
                    subscription_id: u64,
                }
                let params: Params = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                if self.automation.unsubscribe(request.connection.id(), params.subscription_id) {
                    request.connection.reply(request.id, serde_json::json!({}));
                } else {
                    request.connection.error(
                        request.id,
                        IpcError::new("invalid_params", "unknown subscription ID"),
                    );
                }
                return;
            },
            "create_window" => {
                let options: WindowOptions = match decode_ipc_params(&request) {
                    Ok(options) => options,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let created = if self.windows.is_empty() {
                    self.create_initial_window(event_loop, options)
                } else {
                    self.create_window(event_loop, options)
                };
                match created {
                    Ok(window_id) => request
                        .connection
                        .reply(request.id, serde_json::json!({"window_id": window_id})),
                    Err(error) => request.connection.error(
                        request.id,
                        IpcError::new(
                            "invalid_params",
                            format!("failed to create window: {error}"),
                        ),
                    ),
                }
                return;
            },
            "config" => {
                let params: IpcConfig = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let requested = match params.window_id {
                    Some(-1) | None => None,
                    Some(id) => match u64::try_from(id) {
                        Ok(id) => Some(id),
                        Err(_) => {
                            request.connection.error(
                                request.id,
                                IpcError::new("invalid_params", "window ID must be -1 or unsigned"),
                            );
                            return;
                        },
                    },
                };
                let mut options = ParsedOptions::from_options(&params.options);
                let mut matched = requested.is_none();
                for window in self
                    .windows
                    .values_mut()
                    .filter(|window| requested.is_none_or(|id| id == window.ipc_window_id()))
                {
                    matched = true;
                    if params.reset {
                        window.reset_window_config(self.config.clone());
                    } else {
                        window.add_window_config(self.config.clone(), &options);
                    }
                }
                if matched {
                    if requested.is_none() {
                        if params.reset {
                            self.global_ipc_options.clear();
                        } else {
                            self.global_ipc_options.append(&mut options);
                        }
                    }
                    Ok(serde_json::json!({}))
                } else {
                    Err(IpcError::new("window_not_found", "configuration target does not exist"))
                }
            },
            "get_config" => {
                let params: IpcGetConfig = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let config = match params.window_id {
                    Some(-1) | None => {
                        self.global_ipc_options.override_config_rc(self.config.clone())
                    },
                    Some(id) => {
                        let id = match u64::try_from(id) {
                            Ok(id) => id,
                            Err(_) => {
                                request.connection.error(
                                    request.id,
                                    IpcError::new(
                                        "invalid_params",
                                        "window ID must be -1 or unsigned",
                                    ),
                                );
                                return;
                            },
                        };
                        match self.windows.values().find(|window| window.ipc_window_id() == id) {
                            Some(window) => Rc::new(window.config().clone()),
                            None => {
                                request.connection.error(
                                    request.id,
                                    IpcError::new(
                                        "window_not_found",
                                        format!("no Vivido window with ID {id}"),
                                    ),
                                );
                                return;
                            },
                        }
                    },
                };
                match serde_json::to_value(&*config) {
                    Ok(config) => Ok(serde_json::json!({"config": config})),
                    Err(error) => Err(IpcError::new(
                        "unsupported",
                        format!("failed to serialize configuration: {error}"),
                    )),
                }
            },
            "typing" => {
                let params: IpcTyping = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                self.queue_ipc_input(params.window_id, params.text.into_bytes(), &request);
                return;
            },
            "key" => {
                let params: IpcKey = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let target = match self.resolve_ipc_target(params.target.window_id) {
                    Ok(target) => target,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                if !(1..=1000).contains(&params.repeat) {
                    request.connection.error(
                        request.id,
                        IpcError::new("invalid_params", "key repeat must be 1 through 1000"),
                    );
                    return;
                }
                let mut bytes = Vec::new();
                for repeat_index in 0..params.repeat {
                    let encoded = match params.route {
                        IpcInputRoute::Application => crate::input::keyboard::encode_ipc_key_event(
                            &params.key,
                            &params.mods,
                            self.windows[&target].terminal_mode(),
                            repeat_index > 0,
                        ),
                        IpcInputRoute::Ui => self.windows.get_mut(&target).unwrap().ui_key(
                            &params,
                            repeat_index > 0,
                            #[cfg(target_os = "macos")]
                            event_loop,
                            &self.proxy,
                            &mut self.clipboard,
                            &mut self.scheduler,
                        ),
                    };
                    match encoded {
                        Ok(encoded) => bytes.extend(encoded),
                        Err(error) => {
                            request.connection.error(request.id, error);
                            return;
                        },
                    }
                }
                if bytes.is_empty() {
                    request.connection.reply(request.id, serde_json::json!({"written_bytes": 0}));
                } else {
                    let window_id = self.windows[&target].ipc_window_id();
                    self.queue_ipc_input(Some(window_id), bytes, &request);
                }
                return;
            },
            "paste" => {
                let params: IpcPaste = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                if params.text.len() > MAX_INPUT_BYTES {
                    Err(IpcError::new("limit_exceeded", "paste payload exceeds 1 MiB"))
                } else {
                    let target = match self.resolve_ipc_target(params.target.window_id) {
                        Ok(target) => target,
                        Err(error) => {
                            request.connection.error(request.id, error);
                            return;
                        },
                    };
                    let bytes = match params.route {
                        IpcInputRoute::Application => {
                            self.windows[&target].application_paste(&params.text)
                        },
                        IpcInputRoute::Ui => self.windows.get_mut(&target).unwrap().ui_paste(
                            &params.text,
                            #[cfg(target_os = "macos")]
                            event_loop,
                            &self.proxy,
                            &mut self.clipboard,
                            &mut self.scheduler,
                        ),
                    };
                    if bytes.is_empty() {
                        request.connection.reply(request.id, serde_json::json!({}));
                    } else {
                        let window_id = self.windows[&target].ipc_window_id();
                        self.queue_ipc_input(Some(window_id), bytes, &request);
                    }
                    return;
                }
            },
            "mouse" => {
                let params: IpcMouse = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let position = match &params.action {
                    crate::cli::IpcMouseAction::Move(position) => position,
                    crate::cli::IpcMouseAction::Click(action)
                    | crate::cli::IpcMouseAction::DoubleClick(action)
                    | crate::cli::IpcMouseAction::Down(action)
                    | crate::cli::IpcMouseAction::Up(action)
                    | crate::cli::IpcMouseAction::Drag(action) => &action.position,
                    crate::cli::IpcMouseAction::Scroll(action) => &action.position,
                };
                let target = match self.resolve_ipc_target(position.target.window_id) {
                    Ok(target) => target,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                match position.route {
                    IpcInputRoute::Application => {
                        match self.windows[&target].application_mouse(&params) {
                            Ok(bytes) => {
                                let window_id = self.windows[&target].ipc_window_id();
                                self.queue_ipc_input(Some(window_id), bytes, &request);
                            },
                            Err(error) => request.connection.error(request.id, error),
                        }
                    },
                    IpcInputRoute::Ui => {
                        let result = self.windows.get_mut(&target).unwrap().ui_mouse(
                            &params,
                            #[cfg(target_os = "macos")]
                            event_loop,
                            &self.proxy,
                            &mut self.clipboard,
                            &mut self.scheduler,
                        );
                        match result {
                            Ok(bytes) if bytes.is_empty() => {
                                request.connection.reply(request.id, serde_json::json!({}));
                            },
                            Ok(bytes) => {
                                let window_id = self.windows[&target].ipc_window_id();
                                self.queue_ipc_input(Some(window_id), bytes, &request);
                            },
                            Err(error) => request.connection.error(request.id, error),
                        }
                    },
                }
                return;
            },
            "resize" => {
                let params: IpcResize = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let target = match self.resolve_ipc_target(params.target.window_id) {
                    Ok(target) => target,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                if self.windows[&target]
                    .automation
                    .waiters
                    .iter()
                    .any(|waiter| matches!(waiter.kind, WaitKind::Resize { .. }))
                {
                    Err(IpcError::new(
                        "limit_exceeded",
                        "a resize is already pending for this window",
                    ))
                } else {
                    let after_resize = self.windows[&target].automation.resize_confirmation;
                    match self.windows[&target].request_automation_resize(
                        params.columns,
                        params.rows,
                        params.width,
                        params.height,
                    ) {
                        Ok((width, height, grid)) => {
                            let (columns, rows) =
                                grid.map_or((None, None), |(c, r)| (Some(c), Some(r)));
                            self.register_wait_for_target(
                                target,
                                5_000,
                                WaitKind::Resize {
                                    columns,
                                    rows,
                                    width,
                                    height,
                                    after_resize,
                                    pty_token: None,
                                    pty_complete: false,
                                },
                                &request,
                            );
                            return;
                        },
                        Err(error) => Err(error),
                    }
                }
            },
            "signal" => {
                let params: IpcSignal = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                self.resolve_ipc_target(params.target.window_id).and_then(|target| {
                    self.windows[&target]
                        .signal_process_group(params.signal)
                        .map(|process_group| serde_json::json!({"process_group_id": process_group}))
                })
            },
            "get_text" => {
                let params: IpcGetText = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                if params.rows.is_some_and(|rows| rows == 0 || rows > 1000) {
                    Err(IpcError::new("invalid_params", "rows must be 1 through 1000"))
                } else {
                    self.resolve_ipc_target(params.window_id).and_then(|id| {
                        let text = self.windows[&id].text(params.rows);
                        if text.len() > MAX_IPC_TEXT_BYTES {
                            Err(IpcError::new("limit_exceeded", "terminal text exceeds 16 MiB"))
                        } else {
                            Ok(serde_json::json!({"text": text}))
                        }
                    })
                }
            },
            "screenshot" => {
                let params: IpcScreenshot = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let target = match self.resolve_ipc_target(params.window_id) {
                    Ok(target) => target,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                match self.windows.get_mut(&target).unwrap().request_screenshot(
                    request.connection.clone(),
                    request.id,
                    &mut self.scheduler,
                ) {
                    Ok(()) => return,
                    Err(message) => Err(IpcError::new("unsupported", message)),
                }
            },
            "list_windows" => {
                let mut windows: Vec<_> =
                    self.windows.values().map(WindowContext::automation_summary).collect();
                windows.sort_by_key(|window| window["creation_index"].as_u64().unwrap_or(0));
                Ok(serde_json::json!({"windows": windows}))
            },
            "inspect" => {
                let params: IpcTarget = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                self.resolve_ipc_target(params.window_id).map(|target| {
                    self.windows[&target].automation_inspect(self.automation.event_sequence())
                })
            },
            "get_grid" => {
                let params: IpcGetGrid = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                self.resolve_ipc_target(params.target.window_id).and_then(|target| {
                    self.windows[&target].automation_grid(
                        params.start_line,
                        params.row_count,
                        params.since_screen,
                    )
                })
            },
            "transcript" => {
                let params: IpcTranscript = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                if params.max_bytes == 0
                    || params.max_bytes as usize > crate::automation::TRANSCRIPT_CAPACITY
                {
                    Err(IpcError::new("invalid_params", "max_bytes must be 1 through 1048576"))
                } else {
                    self.resolve_ipc_target(params.target.window_id).and_then(|target| {
                        self.windows[&target]
                            .automation
                            .transcript
                            .lock()
                            .unwrap()
                            .snapshot(params.after_offset, params.max_bytes as usize)
                            .map(|snapshot| snapshot.json())
                    })
                }
            },
            "subscribe" => {
                let params: IpcSubscribe = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let target = if params.all {
                    None
                } else {
                    match self.resolve_ipc_target(params.window_id) {
                        Ok(target) => Some(self.windows[&target].ipc_window_id()),
                        Err(error) => {
                            request.connection.error(request.id, error);
                            return;
                        },
                    }
                };
                let current_sequences = serde_json::Value::Array(
                    self.windows
                        .values()
                        .map(|window| {
                            serde_json::json!({
                                "window_id": window.ipc_window_id(),
                                "screen_sequence": window.automation.screen_sequence,
                                "frame_sequence": window.automation.frame_sequence,
                                "output_offset": window
                                    .automation
                                    .transcript
                                    .lock()
                                    .unwrap()
                                    .end_offset(),
                            })
                        })
                        .collect(),
                );
                let kinds = params.events.into_iter().collect();
                match self.automation.subscribe(
                    request.connection.clone(),
                    request.id,
                    SubscriptionRequest {
                        target,
                        all_windows: params.all,
                        kinds,
                        since_event: params.since_event,
                        current_sequences,
                    },
                ) {
                    Ok(_) => (),
                    Err(error) => request.connection.error(request.id, error),
                }
                return;
            },
            "wait_text" => {
                let params: IpcWaitText = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                if params.text.len() > 8192 {
                    Err(IpcError::new("limit_exceeded", "wait pattern exceeds 8 KiB"))
                } else if params.regex {
                    match compile_regex(&params.text) {
                        Ok(()) => {
                            self.register_wait(
                                params.common.target.window_id,
                                params.common.timeout,
                                WaitKind::Text {
                                    pattern: params.text,
                                    regex: true,
                                    after_screen: params.after_screen,
                                },
                                &request,
                            );
                            return;
                        },
                        Err(error) => Err(error),
                    }
                } else {
                    self.register_wait(
                        params.common.target.window_id,
                        params.common.timeout,
                        WaitKind::Text {
                            pattern: params.text,
                            regex: false,
                            after_screen: params.after_screen,
                        },
                        &request,
                    );
                    return;
                }
            },
            "wait_output" => {
                let params: IpcWaitOutput = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let pattern = if params.base64 {
                    use base64::Engine;
                    match base64::engine::general_purpose::STANDARD.decode(&params.pattern) {
                        Ok(pattern) => pattern,
                        Err(error) => {
                            request.connection.error(
                                request.id,
                                IpcError::new(
                                    "invalid_params",
                                    format!("invalid base64 output pattern: {error}"),
                                ),
                            );
                            return;
                        },
                    }
                } else {
                    params.pattern.into_bytes()
                };
                let regex_validation = params
                    .regex
                    .then(|| {
                        std::str::from_utf8(&pattern)
                            .map_err(|error| IpcError::new("regex_invalid", error.to_string()))
                            .and_then(compile_regex)
                    })
                    .transpose();
                if pattern.len() > 8192 {
                    Err(IpcError::new("limit_exceeded", "wait pattern exceeds 8 KiB"))
                } else if let Err(error) = regex_validation {
                    Err(error)
                } else {
                    let target = match self.resolve_ipc_target(params.common.target.window_id) {
                        Ok(target) => target,
                        Err(error) => {
                            request.connection.error(request.id, error);
                            return;
                        },
                    };
                    let transcript = self.windows[&target].automation.transcript.lock().unwrap();
                    let start_offset =
                        params.after_offset.unwrap_or_else(|| transcript.end_offset());
                    if let Err(error) = transcript.range(start_offset, 0) {
                        request.connection.error(request.id, error);
                        return;
                    }
                    drop(transcript);
                    self.register_wait_for_target(
                        target,
                        params.common.timeout,
                        WaitKind::Output { pattern, regex: params.regex, start_offset },
                        &request,
                    );
                    return;
                }
            },
            "wait_screen_change" => {
                let params: IpcWaitSequence = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let target = match self.resolve_ipc_target(params.common.target.window_id) {
                    Ok(target) => target,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let after =
                    params.after_screen.unwrap_or(self.windows[&target].automation.screen_sequence);
                self.register_wait_for_target(
                    target,
                    params.common.timeout,
                    WaitKind::ScreenChange { after },
                    &request,
                );
                return;
            },
            "wait_screen_stable" => {
                let params: IpcWaitStable = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                if params.quiet == 0 || params.quiet > 86_400_000 {
                    Err(IpcError::new(
                        "invalid_params",
                        "quiet duration must be 1 ms through 24 hours",
                    ))
                } else {
                    self.register_wait(
                        params.common.target.window_id,
                        params.common.timeout,
                        WaitKind::ScreenStable {
                            quiet: Duration::from_millis(params.quiet),
                            after_screen: params.after_screen,
                        },
                        &request,
                    );
                    return;
                }
            },
            "wait_frame" => {
                let params: IpcWaitFrame = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let target = match self.resolve_ipc_target(params.common.target.window_id) {
                    Ok(target) => target,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                let after =
                    params.after_frame.unwrap_or(self.windows[&target].automation.frame_sequence);
                self.register_wait_for_target(
                    target,
                    params.common.timeout,
                    WaitKind::Frame { after },
                    &request,
                );
                return;
            },
            "wait_exit" => {
                let params: IpcWaitCommon = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                self.register_wait(
                    params.target.window_id,
                    params.timeout,
                    WaitKind::Exit,
                    &request,
                );
                return;
            },
            "focus" => {
                let params: IpcTarget = match decode_ipc_params(&request) {
                    Ok(params) => params,
                    Err(error) => {
                        request.connection.error(request.id, error);
                        return;
                    },
                };
                match self.resolve_ipc_target(params.window_id) {
                    Ok(target) => {
                        if self.windows[&target]
                            .automation
                            .waiters
                            .iter()
                            .any(|waiter| matches!(waiter.kind, WaitKind::Focus { .. }))
                        {
                            Err(IpcError::new(
                                "limit_exceeded",
                                "a focus request is already pending for this window",
                            ))
                        } else {
                            let after_focus = self.windows[&target].automation.focus_confirmation;
                            self.windows[&target].request_automation_focus();
                            self.register_wait_for_target(
                                target,
                                2_000,
                                WaitKind::Focus { after_focus },
                                &request,
                            );
                            return;
                        }
                    },
                    Err(error) => Err(error),
                }
            },
            _ => Err(IpcError::new(
                "unsupported",
                format!("unsupported IPC method {:?}", request.method),
            )),
        };

        match result {
            Ok(result) => request.connection.reply(request.id, result),
            Err(error) => request.connection.error(request.id, error),
        }
    }

    #[cfg(unix)]
    fn queue_ipc_input(&mut self, requested: Option<u64>, bytes: Vec<u8>, request: &IpcRequest) {
        if bytes.len() > MAX_INPUT_BYTES + 16 {
            request
                .connection
                .error(request.id, IpcError::new("limit_exceeded", "terminal input exceeds 1 MiB"));
            return;
        }
        if bytes.is_empty() {
            request.connection.reply(request.id, serde_json::json!({"written_bytes": 0}));
            return;
        }
        let target = match self.resolve_ipc_target(requested) {
            Ok(target) => target,
            Err(error) => {
                request.connection.error(request.id, error);
                return;
            },
        };
        let token = self.automation.next_write_token();
        let window = self.windows.get_mut(&target).unwrap();
        let length = bytes.len();
        if let Err(error) = window.write_to_pty_with_completion(bytes, token) {
            warn!("failed to queue IPC terminal input: {error}");
            request
                .connection
                .error(request.id, IpcError::new("pty_closed", "failed to queue terminal input"));
            return;
        }
        window.automation.pending_writes.push(PendingWrite {
            token,
            bytes: length,
            connection: request.connection.clone(),
            request_id: request.id,
            deadline: Instant::now() + Duration::from_secs(5),
        });
        self.schedule_automation_timer(target);
    }

    #[cfg(unix)]
    fn handle_ipc_disconnect(&mut self, connection_id: u64) {
        self.automation.disconnect(connection_id);
        for window in self.windows.values_mut() {
            if window.cancel_automation_connection(connection_id) {
                self.scheduler.unschedule(TimerId::new(Topic::ScreenshotReadback, window.id()));
            }
        }
        let window_ids: Vec<_> = self.windows.keys().copied().collect();
        for window_id in window_ids {
            self.schedule_automation_timer(window_id);
        }
    }

    #[cfg(unix)]
    fn register_wait(
        &mut self,
        requested: Option<u64>,
        timeout_ms: u64,
        kind: WaitKind,
        request: &IpcRequest,
    ) {
        match self.resolve_ipc_target(requested) {
            Ok(target) => self.register_wait_for_target(target, timeout_ms, kind, request),
            Err(error) => request.connection.error(request.id, error),
        }
    }

    #[cfg(unix)]
    fn register_wait_for_target(
        &mut self,
        target: WindowId,
        timeout_ms: u64,
        kind: WaitKind,
        request: &IpcRequest,
    ) {
        if !(1..=86_400_000).contains(&timeout_ms) {
            request.connection.error(
                request.id,
                IpcError::new("invalid_params", "timeout must be 1 ms through 24 hours"),
            );
            return;
        }
        self.windows.get_mut(&target).unwrap().automation.waiters.push(Waiter {
            connection: request.connection.clone(),
            request_id: request.id,
            deadline: Instant::now() + Duration::from_millis(timeout_ms),
            kind,
        });
        self.evaluate_waiters(target);
        self.schedule_automation_timer(target);
    }

    #[cfg(unix)]
    fn automation_tick(&mut self, window_id: WindowId) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        let now = Instant::now();
        let mut index = 0;
        while index < window.automation.pending_writes.len() {
            if window.automation.pending_writes[index].deadline <= now {
                let pending = window.automation.pending_writes.swap_remove(index);
                pending.connection.error(
                    pending.request_id,
                    IpcError::new("timeout", "PTY write did not complete within five seconds"),
                );
            } else {
                index += 1;
            }
        }
        let _ = window;
        self.evaluate_waiters(window_id);
        self.schedule_automation_timer(window_id);
    }

    /// Keep exactly one timer at the nearest automation deadline for this window.
    #[cfg(unix)]
    fn schedule_automation_timer(&mut self, window_id: WindowId) {
        let timer_id = TimerId::new(Topic::Automation, window_id);
        self.scheduler.unschedule(timer_id);
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };
        let mut deadline = window
            .automation
            .pending_writes
            .iter()
            .map(|pending| pending.deadline)
            .chain(window.automation.waiters.iter().map(|waiter| waiter.deadline))
            .min();
        for waiter in &window.automation.waiters {
            if let WaitKind::ScreenStable { quiet, after_screen } = &waiter.kind {
                let eligible =
                    after_screen.is_none_or(|after| window.automation.screen_sequence > after);
                if eligible {
                    let stable = window.automation.last_screen_change + *quiet;
                    deadline = Some(deadline.map_or(stable, |current| current.min(stable)));
                }
            }
        }
        let Some(deadline) = deadline else {
            return;
        };
        self.scheduler.schedule(
            Event::new(EventType::AutomationTick, window_id),
            deadline.saturating_duration_since(Instant::now()),
            false,
            timer_id,
        );
    }

    /// Apply focus/resize confirmations after batched winit events have updated window state.
    #[cfg(unix)]
    fn apply_automation_confirmations(&mut self, window_id: WindowId) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        window.automation.focus_confirmation = window
            .automation
            .focus_confirmation
            .saturating_add(std::mem::take(&mut window.automation.pending_focus_confirmations));
        let resize_confirmations =
            std::mem::take(&mut window.automation.pending_resize_confirmations);
        window.automation.resize_confirmation =
            window.automation.resize_confirmation.saturating_add(resize_confirmations);
        if resize_confirmations == 0 {
            return;
        }

        let pending = window.automation.waiters.iter().find_map(|waiter| {
            if let WaitKind::Resize {
                columns,
                rows,
                width,
                height,
                after_resize,
                pty_token: None,
                ..
            } = &waiter.kind
                && window.automation.resize_confirmation > *after_resize
                && window.automation_size_matches(*columns, *rows, *width, *height)
            {
                Some(waiter.request_id)
            } else {
                None
            }
        });
        let Some(request_id) = pending else {
            return;
        };
        let token = self.automation.next_write_token();
        match window.write_pty_resize_with_completion(token) {
            Ok(()) => {
                if let Some(waiter) = window
                    .automation
                    .waiters
                    .iter_mut()
                    .find(|waiter| waiter.request_id == request_id)
                    && let WaitKind::Resize { pty_token, .. } = &mut waiter.kind
                {
                    *pty_token = Some(token);
                }
            },
            Err(_) => {
                if let Some(index) = window
                    .automation
                    .waiters
                    .iter()
                    .position(|waiter| waiter.request_id == request_id)
                {
                    let waiter = window.automation.waiters.swap_remove(index);
                    waiter.connection.error(
                        waiter.request_id,
                        IpcError::new("pty_closed", "failed to apply terminal resize"),
                    );
                }
            },
        }
    }

    #[cfg(unix)]
    fn evaluate_waiters(&mut self, window_id: WindowId) {
        use std::os::unix::process::ExitStatusExt;

        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        let now = Instant::now();
        let waiters = std::mem::take(&mut window.automation.waiters);
        let visible_text = window.text(None);
        let screen_sequence = window.automation.screen_sequence;
        let frame_sequence = window.automation.frame_sequence;
        let last_screen_change = window.automation.last_screen_change;
        let exit_status = window.automation.exit_status;

        for waiter in waiters {
            if !waiter.connection.is_alive() {
                continue;
            }
            if waiter.deadline <= now {
                let error = match &waiter.kind {
                    WaitKind::Resize { columns, rows, width, height, .. } => {
                        let size = window.display.size_info;
                        let pixels = window.display.window.inner_size();
                        IpcError::new(
                            "resize_mismatch",
                            "window did not reach the requested size within five seconds",
                        )
                        .with_data(serde_json::json!({
                            "requested": {
                                "columns": columns,
                                "rows": rows,
                                "width": width,
                                "height": height,
                            },
                            "actual": {
                                "columns": size.columns(),
                                "rows": size.screen_lines(),
                                "width": pixels.width,
                                "height": pixels.height,
                            },
                        }))
                    },
                    WaitKind::Focus { .. } => IpcError::new(
                        "focus_denied",
                        "window system did not confirm focus within two seconds",
                    ),
                    _ => IpcError::new("timeout", "IPC wait timed out"),
                };
                waiter.connection.error(waiter.request_id, error);
                continue;
            }

            let result = match &waiter.kind {
                WaitKind::Text { pattern, regex, after_screen } => {
                    let eligible = after_screen.is_none_or(|after| screen_sequence > after);
                    if eligible
                        && pattern_find(visible_text.as_bytes(), pattern.as_bytes(), *regex)
                            .is_some()
                    {
                        Some(Ok(serde_json::json!({
                            "matched": true,
                            "screen_sequence": screen_sequence,
                        })))
                    } else {
                        None
                    }
                },
                WaitKind::Output { pattern, regex, start_offset } => {
                    let transcript = window.automation.transcript.lock().unwrap();
                    match transcript.range(*start_offset, crate::automation::TRANSCRIPT_CAPACITY) {
                        Ok(bytes) => pattern_find(&bytes, pattern, *regex).map(|(start, end)| {
                            Ok(serde_json::json!({
                                "matched": true,
                                "start_offset": start_offset.saturating_add(start as u64),
                                "end_offset": start_offset.saturating_add(end as u64),
                                "output_offset": transcript.end_offset(),
                            }))
                        }),
                        Err(error) => Some(Err(error)),
                    }
                },
                WaitKind::ScreenChange { after } => (screen_sequence > *after)
                    .then(|| Ok(serde_json::json!({"screen_sequence": screen_sequence}))),
                WaitKind::ScreenStable { quiet, after_screen } => {
                    let eligible = after_screen.is_none_or(|after| screen_sequence > after);
                    (eligible && now.duration_since(last_screen_change) >= *quiet).then(|| {
                        Ok(serde_json::json!({
                            "screen_sequence": screen_sequence,
                            "stable_for_ms": now.duration_since(last_screen_change).as_millis(),
                        }))
                    })
                },
                WaitKind::Frame { after } => (frame_sequence > *after)
                    .then(|| Ok(serde_json::json!({"frame_sequence": frame_sequence}))),
                WaitKind::Exit => exit_status.map(|status| {
                    Ok(serde_json::json!({
                        "exited": true,
                        "code": status.code(),
                        "signal": status.signal(),
                        "core_dumped": status.core_dumped(),
                    }))
                }),
                WaitKind::Resize {
                    columns,
                    rows,
                    width,
                    height,
                    after_resize,
                    pty_complete,
                    ..
                } => {
                    let size = window.display.size_info;
                    let pixels = window.display.window.inner_size();
                    let grid_matches = columns.is_none_or(|columns| {
                        size.columns() == usize::from(columns)
                            && rows.is_some_and(|rows| size.screen_lines() == usize::from(rows))
                    });
                    (window.automation.resize_confirmation > *after_resize
                        && *pty_complete
                        && grid_matches
                        && pixels.width == *width
                        && pixels.height == *height)
                        .then(|| {
                            Ok(serde_json::json!({
                                "columns": size.columns(),
                                "rows": size.screen_lines(),
                                "width": pixels.width,
                                "height": pixels.height,
                            }))
                        })
                },
                WaitKind::Focus { after_focus } => {
                    (window.automation.focus_confirmation > *after_focus && window.is_focused())
                        .then(|| Ok(serde_json::json!({"focused": true})))
                },
            };

            match result {
                Some(Ok(result)) => waiter.connection.reply(waiter.request_id, result),
                Some(Err(error)) => waiter.connection.error(waiter.request_id, error),
                None => window.automation.waiters.push(waiter),
            }
        }
    }
}

#[cfg(unix)]
fn decode_ipc_params<T: DeserializeOwned>(request: &IpcRequest) -> Result<T, IpcError> {
    serde_json::from_value(request.params.clone()).map_err(|error| {
        IpcError::new("invalid_params", format!("invalid {} parameters: {error}", request.method))
    })
}

#[cfg(unix)]
fn compile_regex(pattern: &str) -> Result<(), IpcError> {
    regex_automata::meta::Regex::new(pattern)
        .map(|_| ())
        .map_err(|error| IpcError::new("regex_invalid", error.to_string()))
}

#[cfg(unix)]
fn pattern_find(haystack: &[u8], needle: &[u8], regex: bool) -> Option<(usize, usize)> {
    if regex {
        let pattern = std::str::from_utf8(needle).ok()?;
        let regex = regex_automata::meta::Regex::new(pattern).ok()?;
        regex.find(haystack).map(|found| (found.start(), found.end()))
    } else if needle.is_empty() {
        Some((0, 0))
    } else {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|start| (start, start + needle.len()))
    }
}

impl ApplicationHandler<Event> for Processor {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if cause != StartCause::Init || self.cli_options.daemon {
            return;
        }

        if let Some(window_options) = self.initial_window_options.take()
            && let Err(err) = self.create_initial_window(event_loop, window_options)
        {
            self.initial_window_error = Some(err);
            event_loop.exit();
            return;
        }

        info!("Initialisation complete");
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.config.debug.print_events {
            info!(target: LOG_TARGET_WINIT, "{event:?}");
        }

        // Ignore all events we do not care about.
        if Self::skip_window_event(&event) {
            return;
        }

        let window_context = match self.windows.get_mut(&window_id) {
            Some(window_context) => window_context,
            None => return,
        };

        let is_redraw = matches!(event, WindowEvent::RedrawRequested);
        #[cfg(unix)]
        let focus_confirmed = matches!(&event, WindowEvent::Focused(true));
        #[cfg(unix)]
        let resize_confirmed = matches!(&event, WindowEvent::Resized(_));
        #[cfg(unix)]
        let automation_event = match &event {
            WindowEvent::Focused(focused) => {
                Some(("focus_changed", serde_json::json!({"focused": focused})))
            },
            WindowEvent::Resized(size) => {
                Some(("resized", serde_json::json!({"width": size.width, "height": size.height})))
            },
            _ => None,
        };

        window_context.handle_event(
            #[cfg(target_os = "macos")]
            _event_loop,
            &self.proxy,
            &mut self.clipboard,
            &mut self.scheduler,
            WinitEvent::WindowEvent { window_id, event },
        );

        #[cfg(unix)]
        if focus_confirmed {
            window_context.automation.pending_focus_confirmations =
                window_context.automation.pending_focus_confirmations.saturating_add(1);
        }

        #[cfg(unix)]
        if resize_confirmed {
            window_context.automation.pending_resize_confirmations =
                window_context.automation.pending_resize_confirmations.saturating_add(1);
        }

        #[cfg(unix)]
        if let Some((kind, data)) = automation_event {
            self.automation.emit(Some(window_context.ipc_window_id()), kind, data);
        }

        if is_redraw {
            let presented = window_context.draw(&mut self.scheduler);
            #[cfg(unix)]
            if presented {
                let ipc_window_id = window_context.ipc_window_id();
                let frame_sequence = window_context.automation.record_frame();
                self.automation.emit(
                    Some(ipc_window_id),
                    "frame_presented",
                    serde_json::json!({"frame_sequence": frame_sequence}),
                );
            }
        }

        #[cfg(unix)]
        {
            let _ = window_context;
            self.evaluate_waiters(window_id);
            self.schedule_automation_timer(window_id);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        if self.config.debug.print_events {
            info!(target: LOG_TARGET_WINIT, "{event:?}");
        }

        // Handle events which don't mandate the WindowId.
        match (event.payload, event.window_id.as_ref()) {
            #[cfg(unix)]
            (EventType::IpcRequest(request), _) => self.handle_ipc_request(event_loop, request),
            #[cfg(unix)]
            (EventType::IpcDisconnect(connection_id), _) => {
                self.handle_ipc_disconnect(connection_id);
            },
            #[cfg(unix)]
            (EventType::ScreenshotReadback, Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(window_id) {
                    window_context.poll_screenshot(&mut self.scheduler, &self.proxy);
                }
            },
            #[cfg(unix)]
            (EventType::ScreenshotComplete, Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(window_id) {
                    window_context.complete_screenshot();
                }
            },
            #[cfg(unix)]
            (EventType::AutomationTick, Some(window_id)) => self.automation_tick(*window_id),
            (EventType::ConfigReload(path), _) => {
                // Clear config logs from message bar for all terminals.
                for window_context in self.windows.values_mut() {
                    if !window_context.message_buffer.is_empty() {
                        window_context.message_buffer.remove_target(LOG_TARGET_CONFIG);
                        window_context.display.pending_update.dirty = true;
                    }
                }

                // Load config and update each terminal.
                if let Ok(config) = config::reload(&path, &mut self.cli_options) {
                    self.config = Rc::new(config);

                    // Restart config monitor if imports changed.
                    if let Some(monitor) = self.config_monitor.take() {
                        let paths = &self.config.config_paths;
                        self.config_monitor = if monitor.needs_restart(paths) {
                            monitor.shutdown();
                            ConfigMonitor::new(paths.clone(), self.proxy.clone())
                        } else {
                            Some(monitor)
                        };
                    }

                    for window_context in self.windows.values_mut() {
                        window_context.update_config(self.config.clone());
                    }
                }
            },
            // Create a new terminal window.
            (EventType::CreateWindow(options), _) => {
                if self.windows.is_empty() {
                    // Handle initial window creation in daemon mode.
                    if let Err(err) = self.create_initial_window(event_loop, options) {
                        self.initial_window_error = Some(err);
                        event_loop.exit();
                    }
                } else if let Err(err) = self.create_window(event_loop, options) {
                    error!("Could not open window: {err:?}");
                }
            },
            // Shutdown all windows.
            #[cfg(unix)]
            (EventType::Shutdown, _) => event_loop.exit(),
            // Process events affecting all windows.
            (payload, None) => {
                let event = WinitEvent::UserEvent(Event::new(payload, None));
                for window_context in self.windows.values_mut() {
                    window_context.handle_event(
                        #[cfg(target_os = "macos")]
                        event_loop,
                        &self.proxy,
                        &mut self.clipboard,
                        &mut self.scheduler,
                        event.clone(),
                    );
                }
            },
            #[cfg(unix)]
            (EventType::Terminal(TerminalEvent::Title(title)), Some(window_id)) => {
                self.automation.emit(
                    self.windows.get(window_id).map(WindowContext::ipc_window_id),
                    "title_changed",
                    serde_json::json!({"title": title}),
                );
                if let Some(window_context) = self.windows.get_mut(window_id) {
                    window_context.handle_event(
                        #[cfg(target_os = "macos")]
                        event_loop,
                        &self.proxy,
                        &mut self.clipboard,
                        &mut self.scheduler,
                        WinitEvent::UserEvent(Event::new(
                            EventType::Terminal(TerminalEvent::Title(title)),
                            *window_id,
                        )),
                    );
                }
            },
            #[cfg(unix)]
            (EventType::Terminal(TerminalEvent::ResetTitle), Some(window_id)) => {
                let title = self
                    .windows
                    .get(window_id)
                    .map(|window| window.config().window.identity.title.clone());
                self.automation.emit(
                    self.windows.get(window_id).map(WindowContext::ipc_window_id),
                    "title_changed",
                    serde_json::json!({"title": title}),
                );
                if let Some(window_context) = self.windows.get_mut(window_id) {
                    window_context.handle_event(
                        #[cfg(target_os = "macos")]
                        event_loop,
                        &self.proxy,
                        &mut self.clipboard,
                        &mut self.scheduler,
                        WinitEvent::UserEvent(Event::new(
                            EventType::Terminal(TerminalEvent::ResetTitle),
                            *window_id,
                        )),
                    );
                }
            },
            #[cfg(unix)]
            (EventType::Terminal(TerminalEvent::Bell), Some(window_id)) => {
                self.automation.emit(
                    self.windows.get(window_id).map(WindowContext::ipc_window_id),
                    "bell",
                    serde_json::json!({}),
                );
                if let Some(window_context) = self.windows.get_mut(window_id) {
                    window_context.handle_event(
                        #[cfg(target_os = "macos")]
                        event_loop,
                        &self.proxy,
                        &mut self.clipboard,
                        &mut self.scheduler,
                        WinitEvent::UserEvent(Event::new(
                            EventType::Terminal(TerminalEvent::Bell),
                            *window_id,
                        )),
                    );
                }
            },
            (EventType::Terminal(TerminalEvent::Wakeup), Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(window_id) {
                    window_context.dirty = true;
                    if window_context.display.window.has_frame {
                        window_context.display.window.request_redraw();
                    }
                }
            },
            #[cfg(unix)]
            (EventType::Terminal(TerminalEvent::PtyOutput { start, end }), Some(window_id)) => {
                if let Some(window) = self.windows.get(window_id) {
                    let transcript = window.automation.transcript.lock().unwrap();
                    if let Ok(bytes) = transcript.range(start, end.saturating_sub(start) as usize) {
                        self.automation.emit_output(window.ipc_window_id(), start, &bytes);
                    }
                }
                self.evaluate_waiters(*window_id);
                self.schedule_automation_timer(*window_id);
            },
            #[cfg(unix)]
            (EventType::Terminal(TerminalEvent::PtyWriteComplete(token)), Some(window_id)) => {
                if let Some(window) = self.windows.get_mut(window_id)
                    && let Some(index) = window
                        .automation
                        .pending_writes
                        .iter()
                        .position(|pending| pending.token == token)
                {
                    let pending = window.automation.pending_writes.swap_remove(index);
                    pending.connection.reply(
                        pending.request_id,
                        serde_json::json!({"written_bytes": pending.bytes}),
                    );
                }
                self.schedule_automation_timer(*window_id);
            },
            #[cfg(unix)]
            (EventType::Terminal(TerminalEvent::PtyResizeComplete(token)), Some(window_id)) => {
                if let Some(window) = self.windows.get_mut(window_id)
                    && let Some(waiter) = window.automation.waiters.iter_mut().find(|waiter| {
                        matches!(
                            waiter.kind,
                            WaitKind::Resize { pty_token: Some(expected), .. } if expected == token
                        )
                    })
                    && let WaitKind::Resize { pty_complete, .. } = &mut waiter.kind
                {
                    *pty_complete = true;
                }
                self.evaluate_waiters(*window_id);
                self.schedule_automation_timer(*window_id);
            },
            (EventType::VividFrame, Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(window_id) {
                    window_context.dirty = true;
                    window_context.display.damage_tracker.frame().mark_fully_damaged();
                    if window_context.display.window.has_frame {
                        window_context.display.window.request_redraw();
                    }
                }
            },
            #[cfg(unix)]
            (EventType::Terminal(TerminalEvent::ChildExit(status)), Some(window_id)) => {
                if let Some(window) = self.windows.get_mut(window_id) {
                    use std::os::unix::process::ExitStatusExt;

                    window.automation.exit_status = Some(status);
                    self.automation.emit(
                        Some(window.ipc_window_id()),
                        "child_exit",
                        serde_json::json!({
                            "code": status.code(),
                            "signal": status.signal(),
                            "core_dumped": status.core_dumped(),
                        }),
                    );
                    self.evaluate_waiters(*window_id);
                    self.schedule_automation_timer(*window_id);
                }
            },
            (EventType::Terminal(TerminalEvent::Exit), Some(window_id)) => {
                // Remove the closed terminal.
                let mut window_context = match self.windows.entry(*window_id) {
                    // Don't exit when terminal exits if user asked to hold the window.
                    Entry::Occupied(window_context)
                        if !window_context.get().display.window.hold =>
                    {
                        window_context.remove()
                    },
                    _ => return,
                };

                #[cfg(unix)]
                {
                    let ipc_window_id = window_context.ipc_window_id();
                    window_context.fail_automation_requests("pty_closed", "terminal window closed");
                    self.automation.emit(
                        Some(ipc_window_id),
                        "window_closed",
                        serde_json::json!({"window_id": ipc_window_id}),
                    );
                }

                // Unschedule pending events.
                self.scheduler.unschedule_window(window_context.id());

                // Shutdown if no more terminals are open.
                if self.windows.is_empty() && !self.cli_options.daemon {
                    // Write ref tests of last window to disk.
                    if self.config.debug.ref_test {
                        window_context.write_ref_test_results();
                    }

                    event_loop.exit();
                }
            },
            // NOTE: This event bypasses batching to minimize input latency.
            (EventType::Frame, Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(window_id) {
                    window_context.display.window.has_frame = true;
                    if window_context.dirty {
                        window_context.display.window.request_redraw();
                    }
                }
            },
            (payload, Some(window_id)) => {
                if let Some(window_context) = self.windows.get_mut(window_id) {
                    window_context.handle_event(
                        #[cfg(target_os = "macos")]
                        event_loop,
                        &self.proxy,
                        &mut self.clipboard,
                        &mut self.scheduler,
                        WinitEvent::UserEvent(Event::new(payload, *window_id)),
                    );
                }
            },
        };
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.config.debug.print_events {
            info!(target: LOG_TARGET_WINIT, "About to wait");
        }

        // Dispatch event to all windows.
        for window_context in self.windows.values_mut() {
            window_context.handle_event(
                #[cfg(target_os = "macos")]
                event_loop,
                &self.proxy,
                &mut self.clipboard,
                &mut self.scheduler,
                WinitEvent::AboutToWait,
            );
        }

        #[cfg(unix)]
        {
            let window_ids: Vec<_> = self.windows.keys().copied().collect();
            for window_id in &window_ids {
                self.apply_automation_confirmations(*window_id);
            }
            let mut changes = Vec::new();
            for (platform_id, window) in &mut self.windows {
                if let Some((screen_sequence, rows)) = window.sync_automation_screen() {
                    let grid = window
                        .automation_grid(None, None, Some(screen_sequence.saturating_sub(1)))
                        .ok();
                    changes.push((
                        *platform_id,
                        window.ipc_window_id(),
                        screen_sequence,
                        rows,
                        grid,
                    ));
                }
            }
            for (_platform_id, window_id, screen_sequence, rows, grid) in changes {
                let full = rows.is_none();
                self.automation.emit(
                    Some(window_id),
                    "screen_changed",
                    serde_json::json!({
                        "screen_sequence": screen_sequence,
                        "full": full,
                        "rows": rows,
                        "grid": grid,
                    }),
                );
            }
            for window_id in window_ids {
                self.evaluate_waiters(window_id);
                self.schedule_automation_timer(window_id);
            }
        }

        // Update the scheduler after event processing to ensure
        // the event loop deadline is as accurate as possible.
        let control_flow = match self.scheduler.update() {
            Some(instant) => ControlFlow::WaitUntil(instant),
            None => ControlFlow::Wait,
        };
        event_loop.set_control_flow(control_flow);
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if self.config.debug.print_events {
            info!("Exiting the event loop");
        }

        #[cfg(unix)]
        for window in self.windows.values_mut() {
            window.fail_automation_requests("pty_closed", "Vivido event loop is shutting down");
        }
        self.windows.clear();

        // SAFETY: The clipboard must be dropped before the event loop, so use the nop clipboard
        // as a safe placeholder.
        self.clipboard = Clipboard::new_nop();
    }
}

/// Vivido events.
#[derive(Debug, Clone)]
pub struct Event {
    /// Limit event to a specific window.
    window_id: Option<WindowId>,

    /// Event payload.
    payload: EventType,
}

impl Event {
    pub fn new<I: Into<Option<WindowId>>>(payload: EventType, window_id: I) -> Self {
        Self { window_id: window_id.into(), payload }
    }
}

impl From<Event> for WinitEvent<Event> {
    fn from(event: Event) -> Self {
        WinitEvent::UserEvent(event)
    }
}

/// Vivido events.
#[derive(Debug, Clone)]
pub enum EventType {
    Terminal(TerminalEvent),
    VividFrame,
    ConfigReload(PathBuf),
    Message(Message),
    Scroll(Scroll),
    CreateWindow(WindowOptions),
    #[cfg(unix)]
    IpcRequest(IpcRequest),
    #[cfg(unix)]
    IpcDisconnect(u64),
    #[cfg(unix)]
    ScreenshotReadback,
    #[cfg(unix)]
    ScreenshotComplete,
    #[cfg(unix)]
    AutomationTick,
    BlinkCursor,
    BlinkCursorTimeout,
    SearchNext,
    #[cfg(unix)]
    Shutdown,
    Frame,
}

impl From<TerminalEvent> for EventType {
    fn from(event: TerminalEvent) -> Self {
        Self::Terminal(event)
    }
}

/// Regex search state.
pub struct SearchState {
    /// Search direction.
    pub direction: Direction,

    /// Current position in the search history.
    pub history_index: Option<usize>,

    /// Change in display offset since the beginning of the search.
    display_offset_delta: i32,

    /// Search origin in viewport coordinates relative to original display offset.
    origin: Point,

    /// Focused match during active search.
    focused_match: Option<Match>,

    /// Search regex and history.
    ///
    /// During an active search, the first element is the user's current input.
    ///
    /// While going through history, the [`SearchState::history_index`] will point to the element
    /// in history which is currently being previewed.
    history: VecDeque<String>,

    /// Compiled search automatons.
    dfas: Option<RegexSearch>,
}

impl SearchState {
    /// Search regex text if a search is active.
    pub fn regex(&self) -> Option<&String> {
        self.history_index.and_then(|index| self.history.get(index))
    }

    /// Direction of the search from the search origin.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Focused match during normal incremental search.
    pub fn focused_match(&self) -> Option<&Match> {
        self.focused_match.as_ref()
    }

    /// Clear the focused match.
    pub fn clear_focused_match(&mut self) {
        self.focused_match = None;
    }

    /// Active search dfas.
    pub fn dfas(&mut self) -> Option<&mut RegexSearch> {
        self.dfas.as_mut()
    }

    /// Search regex text if a search is active.
    fn regex_mut(&mut self) -> Option<&mut String> {
        self.history_index.and_then(move |index| self.history.get_mut(index))
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            direction: Direction::Right,
            display_offset_delta: Default::default(),
            focused_match: Default::default(),
            history_index: Default::default(),
            history: Default::default(),
            origin: Default::default(),
            dfas: Default::default(),
        }
    }
}

pub struct ActionContext<'a, N, T> {
    pub notifier: &'a mut N,
    pub terminal: &'a mut Term<T>,
    pub clipboard: &'a mut Clipboard,
    pub mouse: &'a mut Mouse,
    pub touch: &'a mut TouchPurpose,
    pub modifiers: &'a mut Modifiers,
    pub display: &'a mut Display,
    pub message_buffer: &'a mut MessageBuffer,
    pub config: &'a UiConfig,
    pub cursor_blink_timed_out: &'a mut bool,
    pub prev_bell_cmd: &'a mut Option<Instant>,
    #[cfg(target_os = "macos")]
    pub event_loop: &'a ActiveEventLoop,
    pub event_proxy: &'a EventLoopProxy<Event>,
    pub scheduler: &'a mut Scheduler,
    pub search_state: &'a mut SearchState,
    pub dirty: &'a mut bool,
    pub occluded: &'a mut bool,
    pub preserve_title: bool,
    pub vivid_service: &'a VividService,
    #[cfg(not(windows))]
    pub master_fd: RawFd,
    #[cfg(not(windows))]
    pub shell_pid: u32,
}

impl<'a, N: Notify + 'a, T: EventListener> input::ActionContext<T> for ActionContext<'a, N, T> {
    #[inline]
    fn write_to_pty<B: Into<Cow<'static, [u8]>>>(&self, val: B) {
        self.notifier.notify(val);
    }

    /// Request a redraw.
    #[inline]
    fn mark_dirty(&mut self) {
        *self.dirty = true;
    }

    #[inline]
    fn size_info(&self) -> SizeInfo {
        self.display.size_info
    }

    fn scroll(&mut self, scroll: Scroll) {
        let old_offset = self.terminal.grid().display_offset() as i32;

        self.terminal.scroll_display(scroll);

        self.vivid_service
            .update_visibility(!*self.occluded, self.terminal.grid().display_offset());

        let lines_changed = old_offset - self.terminal.grid().display_offset() as i32;

        // Keep track of manual display offset changes during search.
        if self.search_active() {
            self.search_state.display_offset_delta += lines_changed;
        }

        // Update selection.
        if self.mouse.left_button_state == ElementState::Pressed
            || self.mouse.right_button_state == ElementState::Pressed
        {
            let display_offset = self.terminal.grid().display_offset();
            let point = self.mouse.point(&self.size_info(), display_offset);
            self.update_selection(point, self.mouse.cell_side);
        }

        *self.dirty |= lines_changed != 0;
    }

    // Copy text selection.
    fn copy_selection(&mut self, ty: ClipboardType) {
        let text = match self.terminal.selection_to_string().filter(|s| !s.is_empty()) {
            Some(text) => text,
            None => return,
        };

        if ty == ClipboardType::Selection && self.config.selection.save_to_clipboard {
            self.clipboard.store(ClipboardType::Clipboard, text.clone());
        }
        self.clipboard.store(ty, text);
    }

    fn selection_is_empty(&self) -> bool {
        self.terminal.selection.as_ref().is_none_or(Selection::is_empty)
    }

    fn clear_selection(&mut self) {
        // Clear the selection on the terminal.
        let selection = self.terminal.selection.take();
        // Mark the terminal as dirty when selection wasn't empty.
        *self.dirty |= selection.is_some_and(|s| !s.is_empty());
    }

    fn update_selection(&mut self, mut point: Point, side: Side) {
        let mut selection = match self.terminal.selection.take() {
            Some(selection) => selection,
            None => return,
        };

        // Treat motion over message bar like motion over the last line.
        point.line = min(point.line, self.terminal.bottommost_line());

        // Update selection.
        selection.update(point, side);

        self.terminal.selection = Some(selection);
        *self.dirty = true;
    }

    fn start_selection(&mut self, ty: SelectionType, point: Point, side: Side) {
        self.terminal.selection = Some(Selection::new(ty, point, side));
        *self.dirty = true;

        self.copy_selection(ClipboardType::Selection);
    }

    #[inline]
    fn mouse_mode(&self) -> bool {
        self.terminal.mode().intersects(TermMode::MOUSE_MODE)
    }

    #[inline]
    fn mouse_mut(&mut self) -> &mut Mouse {
        self.mouse
    }

    #[inline]
    fn mouse(&self) -> &Mouse {
        self.mouse
    }

    #[inline]
    fn touch_purpose(&mut self) -> &mut TouchPurpose {
        self.touch
    }

    #[inline]
    fn modifiers(&mut self) -> &mut Modifiers {
        self.modifiers
    }

    #[inline]
    fn window(&mut self) -> &mut Window {
        &mut self.display.window
    }

    #[inline]
    fn display(&mut self) -> &mut Display {
        self.display
    }

    #[inline]
    fn terminal(&self) -> &Term<T> {
        self.terminal
    }

    #[inline]
    fn terminal_mut(&mut self) -> &mut Term<T> {
        self.terminal
    }

    fn spawn_new_instance(&mut self) {
        let mut env_args = env::args();
        let program = env_args.next().unwrap();

        let mut args: Vec<String> = Vec::new();

        // Reuse the arguments passed to Vivido for the new instance.
        #[allow(clippy::while_let_on_iterator)]
        while let Some(arg) = env_args.next() {
            // New instances shouldn't inherit command.
            if arg == "-e" || arg == "--command" {
                break;
            }

            // On unix, the working directory of the foreground shell is used by `start_daemon`.
            #[cfg(not(windows))]
            if arg == "--working-directory" {
                let _ = env_args.next();
                continue;
            }

            args.push(arg);
        }

        self.spawn_daemon(&program, &args);
    }

    #[cfg(not(windows))]
    fn create_new_window(&mut self, #[cfg(target_os = "macos")] tabbing_id: Option<String>) {
        let mut options = WindowOptions::default();
        options.terminal_options.working_directory =
            foreground_process_path(self.master_fd, self.shell_pid).ok();

        #[cfg(target_os = "macos")]
        {
            options.window_tabbing_id = tabbing_id;
        }

        let _ = self.event_proxy.send_event(Event::new(EventType::CreateWindow(options), None));
    }

    #[cfg(windows)]
    fn create_new_window(&mut self) {
        let _ = self
            .event_proxy
            .send_event(Event::new(EventType::CreateWindow(WindowOptions::default()), None));
    }

    fn spawn_daemon<I, S>(&self, program: &str, args: I)
    where
        I: IntoIterator<Item = S> + Debug + Copy,
        S: AsRef<OsStr>,
    {
        #[cfg(not(windows))]
        let result = spawn_daemon(program, args, self.master_fd, self.shell_pid);
        #[cfg(windows)]
        let result = spawn_daemon(program, args);

        match result {
            Ok(_) => debug!("Launched {program} with args {args:?}"),
            Err(err) => warn!("Unable to launch {program} with args {args:?}: {err}"),
        }
    }

    fn change_font_size(&mut self, delta: f32) {
        // Round to pick integral px steps, since fonts look better on them.
        let new_size = self.display.font_size.as_px().round() + delta;
        self.display.font_size = FontSize::from_px(new_size);
        let font = self.config.font.clone().with_size(self.display.font_size);
        self.display.pending_update.set_font(font);
    }

    fn reset_font_size(&mut self) {
        let scale_factor = self.display.window.scale_factor as f32;
        self.display.font_size = self.config.font.size().scale(scale_factor);
        self.display
            .pending_update
            .set_font(self.config.font.clone().with_size(self.display.font_size));
    }

    #[inline]
    fn pop_message(&mut self) {
        if !self.message_buffer.is_empty() {
            self.display.pending_update.dirty = true;
            self.message_buffer.pop();
        }
    }

    #[inline]
    fn start_search(&mut self, direction: Direction) {
        // Only create new history entry if the previous regex wasn't empty.
        if self.search_state.history.front().is_none_or(|regex| !regex.is_empty()) {
            self.search_state.history.push_front(String::new());
            self.search_state.history.truncate(MAX_SEARCH_HISTORY_SIZE);
        }

        self.search_state.history_index = Some(0);
        self.search_state.direction = direction;
        self.search_state.focused_match = None;

        // Store original search position as origin and reset location.
        let viewport_top = Line(-(self.terminal.grid().display_offset() as i32)) - 1;
        let viewport_bottom = viewport_top + self.terminal.bottommost_line();
        let last_column = self.terminal.last_column();
        self.search_state.origin = match direction {
            Direction::Right => Point::new(viewport_top, Column(0)),
            Direction::Left => Point::new(viewport_bottom, last_column),
        };

        self.display.damage_tracker.frame().mark_fully_damaged();
        self.display.pending_update.dirty = true;
    }

    #[inline]
    fn confirm_search(&mut self) {
        if let Some(focused_match) = &self.search_state.focused_match {
            // Create a selection for the focused match.
            let start = *focused_match.start();
            let end = *focused_match.end();
            self.start_selection(SelectionType::Simple, start, Side::Left);
            self.update_selection(end, Side::Right);
            self.copy_selection(ClipboardType::Selection);
        }

        self.search_state.dfas = None;

        self.exit_search();
    }

    #[inline]
    fn cancel_search(&mut self) {
        self.search_state.dfas = None;
        self.exit_search();
    }

    #[inline]
    fn search_input(&mut self, c: char) {
        match self.search_state.history_index {
            Some(0) => (),
            // When currently in history, replace active regex with history on change.
            Some(index) => {
                self.search_state.history[0] = self.search_state.history[index].clone();
                self.search_state.history_index = Some(0);
            },
            None => return,
        }
        let regex = &mut self.search_state.history[0];

        match c {
            // Handle backspace/ctrl+h.
            '\x08' | '\x7f' => {
                let _ = regex.pop();
            },
            // Add ascii and unicode text.
            ' '..='~' | '\u{a0}'..='\u{10ffff}' => regex.push(c),
            // Ignore non-printable characters.
            _ => return,
        }

        // Clear selection so it does not obstruct any matches.
        self.terminal.selection = None;

        self.update_search();
    }

    #[inline]
    fn search_pop_word(&mut self) {
        if let Some(regex) = self.search_state.regex_mut() {
            *regex = regex.trim_end().to_owned();
            regex.truncate(regex.rfind(' ').map_or(0, |i| i + 1));
            self.update_search();
        }
    }

    /// Go to the previous regex in the search history.
    #[inline]
    fn search_history_previous(&mut self) {
        let index = match &mut self.search_state.history_index {
            None => return,
            Some(index) if *index + 1 >= self.search_state.history.len() => return,
            Some(index) => index,
        };

        *index += 1;
        self.update_search();
    }

    /// Go to the previous regex in the search history.
    #[inline]
    fn search_history_next(&mut self) {
        let index = match &mut self.search_state.history_index {
            Some(0) | None => return,
            Some(index) => index,
        };

        *index -= 1;
        self.update_search();
    }

    #[inline]
    fn advance_search_origin(&mut self, direction: Direction) {
        // Use focused match as new search origin if available.
        if let Some(focused_match) = &self.search_state.focused_match {
            let new_origin = match direction {
                Direction::Right => focused_match.end().add(self.terminal, Boundary::None, 1),
                Direction::Left => focused_match.start().sub(self.terminal, Boundary::None, 1),
            };

            self.terminal.scroll_to_point(new_origin);

            self.search_state.display_offset_delta = 0;
            self.search_state.origin = new_origin;
        }

        // Search for the next match using the supplied direction.
        let search_direction = mem::replace(&mut self.search_state.direction, direction);
        self.goto_match(None);
        self.search_state.direction = search_direction;

        // If we found a match, we set the search origin right in front of it to make sure that
        // after modifications to the regex the search is started without moving the focused match
        // around.
        let focused_match = match &self.search_state.focused_match {
            Some(focused_match) => focused_match,
            None => return,
        };

        // Set new origin to the left/right of the match, depending on search direction.
        let new_origin = match self.search_state.direction {
            Direction::Right => *focused_match.start(),
            Direction::Left => *focused_match.end(),
        };

        // Store the search origin with display offset by checking how far we need to scroll to it.
        let old_display_offset = self.terminal.grid().display_offset() as i32;
        self.terminal.scroll_to_point(new_origin);
        let new_display_offset = self.terminal.grid().display_offset() as i32;
        self.search_state.display_offset_delta = new_display_offset - old_display_offset;

        // Store origin and scroll back to the match.
        self.terminal.scroll_display(Scroll::Delta(-self.search_state.display_offset_delta));
        self.search_state.origin = new_origin;
    }

    #[inline]
    fn search_direction(&self) -> Direction {
        self.search_state.direction
    }

    #[inline]
    fn search_active(&self) -> bool {
        self.search_state.history_index.is_some()
    }

    /// Handle keyboard typing start.
    ///
    /// This will temporarily disable some features like terminal cursor blinking or the mouse
    /// cursor.
    ///
    /// All features are re-enabled again automatically.
    #[inline]
    fn on_typing_start(&mut self) {
        // Disable cursor blinking.
        let timer_id = TimerId::new(Topic::BlinkCursor, self.display.window.id());
        if self.scheduler.unschedule(timer_id).is_some() {
            self.schedule_blinking();

            // Mark the cursor as visible and queue redraw if the cursor was hidden.
            if mem::take(&mut self.display.cursor_hidden) {
                *self.dirty = true;
            }
        } else if *self.cursor_blink_timed_out {
            self.update_cursor_blinking();
        }

        // Hide mouse cursor.
        if self.config.mouse.hide_when_typing && self.display.window.mouse_visible() {
            self.display.window.set_mouse_visible(false);

            // Request hint highlights update, since the mouse may have been hovering a hint.
            self.mouse.hint_highlight_dirty = true
        }
    }

    /// Process a new character for keyboard hints.
    fn hint_input(&mut self, c: char) {
        if let Some(hint) = self.display.hint_state.keyboard_input(self.terminal, c) {
            self.mouse.block_hint_launcher = false;
            self.trigger_hint(&hint);
        }
        *self.dirty = true;
    }

    /// Trigger a hint action.
    fn trigger_hint(&mut self, hint: &HintMatch) {
        if self.mouse.block_hint_launcher {
            return;
        }

        let hint_bounds = hint.bounds();
        let text = match hint.text(self.terminal) {
            Some(text) => text,
            None => return,
        };

        match &hint.action() {
            // Launch an external program.
            HintAction::Command(command) => {
                let mut args = command.args().to_vec();
                args.push(text.into());
                self.spawn_daemon(command.program(), &args);
            },
            // Copy the text to the clipboard.
            HintAction::Action(HintInternalAction::Copy) => {
                self.clipboard.store(ClipboardType::Clipboard, text);
            },
            // Write the text to the PTY/search.
            HintAction::Action(HintInternalAction::Paste) => self.paste(&text, true),
            // Select the text.
            HintAction::Action(HintInternalAction::Select) => {
                self.start_selection(SelectionType::Simple, *hint_bounds.start(), Side::Left);
                self.update_selection(*hint_bounds.end(), Side::Right);
                self.copy_selection(ClipboardType::Selection);
            },
        }
    }

    /// Handle beginning of terminal text input.
    fn on_terminal_input_start(&mut self) {
        self.on_typing_start();
        self.clear_selection();

        if self.terminal().grid().display_offset() != 0 {
            self.scroll(Scroll::Bottom);
        }
    }

    /// Paste a text into the terminal.
    fn paste(&mut self, text: &str, bracketed: bool) {
        if self.search_active() {
            for c in text.chars() {
                self.search_input(c);
            }
        } else if bracketed && self.terminal().mode().contains(TermMode::BRACKETED_PASTE) {
            self.on_terminal_input_start();

            self.write_to_pty(&b"\x1b[200~"[..]);

            // Write filtered escape sequences.
            //
            // We remove `\x1b` to ensure it's impossible for the pasted text to write the bracketed
            // paste end escape `\x1b[201~` and `\x03` since some shells incorrectly terminate
            // bracketed paste when they receive it.
            let filtered = text.replace(['\x1b', '\x03'], "");
            self.write_to_pty(filtered.into_bytes());

            self.write_to_pty(&b"\x1b[201~"[..]);
        } else {
            self.on_terminal_input_start();

            let payload = if bracketed {
                // In non-bracketed (ie: normal) mode, terminal applications cannot distinguish
                // pasted data from keystrokes.
                //
                // In theory, we should construct the keystrokes needed to produce the data we are
                // pasting... since that's neither practical nor sensible (and probably an
                // impossible task to solve in a general way), we'll just replace line breaks
                // (windows and unix style) with a single carriage return (\r, which is what the
                // Enter key produces).
                text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
            } else {
                // When we explicitly disable bracketed paste don't manipulate with the input,
                // so we pass user input as is.
                text.to_owned().into_bytes()
            };

            self.write_to_pty(payload);
        }
    }

    fn message(&self) -> Option<&Message> {
        self.message_buffer.message()
    }

    fn config(&self) -> &UiConfig {
        self.config
    }

    #[cfg(target_os = "macos")]
    fn event_loop(&self) -> &ActiveEventLoop {
        self.event_loop
    }

    fn clipboard_mut(&mut self) -> &mut Clipboard {
        self.clipboard
    }

    fn scheduler_mut(&mut self) -> &mut Scheduler {
        self.scheduler
    }
}

impl<'a, N: Notify + 'a, T: EventListener> ActionContext<'a, N, T> {
    fn update_search(&mut self) {
        let regex = match self.search_state.regex() {
            Some(regex) => regex,
            None => return,
        };

        // Hide cursor while typing into the search bar.
        if self.config.mouse.hide_when_typing {
            self.display.window.set_mouse_visible(false);
        }

        if regex.is_empty() {
            // Stop search if there's nothing to search for.
            self.search_reset_state();
            self.search_state.dfas = None;
        } else {
            // Create search dfas for the new regex string.
            self.search_state.dfas = RegexSearch::new(regex).ok();

            // Update search highlighting.
            self.goto_match(MAX_SEARCH_WHILE_TYPING);
        }

        *self.dirty = true;
    }

    /// Reset terminal to the state before search was started.
    fn search_reset_state(&mut self) {
        // Unschedule pending timers.
        let timer_id = TimerId::new(Topic::DelayedSearch, self.display.window.id());
        self.scheduler.unschedule(timer_id);

        // Clear focused match.
        self.search_state.focused_match = None;

        self.search_state.display_offset_delta = 0;
    }

    /// Jump to the first regex match from the search origin.
    fn goto_match(&mut self, mut limit: Option<usize>) {
        let dfas = match &mut self.search_state.dfas {
            Some(dfas) => dfas,
            None => return,
        };

        // Limit search only when enough lines are available to run into the limit.
        limit = limit.filter(|&limit| limit <= self.terminal.total_lines());

        // Jump to the next match.
        let direction = self.search_state.direction;
        let clamped_origin = self.search_state.origin.grid_clamp(self.terminal, Boundary::Grid);
        match self.terminal.search_next(dfas, clamped_origin, direction, Side::Left, limit) {
            Some(regex_match) => {
                let old_offset = self.terminal.grid().display_offset() as i32;

                self.terminal.scroll_to_point(*regex_match.start());

                // Update the focused match.
                self.search_state.focused_match = Some(regex_match);

                // Store number of lines the viewport had to be moved.
                let display_offset = self.terminal.grid().display_offset();
                self.search_state.display_offset_delta += old_offset - display_offset as i32;

                // Since we found a result, we require no delayed re-search.
                let timer_id = TimerId::new(Topic::DelayedSearch, self.display.window.id());
                self.scheduler.unschedule(timer_id);
            },
            // Reset viewport only when we know there is no match, to prevent unnecessary jumping.
            None if limit.is_none() => self.search_reset_state(),
            None => {
                // Schedule delayed search if we ran into our search limit.
                let timer_id = TimerId::new(Topic::DelayedSearch, self.display.window.id());
                if !self.scheduler.scheduled(timer_id) {
                    let event = Event::new(EventType::SearchNext, self.display.window.id());
                    self.scheduler.schedule(event, TYPING_SEARCH_DELAY, false, timer_id);
                }

                // Clear focused match.
                self.search_state.focused_match = None;
            },
        }

        *self.dirty = true;
    }

    /// Cleanup the search state.
    fn exit_search(&mut self) {
        self.display.damage_tracker.frame().mark_fully_damaged();
        self.display.pending_update.dirty = true;
        self.search_state.history_index = None;

        // Clear focused match.
        self.search_state.focused_match = None;
    }

    /// Update the cursor blinking state.
    fn update_cursor_blinking(&mut self) {
        // Get config cursor style.
        let cursor_style = self.config.cursor.style;

        // Check terminal cursor style.
        let terminal_blinking = self.terminal.cursor_style().blinking;
        let mut blinking = cursor_style.blinking_override().unwrap_or(terminal_blinking);
        blinking &= self.terminal().mode().contains(TermMode::SHOW_CURSOR)
            && self.display().ime.preedit().is_none();

        // Update cursor blinking state.
        let window_id = self.display.window.id();
        self.scheduler.unschedule(TimerId::new(Topic::BlinkCursor, window_id));
        self.scheduler.unschedule(TimerId::new(Topic::BlinkTimeout, window_id));

        // Reset blinking timeout.
        *self.cursor_blink_timed_out = false;

        if blinking && self.terminal.is_focused {
            self.schedule_blinking();
            self.schedule_blinking_timeout();
        } else {
            self.display.cursor_hidden = false;
            *self.dirty = true;
        }
    }

    fn schedule_blinking(&mut self) {
        let window_id = self.display.window.id();
        let timer_id = TimerId::new(Topic::BlinkCursor, window_id);
        let event = Event::new(EventType::BlinkCursor, window_id);
        let blinking_interval = Duration::from_millis(self.config.cursor.blink_interval());
        self.scheduler.schedule(event, blinking_interval, true, timer_id);
    }

    fn schedule_blinking_timeout(&mut self) {
        let blinking_timeout = self.config.cursor.blink_timeout();
        if blinking_timeout == Duration::ZERO {
            return;
        }

        let window_id = self.display.window.id();
        let event = Event::new(EventType::BlinkCursorTimeout, window_id);
        let timer_id = TimerId::new(Topic::BlinkTimeout, window_id);

        self.scheduler.schedule(event, blinking_timeout, false, timer_id);
    }
}

/// Identified purpose of the touch input.
#[derive(Default, Debug)]
pub enum TouchPurpose {
    #[default]
    None,
    Select(TouchEvent),
    Scroll(TouchEvent),
    Zoom(TouchZoom),
    ZoomPendingSlot(TouchEvent),
    Tap(TouchEvent),
    Invalid(HashSet<u64, RandomState>),
}

/// Touch zooming state.
#[derive(Debug)]
pub struct TouchZoom {
    slots: (TouchEvent, TouchEvent),
    fractions: f32,
}

impl TouchZoom {
    pub fn new(slots: (TouchEvent, TouchEvent)) -> Self {
        Self { slots, fractions: Default::default() }
    }

    /// Get slot distance change since last update.
    pub fn font_delta(&mut self, slot: TouchEvent) -> f32 {
        let old_distance = self.distance();

        // Update touch slots.
        if slot.id == self.slots.0.id {
            self.slots.0 = slot;
        } else {
            self.slots.1 = slot;
        }

        // Calculate font change in `FONT_SIZE_STEP` increments.
        let delta = (self.distance() - old_distance) * TOUCH_ZOOM_FACTOR + self.fractions;
        let font_delta = (delta.abs() / FONT_SIZE_STEP).floor() * FONT_SIZE_STEP * delta.signum();
        self.fractions = delta - font_delta;

        font_delta
    }

    /// Get active touch slots.
    pub fn slots(&self) -> (TouchEvent, TouchEvent) {
        self.slots
    }

    /// Calculate distance between slots.
    fn distance(&self) -> f32 {
        let delta_x = self.slots.0.location.x - self.slots.1.location.x;
        let delta_y = self.slots.0.location.y - self.slots.1.location.y;
        delta_x.hypot(delta_y) as f32
    }
}

/// State of the mouse.
#[derive(Debug)]
pub struct Mouse {
    pub left_button_state: ElementState,
    pub middle_button_state: ElementState,
    pub right_button_state: ElementState,
    pub last_click_timestamp: Instant,
    pub last_click_button: MouseButton,
    pub last_click_point: Option<Point>,
    pub click_state: ClickState,
    pub accumulated_scroll: AccumulatedScroll,
    pub cell_side: Side,
    pub block_hint_launcher: bool,
    pub hint_highlight_dirty: bool,
    pub inside_text_area: bool,
    pub x: usize,
    pub y: usize,
}

impl Default for Mouse {
    fn default() -> Mouse {
        Mouse {
            last_click_timestamp: Instant::now(),
            last_click_button: MouseButton::Left,
            last_click_point: None,
            left_button_state: ElementState::Released,
            middle_button_state: ElementState::Released,
            right_button_state: ElementState::Released,
            click_state: ClickState::None,
            cell_side: Side::Left,
            hint_highlight_dirty: Default::default(),
            block_hint_launcher: Default::default(),
            inside_text_area: Default::default(),
            accumulated_scroll: Default::default(),
            x: Default::default(),
            y: Default::default(),
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ClickState {
    None,
    Click,
    DoubleClick,
}

impl Mouse {
    /// Convert mouse pixel coordinates to viewport point.
    ///
    /// If the coordinates are outside of the terminal grid, like positions inside the padding, the
    /// coordinates will be clamped to the closest grid coordinates.
    #[inline]
    pub fn point(&self, size: &SizeInfo, display_offset: usize) -> Point {
        let col = self.x.saturating_sub(size.padding_x() as usize) / (size.cell_width() as usize);
        let col = min(Column(col), size.last_column());

        let line = self.y.saturating_sub(size.padding_y() as usize) / (size.cell_height() as usize);
        let line = min(line, size.bottommost_line().0 as usize);

        term::viewport_to_point(display_offset, Point::new(line, col))
    }
}

/// The amount of scroll accumulated from the pointer events.
#[derive(Default, Debug)]
pub struct AccumulatedScroll {
    /// Scroll we should perform along `x` axis.
    pub x: f64,

    /// Scroll we should perform along `y` axis.
    pub y: f64,
}

impl input::Processor<EventProxy, ActionContext<'_, Notifier, EventProxy>> {
    /// Handle events from winit.
    pub fn handle_event(&mut self, event: WinitEvent<Event>) {
        match event {
            WinitEvent::UserEvent(Event { payload, .. }) => match payload {
                EventType::SearchNext => self.ctx.goto_match(None),
                EventType::Scroll(scroll) => self.ctx.scroll(scroll),
                EventType::BlinkCursor => {
                    // Only change state when timeout isn't reached, since we could get
                    // BlinkCursor and BlinkCursorTimeout events at the same time.
                    if !*self.ctx.cursor_blink_timed_out {
                        self.ctx.display.cursor_hidden ^= true;
                        *self.ctx.dirty = true;
                    }
                },
                EventType::BlinkCursorTimeout => {
                    // Disable blinking after timeout reached.
                    let timer_id = TimerId::new(Topic::BlinkCursor, self.ctx.display.window.id());
                    self.ctx.scheduler.unschedule(timer_id);
                    *self.ctx.cursor_blink_timed_out = true;
                    self.ctx.display.cursor_hidden = false;
                    *self.ctx.dirty = true;
                },
                // Add message only if it's not already queued.
                EventType::Message(message) if !self.ctx.message_buffer.is_queued(&message) => {
                    self.ctx.message_buffer.push(message);
                    self.ctx.display.pending_update.dirty = true;
                },
                EventType::Terminal(event) => match event {
                    TerminalEvent::Title(title) => {
                        if !self.ctx.preserve_title && self.ctx.config.window.dynamic_title {
                            self.ctx.window().set_title(title);
                        }
                    },
                    TerminalEvent::ResetTitle => {
                        let window_config = &self.ctx.config.window;
                        if !self.ctx.preserve_title && window_config.dynamic_title {
                            self.ctx.display.window.set_title(window_config.identity.title.clone());
                        }
                    },
                    TerminalEvent::Bell => {
                        // Set window urgency hint when window is not focused.
                        let focused = self.ctx.terminal.is_focused;
                        if !focused && self.ctx.terminal.mode().contains(TermMode::URGENCY_HINTS) {
                            self.ctx.window().set_urgent(true);
                        }

                        // Ring visual bell.
                        self.ctx.display.visual_bell.ring();

                        // Execute bell command.
                        if let Some(bell_command) = &self.ctx.config.bell.command
                            && self
                                .ctx
                                .prev_bell_cmd
                                .is_none_or(|i| i.elapsed() >= BELL_CMD_COOLDOWN)
                        {
                            self.ctx.spawn_daemon(bell_command.program(), bell_command.args());

                            *self.ctx.prev_bell_cmd = Some(Instant::now());
                        }
                    },
                    TerminalEvent::Graphics(command) => {
                        self.ctx.display.submit_graphics(command);
                        self.ctx.mark_dirty();
                    },
                    TerminalEvent::VividMarker { marker, line, column, alternate } => {
                        self.ctx
                            .vivid_service
                            .handle_terminal_marker(&marker, line, column, alternate);
                        self.ctx.mark_dirty();
                    },
                    TerminalEvent::VividGridScroll { origin, end, lines, history_size } => {
                        self.ctx.vivid_service.handle_grid_scroll(origin, end, lines, history_size);
                        self.ctx.mark_dirty();
                    },
                    TerminalEvent::VividClear => {
                        self.ctx.vivid_service.handle_terminal_clear();
                        self.ctx.mark_dirty();
                    },
                    TerminalEvent::VividScreenSwap { alternate } => {
                        self.ctx.vivid_service.handle_screen_swap(alternate);
                        self.ctx.vivid_service.update_visibility(
                            !*self.ctx.occluded,
                            self.ctx.terminal.grid().display_offset(),
                        );
                        self.ctx.mark_dirty();
                    },
                    TerminalEvent::ClipboardStore(clipboard_type, content) => {
                        if self.ctx.terminal.is_focused {
                            self.ctx.clipboard.store(clipboard_type, content);
                        }
                    },
                    TerminalEvent::ClipboardLoad(clipboard_type, format) => {
                        if self.ctx.terminal.is_focused {
                            let text = format(self.ctx.clipboard.load(clipboard_type).as_str());
                            self.ctx.write_to_pty(text.into_bytes());
                        }
                    },
                    TerminalEvent::ColorRequest(index, format) => {
                        let color = match self.ctx.terminal().colors()[index] {
                            Some(color) => Rgb(color),
                            // Ignore cursor color requests unless it was changed.
                            None if index == NamedColor::Cursor as usize => return,
                            None => self.ctx.display.colors[index],
                        };
                        self.ctx.write_to_pty(format(color.0).into_bytes());
                    },
                    TerminalEvent::TextAreaSizeRequest(format) => {
                        let text = format(self.ctx.size_info().into());
                        self.ctx.write_to_pty(text.into_bytes());
                    },
                    TerminalEvent::PtyWrite(text) => self.ctx.write_to_pty(text.into_bytes()),
                    TerminalEvent::MouseCursorDirty => self.reset_mouse_cursor(),
                    TerminalEvent::CursorBlinkingChange => self.ctx.update_cursor_blinking(),
                    TerminalEvent::Exit | TerminalEvent::ChildExit(_) | TerminalEvent::Wakeup => (),
                    #[cfg(unix)]
                    TerminalEvent::PtyOutput { .. }
                    | TerminalEvent::PtyWriteComplete(_)
                    | TerminalEvent::PtyResizeComplete(_) => (),
                },
                #[cfg(unix)]
                EventType::IpcRequest(_)
                | EventType::IpcDisconnect(_)
                | EventType::ScreenshotReadback
                | EventType::ScreenshotComplete
                | EventType::AutomationTick
                | EventType::Shutdown => (),
                EventType::Message(_)
                | EventType::ConfigReload(_)
                | EventType::CreateWindow(_)
                | EventType::Frame => (),
                EventType::VividFrame => (),
            },
            WinitEvent::WindowEvent { event, .. } => {
                match event {
                    WindowEvent::CloseRequested => {
                        // User asked to close the window, so no need to hold it.
                        self.ctx.window().hold = false;
                        self.ctx.terminal.exit();
                    },
                    WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        let old_scale_factor =
                            mem::replace(&mut self.ctx.window().scale_factor, scale_factor);

                        let display_update_pending = &mut self.ctx.display.pending_update;

                        // Rescale font size for the new factor.
                        let font_scale = scale_factor as f32 / old_scale_factor as f32;
                        self.ctx.display.font_size = self.ctx.display.font_size.scale(font_scale);

                        let font = self.ctx.config.font.clone();
                        display_update_pending.set_font(font.with_size(self.ctx.display.font_size));
                    },
                    WindowEvent::Resized(size) => {
                        // Ignore resize events to zero in any dimension, to avoid issues with Winit
                        // and the ConPTY. A 0x0 resize will also occur when the window is minimized
                        // on Windows.
                        if size.width == 0 || size.height == 0 {
                            return;
                        }

                        self.ctx.display.pending_update.set_dimensions(size);
                    },
                    WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                        self.key_input(event);
                    },
                    WindowEvent::ModifiersChanged(modifiers) => self.modifiers_input(modifiers),
                    WindowEvent::MouseInput { state, button, .. } => {
                        self.ctx.window().set_mouse_visible(true);
                        self.mouse_input(state, button);
                    },
                    WindowEvent::CursorMoved { position, .. } => {
                        self.ctx.window().set_mouse_visible(true);
                        self.mouse_moved(position);
                    },
                    WindowEvent::MouseWheel { delta, phase, .. } => {
                        self.ctx.window().set_mouse_visible(true);
                        self.mouse_wheel_input(delta, phase);
                    },
                    WindowEvent::Touch(touch) => self.touch(touch),
                    WindowEvent::Focused(is_focused) => {
                        self.ctx.terminal.is_focused = is_focused;

                        // When the unfocused hollow is used we must redraw on focus change.
                        if self.ctx.config.cursor.unfocused_hollow {
                            *self.ctx.dirty = true;
                        }

                        // Reset the urgency hint when gaining focus.
                        if is_focused {
                            self.ctx.window().set_urgent(false);
                        }

                        self.ctx.update_cursor_blinking();
                        self.on_focus_change(is_focused);

                        // Ensure IME is disabled while unfocused.
                        self.ctx.window().set_ime_inhibitor(ImeInhibitor::FOCUS, !is_focused);
                    },
                    WindowEvent::Occluded(occluded) => {
                        *self.ctx.occluded = occluded;
                        self.ctx.vivid_service.update_visibility(
                            !occluded,
                            self.ctx.terminal.grid().display_offset(),
                        );
                    },
                    WindowEvent::DroppedFile(path) => {
                        let path: String = path.to_string_lossy().into();
                        self.ctx.paste(&(path + " "), true);
                    },
                    WindowEvent::CursorLeft { .. } => {
                        self.ctx.mouse.inside_text_area = false;

                        if self.ctx.display().highlighted_hint.is_some() {
                            *self.ctx.dirty = true;
                        }
                    },
                    WindowEvent::Ime(ime) => match ime {
                        Ime::Commit(text) => {
                            *self.ctx.dirty = true;
                            // Don't use bracketed paste for single char input.
                            self.ctx.paste(&text, text.chars().count() > 1);
                            self.ctx.update_cursor_blinking();
                        },
                        Ime::Preedit(text, cursor_offset) => {
                            let preedit =
                                (!text.is_empty()).then(|| Preedit::new(text, cursor_offset));

                            if self.ctx.display.ime.preedit() != preedit.as_ref() {
                                self.ctx.display.ime.set_preedit(preedit);
                                self.ctx.update_cursor_blinking();
                                *self.ctx.dirty = true;
                            }
                        },
                        Ime::Enabled => {
                            self.ctx.display.ime.set_enabled(true);
                            *self.ctx.dirty = true;
                        },
                        Ime::Disabled => {
                            self.ctx.display.ime.set_enabled(false);
                            *self.ctx.dirty = true;
                        },
                    },
                    WindowEvent::KeyboardInput { is_synthetic: true, .. }
                    | WindowEvent::ActivationTokenDone { .. }
                    | WindowEvent::DoubleTapGesture { .. }
                    | WindowEvent::TouchpadPressure { .. }
                    | WindowEvent::RotationGesture { .. }
                    | WindowEvent::CursorEntered { .. }
                    | WindowEvent::PinchGesture { .. }
                    | WindowEvent::AxisMotion { .. }
                    | WindowEvent::PanGesture { .. }
                    | WindowEvent::HoveredFileCancelled
                    | WindowEvent::Destroyed
                    | WindowEvent::ThemeChanged(_)
                    | WindowEvent::HoveredFile(_)
                    | WindowEvent::RedrawRequested
                    | WindowEvent::Moved(_) => (),
                }
            },
            WinitEvent::Suspended
            | WinitEvent::NewEvents { .. }
            | WinitEvent::DeviceEvent { .. }
            | WinitEvent::LoopExiting
            | WinitEvent::Resumed
            | WinitEvent::MemoryWarning
            | WinitEvent::AboutToWait => (),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EventProxy {
    proxy: EventLoopProxy<Event>,
    window_id: WindowId,
}

impl EventProxy {
    pub fn new(proxy: EventLoopProxy<Event>, window_id: WindowId) -> Self {
        Self { proxy, window_id }
    }

    /// Send an event to the event loop.
    pub fn send_event(&self, event: EventType) {
        let _ = self.proxy.send_event(Event::new(event, self.window_id));
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TerminalEvent) {
        let _ = self.proxy.send_event(Event::new(event.into(), self.window_id));
    }
}

#[cfg(all(test, unix))]
mod ipc_wait_tests {
    use super::pattern_find;

    #[test]
    fn literal_and_regex_wait_matching_report_byte_ranges() {
        assert_eq!(pattern_find(b"before ready> after", b"ready>", false), Some((7, 13)));
        assert_eq!(pattern_find(b"status=1234", br"status=\d+", true), Some((0, 11)));
        assert_eq!(pattern_find("界面 ready".as_bytes(), "界面".as_bytes(), false), Some((0, 6)));
    }
}
