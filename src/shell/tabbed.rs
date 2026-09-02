//! Standalone Windows/Linux application hosting one terminal pane per tab.

use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
#[cfg(target_os = "linux")]
use winit::event::TouchPhase;
use winit::event::{
    ElementState, Event as WinitEvent, KeyEvent, MouseButton, StartCause, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{CursorIcon, Fullscreen, ResizeDirection, Window, WindowId};

#[cfg(windows)]
use winit::platform::windows::WindowAttributesExtWindows;

#[cfg(target_os = "linux")]
use crate::accessibility::terminal_document_enabled;
use crate::cli::WindowOptions;
use crate::config::UiConfig;
use crate::config::window::Decorations;
use crate::display::renderer::EmbeddedFramePlacement;
use crate::event::{Event, Processor};
use crate::host::{IpcError, IpcRequest};

use super::accessibility::{AccessibilityCommand, ChromeState, ShellAccessibility};
use super::launch::{LaunchAction, LaunchEntry};
use super::menu::NewTabMenu;
#[cfg(windows)]
use super::menu_window::MenuWindow;
use super::{
    ChromeHitMap, ChromeLayout, ChromeRenderer, NativePaneHost, PaneHost, ShellAction, Tabs,
    compute_layout, launch,
};

const INITIAL_WIDTH: f64 = 1000.0;
const INITIAL_HEIGHT: f64 = 700.0;
const MINIMUM_WIDTH: f64 = 320.0;
const MINIMUM_HEIGHT: f64 = 160.0;
/// Width of the frame band along each window edge which starts a resize.
const RESIZE_EDGE_LOGICAL: f64 = if cfg!(windows) { 10.0 } else { 8.0 };
/// How far a diagonal corner grip reaches along both of its edges.
///
/// The edge band alone would leave a grip only as large as its own width, which is too small a
/// target to find with a pointer; every desktop frame widens its corners the same way.
const RESIZE_CORNER_LOGICAL: f64 = 16.0;
const CLAIMED_METHODS: [&str; 1] = ["create_window"];

/// Windows/Linux headed Vivido with one integrated top-level tab host.
pub struct TabbedApplication {
    processor: Processor,
    config: UiConfig,
    initial_options: Option<WindowOptions>,
    chrome: Option<Arc<Window>>,
    chrome_id: Option<WindowId>,
    renderer: Option<ChromeRenderer>,
    accessibility: Option<ShellAccessibility>,
    layout: ChromeLayout,
    hits: ChromeHitMap,
    tabs: Tabs,
    tab_options: HashMap<WindowId, WindowOptions>,
    menu: Option<NewTabMenu>,
    /// Launch entries, probed once: enumerating them costs a process spawn on Windows.
    launch_entries: Option<Vec<LaunchEntry>>,
    #[cfg(windows)]
    menu_window: Option<MenuWindow>,
    cursor: Option<PhysicalPosition<f64>>,
    last_title_click: Option<(Instant, PhysicalPosition<f64>)>,
    draw_controls: bool,
    closing: bool,
    #[cfg(target_os = "linux")]
    pointer_capture: bool,
    #[cfg(target_os = "linux")]
    touch_capture: std::collections::HashSet<u64>,
}

impl TabbedApplication {
    pub fn new(mut processor: Processor, config: UiConfig) -> Self {
        let initial_options = processor.take_initial_window_options();
        processor.claim_ipc_methods(&CLAIMED_METHODS);
        let draw_controls = config.window.decorations != Decorations::None;
        Self {
            processor,
            config,
            initial_options,
            chrome: None,
            chrome_id: None,
            renderer: None,
            accessibility: None,
            layout: ChromeLayout::default(),
            hits: ChromeHitMap::default(),
            tabs: Tabs::default(),
            tab_options: HashMap::new(),
            menu: None,
            launch_entries: None,
            #[cfg(windows)]
            menu_window: None,
            cursor: None,
            last_title_click: None,
            draw_controls,
            closing: false,
            #[cfg(target_os = "linux")]
            pointer_capture: false,
            #[cfg(target_os = "linux")]
            touch_capture: std::collections::HashSet::new(),
        }
    }

    pub fn run(mut self, event_loop: EventLoop<Event>) -> (Processor, Result<(), Box<dyn Error>>) {
        let result = event_loop.run_app(&mut self).map_err(|error| error.into());
        (self.processor, result)
    }

    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<(), Box<dyn Error>> {
        if self.chrome.is_some() {
            return Ok(());
        }
        let mut attributes = Window::default_attributes()
            .with_title(self.config.window.identity.title.clone())
            .with_inner_size(LogicalSize::new(INITIAL_WIDTH, INITIAL_HEIGHT))
            .with_min_inner_size(LogicalSize::new(MINIMUM_WIDTH, MINIMUM_HEIGHT))
            .with_visible(false)
            .with_transparent(true)
            .with_blur(self.config.window.blur)
            .with_theme(self.config.window.theme())
            .with_maximized(self.config.window.maximized())
            .with_fullscreen(self.config.window.fullscreen())
            .with_window_level(self.config.window.level.into())
            .with_decorations(false);
        if let Some(position) = self.config.window.position {
            attributes = attributes.with_position(LogicalPosition::new(position.x, position.y));
        }
        #[cfg(windows)]
        {
            // Chrome is presented through DirectComposition. An HWND redirection bitmap would
            // retain an opaque copy of the initial client area underneath that visual, making
            // transparency appear only in regions exposed by a later resize.
            attributes = attributes
                .with_no_redirection_bitmap(true)
                .with_clip_children(true)
                .with_undecorated_shadow(self.draw_controls);
        }
        let chrome = Arc::new(event_loop.create_window(attributes)?);
        let accessibility =
            ShellAccessibility::new(event_loop, &chrome, &self.config.window.identity.title);
        let renderer = ChromeRenderer::new(Arc::clone(&chrome), &self.config)?;
        self.chrome_id = Some(chrome.id());
        self.layout = compute_layout(chrome.inner_size(), chrome.scale_factor());
        self.chrome = Some(chrome);
        self.renderer = Some(renderer);
        self.accessibility = Some(accessibility);

        let options = self.initial_options.take().unwrap_or_default();
        self.create_tab(event_loop, options)?;
        self.sync_visibility_geometry_and_focus(true);
        if let Some(chrome) = &self.chrome {
            chrome.set_visible(true);
            chrome.request_redraw();
        }
        Ok(())
    }

    fn host(&self) -> Option<NativePaneHost> {
        self.chrome.as_ref().map(|chrome| NativePaneHost::new(Arc::clone(chrome)))
    }

    fn create_tab(
        &mut self,
        event_loop: &ActiveEventLoop,
        mut options: WindowOptions,
    ) -> Result<WindowId, Box<dyn Error>> {
        if options.terminal_options.working_directory.is_none() {
            options.terminal_options.working_directory = self
                .tabs
                .active_window()
                .and_then(|window_id| self.processor.window(window_id))
                .and_then(|window| window.current_directory())
                .or_else(|| std::env::current_dir().ok());
        }
        let inherited_options = options.clone();
        let host = self.host().ok_or("tab host is not initialized")?;
        let window_id = host.create_pane_with_options(&mut self.processor, event_loop, options)?;
        if self.tabs.is_empty() {
            let content_size = self
                .processor
                .window(window_id)
                .map(|window| window.terminal_content_size())
                .unwrap_or_default();
            if content_size.width > 0 && content_size.height > 0 {
                let tab_height = (super::TAB_BAR_LOGICAL
                    * self.chrome.as_ref().map_or(1.0, |window| window.scale_factor()))
                .round() as u32;
                let bottom_gutter = if cfg!(windows) {
                    (10.0 * self.chrome.as_ref().map_or(1.0, |window| window.scale_factor()))
                        .round() as u32
                } else {
                    0
                };
                let size = winit::dpi::PhysicalSize::new(
                    content_size.width,
                    content_size.height.saturating_add(tab_height).saturating_add(bottom_gutter),
                );
                if let Some(chrome) = &self.chrome {
                    let _ = chrome.request_inner_size(size);
                }
                self.layout = compute_layout(
                    size,
                    self.chrome.as_ref().map_or(1.0, |window| window.scale_factor()),
                );
            }
        }
        self.tab_options.insert(window_id, inherited_options);
        let title = self
            .processor
            .window(window_id)
            .map(|window| window.title().to_owned())
            .unwrap_or_else(|| self.config.window.identity.title.clone());
        self.tabs.add(window_id, title);
        self.sync_visibility_geometry_and_focus(true);
        self.request_redraw();
        Ok(window_id)
    }

    fn inherited_tab_options(&self, source: Option<WindowId>) -> WindowOptions {
        let source = source.or_else(|| self.tabs.active_window());
        let mut options =
            source.and_then(|id| self.tab_options.get(&id)).cloned().unwrap_or_default();
        options.ipc_window_id = None;
        options.no_activate = false;
        #[cfg(windows)]
        {
            options.parent_window = None;
        }
        options.terminal_options.working_directory = None;
        options
    }

    /// Open the `+` button's launch menu, or close it when it is already open.
    fn toggle_new_tab_menu(&mut self, event_loop: &ActiveEventLoop) {
        if self.menu.is_some() {
            self.close_menu();
            return;
        }
        let anchor = self.hits.new_tab;
        let Some(bounds) = self.chrome.as_ref().map(|chrome| chrome.inner_size()) else { return };
        if anchor.width == 0 {
            return;
        }
        let entries = self.launch_entries.get_or_insert_with(launch::entries).clone();
        let Some(renderer) = &mut self.renderer else { return };
        let Some(menu) = renderer.layout_menu(entries, anchor, bounds) else { return };
        #[cfg(windows)]
        self.show_menu_window(event_loop, &menu);
        #[cfg(target_os = "linux")]
        let _ = event_loop;
        self.menu = Some(menu);
        self.request_redraw();
    }

    /// Close the menu and hand the keyboard back to the terminal.
    fn close_menu(&mut self) {
        self.dismiss_menu(true);
    }

    /// Close the menu, restoring pane focus only when this side gave the focus away.
    ///
    /// Dismissal triggered by the popup losing focus must not take it back: on Windows the user
    /// may have clicked another application entirely, and refocusing the pane would drag Vivido
    /// back in front of it.
    fn dismiss_menu(&mut self, restore_focus: bool) {
        if self.menu.take().is_none() {
            return;
        }
        #[cfg(windows)]
        {
            if let Some(window) = &self.menu_window {
                window.hide();
            }
            if restore_focus
                && let (Some(active), Some(host)) = (self.tabs.active_window(), self.host())
            {
                host.focus(&mut self.processor, active);
            }
        }
        #[cfg(target_os = "linux")]
        let _ = restore_focus;
        self.request_redraw();
    }

    /// Redraw whichever surface presents the open menu.
    fn refresh_menu(&self) {
        #[cfg(windows)]
        if let Some(window) = &self.menu_window {
            window.request_redraw();
        }
        #[cfg(target_os = "linux")]
        self.request_redraw();
    }

    fn activate_menu_entry(&mut self, event_loop: &ActiveEventLoop, index: usize) {
        let action = self.menu.as_ref().and_then(|menu| menu.action(index)).cloned();
        self.close_menu();
        if let Some(action) = action {
            self.launch(event_loop, &action);
        }
    }

    fn launch(&mut self, event_loop: &ActiveEventLoop, action: &LaunchAction) {
        match action {
            LaunchAction::NewTab(program) => {
                let mut options = self.inherited_tab_options(None);
                // Without a program the entry is the plain `+` click, which keeps whatever the
                // active tab was started with.
                if let Some(program) = program {
                    options.terminal_options.set_command(program);
                }
                if let Err(error) = self.create_tab(event_loop, options) {
                    log::error!("could not create tab: {error}");
                }
            },
            LaunchAction::NewWindow => self.spawn_new_window(),
        }
    }

    /// Start another top-level Vivido, the way the `SpawnNewInstance` action does.
    ///
    /// One chrome window owns one tab strip, so a second window is a second process rather than
    /// another surface inside this one.
    fn spawn_new_window(&mut self) {
        let program = match std::env::current_exe() {
            Ok(program) => program,
            Err(error) => {
                log::error!("could not locate the Vivido executable: {error}");
                return;
            },
        };
        let arguments = crate::daemon::relaunch_arguments(std::env::args().skip(1));
        #[cfg(not(windows))]
        let working_directory = self
            .tabs
            .active_window()
            .and_then(|id| self.processor.window(id))
            .and_then(|window| window.current_directory());

        let program = program.to_string_lossy().into_owned();
        #[cfg(not(windows))]
        let result = crate::daemon::spawn_daemon(&program, &arguments, working_directory);
        #[cfg(windows)]
        let result = crate::daemon::spawn_daemon(&program, &arguments);
        if let Err(error) = result {
            log::error!("could not open a new Vivido window: {error}");
        }
    }

    /// Apply one key press to the open menu.
    fn menu_key(&mut self, event_loop: &ActiveEventLoop, key: &KeyEvent) {
        if key.state != ElementState::Pressed {
            return;
        }
        match key.logical_key {
            Key::Named(NamedKey::Escape) => self.close_menu(),
            Key::Named(NamedKey::ArrowDown) => self.move_menu_selection(1),
            Key::Named(NamedKey::ArrowUp) => self.move_menu_selection(-1),
            Key::Named(NamedKey::Enter) => {
                if let Some(index) = self.menu.as_ref().and_then(NewTabMenu::selected_index) {
                    self.activate_menu_entry(event_loop, index);
                }
            },
            _ => (),
        }
    }

    fn move_menu_selection(&mut self, delta: isize) {
        if self.menu.as_mut().is_some_and(|menu| menu.move_selection(delta)) {
            self.refresh_menu();
        }
    }

    #[cfg(windows)]
    fn show_menu_window(&mut self, event_loop: &ActiveEventLoop, menu: &NewTabMenu) {
        if self.menu_window.is_none() {
            let Some(chrome) = self.chrome.clone() else { return };
            match MenuWindow::new(event_loop, &chrome, &self.config) {
                Ok(window) => self.menu_window = Some(window),
                Err(error) => {
                    log::error!("could not create the Vivido tab menu window: {error}");
                    return;
                },
            }
        }
        let scale = self.chrome.as_ref().map_or(1.0, |chrome| chrome.scale_factor());
        if let Some(window) = &mut self.menu_window {
            window.rescale(scale, &self.config);
            window.show(menu);
        }
    }

    #[cfg(windows)]
    fn handle_menu_window_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => {
                if let (Some(window), Some(menu)) = (&mut self.menu_window, &self.menu) {
                    window.render(menu);
                }
            },
            WindowEvent::CursorMoved { position, .. } => {
                let Some(point) = self
                    .menu_window
                    .as_ref()
                    .map(|window| window.to_chrome(position.x, position.y))
                else {
                    return;
                };
                // The popup reports its own coordinates; the menu and the chrome share one space.
                self.cursor = Some(PhysicalPosition::new(point.0, point.1));
                if self.menu.as_mut().is_some_and(|menu| menu.hover(point.0, point.1)) {
                    self.refresh_menu();
                }
            },
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let hit = self.cursor.and_then(|position| {
                    self.menu.as_ref().and_then(|menu| menu.hit(position.x, position.y))
                });
                match hit {
                    Some(index) => self.activate_menu_entry(event_loop, index),
                    None => self.close_menu(),
                }
            },
            WindowEvent::Focused(false) | WindowEvent::CloseRequested => self.dismiss_menu(false),
            _ => (),
        }
    }

    fn switch_to(&mut self, index: usize) {
        if self.tabs.select(index) {
            self.sync_visibility_geometry_and_focus(true);
            self.request_redraw();
        }
    }

    fn sync_visibility_geometry_and_focus(&mut self, focus: bool) {
        let Some(host) = self.host() else { return };
        let active = self.tabs.active_window();
        let panes = self.tabs.as_slice().iter().map(|tab| tab.window_id).collect::<Vec<_>>();
        for pane in panes {
            let visible = Some(pane) == active;
            host.move_pane(&mut self.processor, pane, self.layout.content);
            host.reveal(&mut self.processor, pane, visible);
            #[cfg(target_os = "linux")]
            self.processor.set_embedded_window_focused(
                pane,
                visible && self.chrome.as_ref().is_some_and(|window| window.has_focus()),
            );
        }
        if focus && let Some(active) = active {
            host.focus(&mut self.processor, active);
        }
        self.update_chrome_title();
    }

    #[cfg(windows)]
    fn focus_active_pane(&mut self) {
        let Some(active) = active_pane_for_chrome_focus(self.tabs.active_window()) else {
            return;
        };
        if let Some(host) = self.host() {
            host.focus(&mut self.processor, active);
        }
    }

    fn update_chrome_title(&self) {
        if let (Some(chrome), Some(tab)) = (&self.chrome, self.tabs.active()) {
            chrome.set_title(&tab.title);
        }
    }

    fn close_tab(&mut self, index: usize) {
        let Some(window_id) = self.tabs.as_slice().get(index).map(|tab| tab.window_id) else {
            return;
        };
        if let Some(window) = self.processor.window_mut(window_id) {
            window.request_close();
        }
    }

    fn close_all(&mut self) {
        self.closing = true;
        let ids = self.tabs.as_slice().iter().map(|tab| tab.window_id).collect::<Vec<_>>();
        for id in ids {
            if let Some(window) = self.processor.window_mut(id) {
                window.request_close();
            }
        }
    }

    fn reap_tabs(&mut self, event_loop: &ActiveEventLoop) {
        let closed = self
            .tabs
            .as_slice()
            .iter()
            .filter(|tab| self.processor.window(tab.window_id).is_none())
            .map(|tab| tab.window_id)
            .collect::<Vec<_>>();
        if closed.is_empty() {
            return;
        }
        for id in closed {
            self.tabs.remove(id);
            self.tab_options.remove(&id);
        }
        if self.tabs.is_empty() {
            event_loop.exit();
        } else {
            self.sync_visibility_geometry_and_focus(true);
            self.request_redraw();
        }
    }

    fn refresh_titles(&mut self) {
        let updates = self
            .tabs
            .as_slice()
            .iter()
            .filter_map(|tab| {
                let title = self.processor.window(tab.window_id)?.title().to_owned();
                (title != tab.title).then_some((tab.window_id, title))
            })
            .collect::<Vec<_>>();
        if updates.is_empty() {
            return;
        }
        for (id, title) in updates {
            self.tabs.update_title(id, title);
        }
        self.update_chrome_title();
        self.request_redraw();
    }

    fn drain_shell_actions(&mut self, event_loop: &ActiveEventLoop) {
        for request in self.processor.take_shell_actions() {
            match request.action {
                ShellAction::CreateTab(options) => {
                    let mut inherited = self.inherited_tab_options(Some(request.source));
                    if options.terminal_options.working_directory.is_some() {
                        inherited.terminal_options.working_directory =
                            options.terminal_options.working_directory;
                    }
                    if let Err(error) = self.create_tab(event_loop, inherited) {
                        log::error!("could not create tab: {error}");
                    }
                },
                ShellAction::SelectNextTab => {
                    if self.tabs.cycle(1) {
                        self.sync_visibility_geometry_and_focus(true);
                        self.request_redraw();
                    }
                },
                ShellAction::SelectPreviousTab => {
                    if self.tabs.cycle(-1) {
                        self.sync_visibility_geometry_and_focus(true);
                        self.request_redraw();
                    }
                },
                ShellAction::SelectTab(index) => self.switch_to(index),
                ShellAction::SelectLastTab => {
                    if let Some(index) = self.tabs.as_slice().len().checked_sub(1) {
                        self.switch_to(index);
                    }
                },
                ShellAction::Minimize => {
                    if let Some(chrome) = &self.chrome {
                        chrome.set_minimized(true);
                    }
                },
                ShellAction::ToggleMaximized => {
                    if let Some(chrome) = &self.chrome {
                        chrome.set_maximized(!chrome.is_maximized());
                    }
                },
                ShellAction::ToggleFullscreen => {
                    if let Some(chrome) = &self.chrome {
                        let fullscreen =
                            chrome.fullscreen().is_none().then_some(Fullscreen::Borderless(None));
                        chrome.set_fullscreen(fullscreen);
                    }
                },
                ShellAction::Hide => {
                    if let Some(chrome) = &self.chrome {
                        chrome.set_visible(false);
                    }
                },
                ShellAction::Activate => {
                    if self.tabs.select_window(request.source) {
                        self.sync_visibility_geometry_and_focus(true);
                        self.request_redraw();
                    } else if self.tabs.active_window() == Some(request.source) {
                        self.sync_visibility_geometry_and_focus(true);
                    }
                },
                ShellAction::Resize { width, height } => {
                    if let Some(chrome) = &self.chrome {
                        let tab_height = self.layout.tab_bar.height;
                        let bottom = chrome
                            .inner_size()
                            .height
                            .saturating_sub(self.layout.content.height.saturating_add(tab_height));
                        let _ = chrome.request_inner_size(winit::dpi::PhysicalSize::new(
                            width,
                            height.saturating_add(tab_height).saturating_add(bottom),
                        ));
                    }
                },
                ShellAction::SetPosition { x, y } => {
                    if let Some(chrome) = &self.chrome {
                        chrome.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
                    }
                },
                ShellAction::SetVisible(visible) => {
                    if let Some(chrome) = &self.chrome {
                        chrome.set_visible(visible);
                    }
                },
                ShellAction::SetLevel(level) => {
                    if let Some(chrome) = &self.chrome {
                        chrome.set_window_level(level);
                    }
                },
            }
        }
    }

    fn drain_host_requests(&mut self, event_loop: &ActiveEventLoop) {
        for request in self.processor.take_host_requests() {
            let result = match request.method.as_str() {
                "create_window" => self.host_create_window(event_loop, &request),
                unknown => Err(IpcError::new(
                    "unsupported",
                    format!("Vivido tab host does not answer {unknown}"),
                )),
            };
            match result {
                Ok(result) => request.connection.reply(request.id, result),
                Err(error) => request.connection.error(request.id, error),
            }
        }
    }

    fn host_create_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        request: &IpcRequest,
    ) -> Result<serde_json::Value, IpcError> {
        let options: WindowOptions = serde_json::from_value(request.params.clone())
            .map_err(|error| IpcError::new("invalid_params", error.to_string()))?;
        let window_id = self.create_tab(event_loop, options).map_err(|error| {
            IpcError::new("invalid_params", format!("failed to create tab: {error}"))
        })?;
        let ipc_window_id =
            self.processor
                .window(window_id)
                .map(|window| window.ipc_window_id())
                .ok_or_else(|| IpcError::new("window_not_found", "new tab disappeared"))?;
        Ok(json!({"window_id": ipc_window_id}))
    }

    fn request_redraw(&self) {
        if let Some(chrome) = &self.chrome {
            chrome.request_redraw();
        }
    }

    fn update_accessibility(&mut self) {
        let title = self
            .tabs
            .active()
            .map_or(self.config.window.identity.title.as_str(), |tab| tab.title.as_str())
            .to_owned();
        #[cfg(target_os = "linux")]
        let terminal = if terminal_document_enabled() {
            self.tabs
                .active_window()
                .and_then(|id| self.processor.window(id))
                .map(|window| window.accessibility_snapshot())
        } else {
            None
        };
        #[cfg(windows)]
        let terminal = None;
        if let Some(accessibility) = &mut self.accessibility {
            accessibility.update(ChromeState {
                title: &title,
                tabs: &self.tabs,
                layout: self.layout,
                hits: &self.hits,
                draw_controls: self.draw_controls,
                menu: self.menu.as_ref(),
                terminal: terminal.as_ref(),
            });
        }
    }

    fn drain_accessibility_commands(&mut self, event_loop: &ActiveEventLoop) {
        let commands =
            self.accessibility.as_ref().map(ShellAccessibility::take_commands).unwrap_or_default();
        for command in commands {
            match command {
                AccessibilityCommand::SelectTab(index) => self.switch_to(index),
                AccessibilityCommand::CloseTab(index) => self.close_tab(index),
                AccessibilityCommand::NewTab => {
                    let options = self.inherited_tab_options(None);
                    if let Err(error) = self.create_tab(event_loop, options) {
                        log::error!("could not create accessible tab: {error}");
                    }
                },
                AccessibilityCommand::ShowNewTabMenu => self.toggle_new_tab_menu(event_loop),
                #[cfg(target_os = "linux")]
                AccessibilityCommand::MenuItem(index) => {
                    self.activate_menu_entry(event_loop, index);
                },
                AccessibilityCommand::PreviousTabs => {
                    self.tabs.shift_visible(-1, self.hits.tabs.len().max(1));
                    self.request_redraw();
                },
                AccessibilityCommand::NextTabs => {
                    self.tabs.shift_visible(1, self.hits.tabs.len().max(1));
                    self.request_redraw();
                },
                AccessibilityCommand::Minimize => {
                    if let Some(chrome) = &self.chrome {
                        chrome.set_minimized(true);
                    }
                },
                AccessibilityCommand::ToggleMaximized => {
                    if let Some(chrome) = &self.chrome {
                        chrome.set_maximized(!chrome.is_maximized());
                    }
                },
                AccessibilityCommand::CloseWindow => {
                    self.close_all();
                    event_loop.exit();
                },
            }
        }
    }

    fn render(&mut self) {
        let Some(chrome) = &self.chrome else { return };
        #[cfg(target_os = "linux")]
        self.processor.draw_pending_embedded_windows();

        #[cfg(target_os = "linux")]
        let frames = self
            .tabs
            .active_window()
            .and_then(|id| self.processor.embedded_frame(id))
            .map(|frame| EmbeddedFramePlacement {
                frame,
                origin: PhysicalPosition::new(
                    u32::try_from(self.layout.content.x.max(0)).unwrap_or_default(),
                    u32::try_from(self.layout.content.y.max(0)).unwrap_or_default(),
                ),
            })
            .into_iter()
            .collect::<Vec<_>>();
        #[cfg(windows)]
        let frames = Vec::<EmbeddedFramePlacement<'_>>::new();

        if let Some(renderer) = &mut self.renderer {
            match renderer.render(
                chrome.inner_size(),
                &mut self.tabs,
                self.draw_controls,
                &frames,
                self.menu.as_ref(),
            ) {
                Ok((layout, hits, _)) => {
                    let geometry_changed = layout != self.layout;
                    self.layout = layout;
                    self.hits = hits;
                    if geometry_changed {
                        self.sync_visibility_geometry_and_focus(false);
                    }
                    self.update_accessibility();
                },
                Err(error) => log::error!("could not render Vivido tab chrome: {error}"),
            }
        }
    }

    fn click_chrome(&mut self, event_loop: &ActiveEventLoop) {
        let Some(position) = self.cursor else { return };
        if self.hits.close.contains(position.x, position.y) {
            self.close_all();
            event_loop.exit();
            return;
        }
        if self.hits.minimize.contains(position.x, position.y) {
            if let Some(chrome) = &self.chrome {
                chrome.set_minimized(true);
            }
            return;
        }
        if self.hits.maximize.contains(position.x, position.y) {
            if let Some(chrome) = &self.chrome {
                chrome.set_maximized(!chrome.is_maximized());
            }
            return;
        }
        if let Some((index, _)) =
            self.hits.tab_closes.iter().find(|(_, rect)| rect.contains(position.x, position.y))
        {
            self.close_tab(*index);
            return;
        }
        if let Some((index, _)) =
            self.hits.tabs.iter().find(|(_, rect)| rect.contains(position.x, position.y))
        {
            self.switch_to(*index);
            return;
        }
        if self.hits.new_tab.contains(position.x, position.y) {
            let options = self.inherited_tab_options(None);
            if let Err(error) = self.create_tab(event_loop, options) {
                log::error!("could not create tab: {error}");
            }
            return;
        }
        let capacity = self.hits.tabs.len().max(1);
        if self.hits.previous.contains(position.x, position.y) {
            self.tabs.shift_visible(-1, capacity);
            self.request_redraw();
            return;
        }
        if self.hits.next.contains(position.x, position.y) {
            self.tabs.shift_visible(1, capacity);
            self.request_redraw();
            return;
        }
        if self.layout.tab_bar.contains(position.x, position.y) {
            let now = Instant::now();
            let double = self.last_title_click.is_some_and(|(at, previous)| {
                now.duration_since(at) <= Duration::from_millis(500)
                    && (previous.x - position.x).abs() <= 4.0
                    && (previous.y - position.y).abs() <= 4.0
            });
            if let Some(chrome) = &self.chrome {
                if double {
                    chrome.set_maximized(!chrome.is_maximized());
                    self.last_title_click = None;
                } else {
                    self.last_title_click = Some((now, position));
                    let _ = chrome.drag_window();
                }
            }
        }
    }

    fn resize_direction(&self, position: PhysicalPosition<f64>) -> Option<ResizeDirection> {
        let chrome = self.chrome.as_ref()?;
        if chrome.is_maximized() || chrome.fullscreen().is_some() {
            return None;
        }
        resize_direction_at(chrome.inner_size(), chrome.scale_factor(), position)
    }

    /// Whether the pointer is in a band which resizes the window rather than reaching the pane.
    fn pointer_over_resize_edge(&self) -> bool {
        self.pointer_resize_direction().is_some()
    }

    fn pointer_resize_direction(&self) -> Option<ResizeDirection> {
        self.cursor.and_then(|position| self.resize_direction(position))
    }

    fn update_resize_cursor(&self, position: PhysicalPosition<f64>) {
        let Some(chrome) = &self.chrome else { return };
        let icon =
            self.resize_direction(position).map(resize_cursor).unwrap_or(CursorIcon::Default);
        chrome.set_cursor(icon);
        chrome.set_cursor_visible(true);
    }

    #[cfg(target_os = "linux")]
    fn route_linux_input(&mut self, event: WindowEvent) {
        let Some(active) = self.tabs.active_window() else { return };
        let translated = match event {
            WindowEvent::CursorMoved { device_id, position } => {
                if !self.pointer_capture && !self.layout.content.contains(position.x, position.y) {
                    return;
                }
                WindowEvent::CursorMoved {
                    device_id,
                    position: PhysicalPosition::new(
                        position.x - f64::from(self.layout.content.x),
                        position.y - f64::from(self.layout.content.y),
                    ),
                }
            },
            WindowEvent::Touch(mut touch) => {
                if touch.phase == TouchPhase::Started {
                    if !self.layout.content.contains(touch.location.x, touch.location.y) {
                        return;
                    }
                    self.touch_capture.insert(touch.id);
                } else if !self.touch_capture.contains(&touch.id) {
                    return;
                }
                let ended = matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled);
                let touch_id = touch.id;
                touch.location.x -= f64::from(self.layout.content.x);
                touch.location.y -= f64::from(self.layout.content.y);
                let event = WindowEvent::Touch(touch);
                if ended {
                    self.touch_capture.remove(&touch_id);
                }
                event
            },
            event @ WindowEvent::MouseInput { state, .. } => match state {
                ElementState::Pressed
                    if self.cursor.is_some_and(|position| {
                        self.layout.content.contains(position.x, position.y)
                    }) =>
                {
                    self.pointer_capture = true;
                    event
                },
                ElementState::Released if self.pointer_capture => {
                    self.pointer_capture = false;
                    event
                },
                _ => return,
            },
            event @ WindowEvent::MouseWheel { .. }
                if self.cursor.is_some_and(|position| {
                    self.layout.content.contains(position.x, position.y)
                }) =>
            {
                event
            },
            WindowEvent::KeyboardInput { .. }
            | WindowEvent::ModifiersChanged(_)
            | WindowEvent::Ime(_)
            | WindowEvent::CursorEntered { .. }
            | WindowEvent::CursorLeft { .. } => event,
            _ => return,
        };
        self.processor.handle_embedded_window_event(active, translated);
        self.sync_linux_input_state(active);
    }

    #[cfg(target_os = "linux")]
    fn sync_linux_input_state(&self, window_id: WindowId) {
        let (Some(chrome), Some(state)) =
            (&self.chrome, self.processor.embedded_input_state(window_id))
        else {
            return;
        };
        // The frame owns the pointer while it sits in a resize band, so the pane's own cursor —
        // a text bar over the terminal — must not replace the grip the edge just set.
        if !self.pointer_over_resize_edge() {
            chrome.set_cursor(state.cursor);
            chrome.set_cursor_visible(state.cursor_visible);
        }
        chrome.set_ime_allowed(state.ime_allowed);
        if let Some((position, size)) = state.ime_cursor_area {
            chrome.set_ime_cursor_area(
                PhysicalPosition::new(
                    position.x + f64::from(self.layout.content.x),
                    position.y + f64::from(self.layout.content.y),
                ),
                size,
            );
        }
    }

    fn handle_chrome_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        if let (Some(accessibility), Some(chrome)) = (&mut self.accessibility, &self.chrome) {
            accessibility.process_event(chrome, &event);
        }
        match event {
            WindowEvent::CloseRequested => {
                self.close_all();
                event_loop.exit();
            },
            WindowEvent::Resized(size) => {
                self.close_menu();
                if let (Some(renderer), Some(chrome)) = (&mut self.renderer, &self.chrome) {
                    renderer.resize(size, chrome.scale_factor(), &self.config);
                }
                self.layout = compute_layout(
                    size,
                    self.chrome.as_ref().map_or(1.0, |window| window.scale_factor()),
                );
                self.sync_visibility_geometry_and_focus(false);
                self.request_redraw();
            },
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.close_menu();
                if let (Some(renderer), Some(chrome)) = (&mut self.renderer, &self.chrome) {
                    renderer.resize(chrome.inner_size(), scale_factor, &self.config);
                    self.layout = compute_layout(chrome.inner_size(), scale_factor);
                }
                self.sync_visibility_geometry_and_focus(false);
                self.request_redraw();
            },
            WindowEvent::RedrawRequested => self.render(),
            event @ WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(position);
                self.update_resize_cursor(position);
                // An open menu owns the pointer, so the pane never sees a move behind it.
                if let Some(changed) =
                    self.menu.as_mut().map(|menu| menu.hover(position.x, position.y))
                {
                    if changed {
                        self.refresh_menu();
                    }
                    return;
                }
                #[cfg(target_os = "linux")]
                self.route_linux_input(event);
                #[cfg(windows)]
                let _ = event;
            },
            WindowEvent::MouseInput { state: ElementState::Pressed, button, .. }
                if self.menu.is_some() =>
            {
                // A press anywhere dismisses the menu and never reaches the pane behind it; only a
                // left press on a row also launches it.
                let hit = (button == MouseButton::Left)
                    .then(|| {
                        self.cursor.and_then(|position| {
                            self.menu.as_ref().and_then(|menu| menu.hit(position.x, position.y))
                        })
                    })
                    .flatten();
                match hit {
                    Some(index) => self.activate_menu_entry(event_loop, index),
                    None => self.close_menu(),
                }
            },
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } if self
                .cursor
                .is_some_and(|position| self.hits.new_tab.contains(position.x, position.y)) =>
            {
                self.toggle_new_tab_menu(event_loop)
            },
            WindowEvent::KeyboardInput { event: key, .. } if self.menu.is_some() => {
                self.menu_key(event_loop, &key);
            },
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if self.pointer_over_resize_edge() => {
                if let (Some(chrome), Some(direction)) =
                    (&self.chrome, self.pointer_resize_direction())
                {
                    let _ = chrome.drag_resize_window(direction);
                }
            },
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if self
                .cursor
                .is_some_and(|position| self.layout.tab_bar.contains(position.x, position.y)) =>
            {
                self.click_chrome(event_loop)
            },
            #[cfg(windows)]
            WindowEvent::Focused(true) => {
                // The integrated chrome is the top-level activation target, while keyboard input
                // belongs to its active child pane. Windows can return focus to the chrome after
                // an application-mode transition; hand it straight back so the next key is not
                // discarded until a tab switch happens to call `SetFocus`. An open menu is the one
                // exception: it holds the keyboard until it closes.
                if self.menu.is_none() {
                    self.focus_active_pane();
                }
            },
            #[cfg(windows)]
            WindowEvent::Focused(false) => {},
            #[cfg(target_os = "linux")]
            WindowEvent::Focused(focused) => {
                if !focused {
                    self.dismiss_menu(false);
                }
                if let Some(active) = self.tabs.active_window() {
                    self.processor.set_embedded_window_focused(active, focused);
                }
            },
            #[cfg(target_os = "linux")]
            event => self.route_linux_input(event),
            #[cfg(windows)]
            _ => (),
        }
    }
}

#[cfg(windows)]
fn active_pane_for_chrome_focus(active: Option<WindowId>) -> Option<WindowId> {
    active
}

fn resize_direction_at(
    size: winit::dpi::PhysicalSize<u32>,
    scale_factor: f64,
    position: PhysicalPosition<f64>,
) -> Option<ResizeDirection> {
    let width = f64::from(size.width);
    let height = f64::from(size.height);
    // Clamping both bands to half the window keeps opposite edges from claiming the same pixel.
    let edge_x = (RESIZE_EDGE_LOGICAL * scale_factor).max(1.0).min(width / 2.0);
    let edge_y = (RESIZE_EDGE_LOGICAL * scale_factor).max(1.0).min(height / 2.0);
    let corner_x = (RESIZE_CORNER_LOGICAL * scale_factor).max(edge_x).min(width / 2.0);
    let corner_y = (RESIZE_CORNER_LOGICAL * scale_factor).max(edge_y).min(height / 2.0);

    let on_left = position.x < edge_x;
    let on_right = position.x >= width - edge_x;
    let on_top = position.y < edge_y;
    let on_bottom = position.y >= height - edge_y;
    if !(on_left || on_right || on_top || on_bottom) {
        return None;
    }

    // Along a horizontal edge the corner grip reaches further in from the sides, and vice versa.
    let vertical = on_top || on_bottom;
    let horizontal = on_left || on_right;
    let left = on_left || (vertical && position.x < corner_x);
    let right = on_right || (vertical && position.x >= width - corner_x);
    let top = on_top || (horizontal && position.y < corner_y);
    let bottom = on_bottom || (horizontal && position.y >= height - corner_y);
    match (left, right, top, bottom) {
        (true, false, true, false) => Some(ResizeDirection::NorthWest),
        (false, true, true, false) => Some(ResizeDirection::NorthEast),
        (true, false, false, true) => Some(ResizeDirection::SouthWest),
        (false, true, false, true) => Some(ResizeDirection::SouthEast),
        (true, false, false, false) => Some(ResizeDirection::West),
        (false, true, false, false) => Some(ResizeDirection::East),
        (false, false, true, false) => Some(ResizeDirection::North),
        (false, false, false, true) => Some(ResizeDirection::South),
        _ => None,
    }
}

fn resize_cursor(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::East | ResizeDirection::West => CursorIcon::EwResize,
        ResizeDirection::North | ResizeDirection::South => CursorIcon::NsResize,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::NeswResize,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::NwseResize,
    }
}

impl ApplicationHandler<Event> for TabbedApplication {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if cause == StartCause::Init
            && let Err(error) = self.initialize(event_loop)
        {
            log::error!("could not initialize Vivido tabs: {error}");
            event_loop.exit();
            return;
        }
        self.processor.handle_winit_event(event_loop, WinitEvent::NewEvents(cause));
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if Some(window_id) == self.chrome_id {
            self.handle_chrome_event(event_loop, event);
            return;
        }
        #[cfg(windows)]
        if self.menu_window.as_ref().is_some_and(|window| window.id() == window_id) {
            self.handle_menu_window_event(event_loop, event);
            return;
        }
        if self.processor.window(window_id).is_none() {
            return;
        }
        if matches!(event, WindowEvent::Focused(true)) && self.tabs.select_window(window_id) {
            self.sync_visibility_geometry_and_focus(true);
            self.request_redraw();
        }
        self.processor.handle_winit_event(event_loop, WinitEvent::WindowEvent { window_id, event });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        self.processor.handle_winit_event(event_loop, WinitEvent::UserEvent(event));
        self.request_redraw();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.processor.handle_winit_event(event_loop, WinitEvent::AboutToWait);
        self.drain_host_requests(event_loop);
        self.drain_shell_actions(event_loop);
        self.drain_accessibility_commands(event_loop);
        self.reap_tabs(event_loop);
        self.refresh_titles();
        self.update_accessibility();
        if self.processor.has_pending_embedded_redraw() {
            self.request_redraw();
        }
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        if !self.closing {
            self.close_all();
        }
        self.processor.handle_winit_event(event_loop, WinitEvent::LoopExiting);
        self.menu = None;
        #[cfg(windows)]
        {
            self.menu_window = None;
        }
        self.renderer = None;
        self.accessibility = None;
        self.chrome = None;
        self.chrome_id = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalSize;

    fn direction(x: f64, y: f64) -> Option<ResizeDirection> {
        resize_direction_at(PhysicalSize::new(800, 600), 1.0, PhysicalPosition::new(x, y))
    }

    #[test]
    fn every_corner_grips_its_diagonal() {
        let inside_corner = RESIZE_CORNER_LOGICAL - 1.0;
        assert_eq!(direction(0.0, 0.0), Some(ResizeDirection::NorthWest));
        assert_eq!(direction(inside_corner, 1.0), Some(ResizeDirection::NorthWest));
        assert_eq!(direction(1.0, inside_corner), Some(ResizeDirection::NorthWest));
        assert_eq!(direction(799.0, 0.0), Some(ResizeDirection::NorthEast));
        assert_eq!(direction(800.0 - inside_corner, 1.0), Some(ResizeDirection::NorthEast));
        assert_eq!(direction(0.0, 599.0), Some(ResizeDirection::SouthWest));
        assert_eq!(direction(1.0, 600.0 - inside_corner), Some(ResizeDirection::SouthWest));
        assert_eq!(direction(799.0, 599.0), Some(ResizeDirection::SouthEast));
        assert_eq!(direction(800.0 - inside_corner, 599.0), Some(ResizeDirection::SouthEast));
    }

    #[test]
    fn edges_outside_the_corners_resize_along_one_axis() {
        assert_eq!(direction(400.0, 1.0), Some(ResizeDirection::North));
        assert_eq!(direction(400.0, 599.0), Some(ResizeDirection::South));
        assert_eq!(direction(1.0, 300.0), Some(ResizeDirection::West));
        assert_eq!(direction(799.0, 300.0), Some(ResizeDirection::East));
    }

    #[test]
    fn the_window_interior_is_not_a_grip() {
        assert_eq!(direction(400.0, 300.0), None);
        assert_eq!(direction(RESIZE_EDGE_LOGICAL, RESIZE_CORNER_LOGICAL), None);
        assert_eq!(direction(RESIZE_CORNER_LOGICAL, RESIZE_EDGE_LOGICAL), None);
    }

    #[test]
    fn grips_scale_with_the_window_and_never_overlap() {
        let scaled = resize_direction_at(
            PhysicalSize::new(1600, 1200),
            2.0,
            PhysicalPosition::new(30.0, 4.0),
        );
        assert_eq!(scaled, Some(ResizeDirection::NorthWest));

        // A window narrower than two corner grips must still split them left from right.
        let tiny = PhysicalSize::new(20, 20);
        assert_eq!(
            resize_direction_at(tiny, 1.0, PhysicalPosition::new(5.0, 5.0)),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            resize_direction_at(tiny, 1.0, PhysicalPosition::new(15.0, 15.0)),
            Some(ResizeDirection::SouthEast)
        );
        assert_eq!(
            resize_direction_at(tiny, 1.0, PhysicalPosition::new(9.9, 1.0)),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            resize_direction_at(tiny, 1.0, PhysicalPosition::new(10.0, 1.0)),
            Some(ResizeDirection::NorthEast)
        );
    }

    #[cfg(windows)]
    #[test]
    fn focused_chrome_hands_keyboard_focus_to_the_active_pane() {
        let active = WindowId::from(7);

        assert_eq!(active_pane_for_chrome_focus(Some(active)), Some(active));
        assert_eq!(active_pane_for_chrome_focus(None), None);
    }
}
