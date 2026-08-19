#[cfg(not(any(target_os = "macos", windows)))]
use winit::platform::startup_notify::{
    self, EventLoopExtStartupNotify, WindowAttributesExtStartupNotify,
};
#[cfg(not(any(target_os = "macos", windows)))]
use winit::window::ActivationToken;

#[cfg(not(any(target_os = "macos", windows)))]
use winit::platform::wayland::WindowAttributesExtWayland;

use std::cell::Cell;
use std::fmt::{self, Display, Formatter};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "macos")]
use {
    objc2::MainThreadMarker,
    objc2_app_kit::{NSColorSpace, NSView},
    winit::platform::macos::{OptionAsAlt, WindowAttributesExtMacOS, WindowExtMacOS},
};

use bitflags::bitflags;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
#[cfg(windows)]
use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
#[cfg(any(target_os = "macos", windows))]
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{
    CursorIcon, Fullscreen, ImePurpose, Theme, UserAttentionType, Window as WinitWindow,
    WindowAttributes, WindowId, WindowLevel,
};

use crate::terminal::index::Point;

use crate::cli::WindowOptions;
use crate::config::UiConfig;
use crate::config::window::{Decorations, Identity, WindowConfig};
use crate::display::SizeInfo;

/// This should match the definition of IDI_ICON from `Vivido.rc`.
#[cfg(windows)]
const IDI_ICON: u16 = 0x101;

/// Window errors.
#[derive(Debug)]
pub enum Error {
    /// Error creating the window.
    WindowCreation(winit::error::OsError),
}

/// Result of fallible operations concerning a Window.
type Result<T> = std::result::Result<T, Error>;

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::WindowCreation(err) => err.source(),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::WindowCreation(err) => write!(f, "Error creating window; {err}"),
        }
    }
}

impl From<winit::error::OsError> for Error {
    fn from(val: winit::error::OsError) -> Self {
        Error::WindowCreation(val)
    }
}

/// Source the renderer should present into.
///
/// A windowed [`Window`] presents through a windowing-system surface; a headless one has no
/// surface at all and the renderer keeps its offscreen target as the final image.
pub enum RenderSource {
    Surface(Arc<WinitWindow>),
    Offscreen,
    Embedded,
}

#[derive(Clone, Copy, Debug)]
pub struct EmbeddedInputState {
    pub cursor: CursorIcon,
    pub cursor_visible: bool,
    pub ime_allowed: bool,
    pub ime_cursor_area: Option<(PhysicalPosition<f64>, PhysicalSize<f64>)>,
}

/// Next identifier handed to a headless window.
///
/// The counter starts with the high bit set so a synthesized id can never collide with one minted
/// by a windowing system, which allocates from the bottom.
static NEXT_HEADLESS_ID: AtomicU64 = AtomicU64::new(1 << 63);

/// State a windowing system would otherwise own on our behalf.
///
/// Every field is a [`Cell`] because the windowing-system setters take `&self`; mirroring that
/// signature keeps all ~50 call sites of [`Window`] unchanged.
struct HeadlessWindow {
    id: WindowId,
    size: Cell<PhysicalSize<u32>>,
    position: Cell<PhysicalPosition<i32>>,
    visible: Cell<bool>,
    maximized: Cell<bool>,
    fullscreen: Cell<bool>,
    theme: Cell<Option<Theme>>,
    embedded: bool,
}

impl HeadlessWindow {
    fn new(size: PhysicalSize<u32>, theme: Option<Theme>, embedded: bool) -> Self {
        let id = WindowId::from(NEXT_HEADLESS_ID.fetch_add(1, Ordering::Relaxed));
        Self {
            id,
            size: Cell::new(size),
            position: Cell::new(PhysicalPosition::new(0, 0)),
            visible: Cell::new(true),
            maximized: Cell::new(false),
            fullscreen: Cell::new(false),
            theme: Cell::new(theme),
            embedded,
        }
    }
}

/// Where a [`Window`]'s frames go.
enum Backend {
    Winit(Arc<WinitWindow>),
    Headless(HeadlessWindow),
}

impl Backend {
    fn id(&self) -> WindowId {
        match self {
            Self::Winit(window) => window.id(),
            Self::Headless(headless) => headless.id,
        }
    }

    fn inner_size(&self) -> PhysicalSize<u32> {
        match self {
            Self::Winit(window) => window.inner_size(),
            Self::Headless(headless) => headless.size.get(),
        }
    }

    fn outer_position(&self) -> Option<PhysicalPosition<i32>> {
        match self {
            Self::Winit(window) => window.outer_position().ok(),
            Self::Headless(headless) => Some(headless.position.get()),
        }
    }

    fn is_maximized(&self) -> bool {
        match self {
            Self::Winit(window) => window.is_maximized(),
            Self::Headless(headless) => headless.maximized.get(),
        }
    }

    fn is_fullscreen(&self) -> bool {
        match self {
            Self::Winit(window) => window.fullscreen().is_some(),
            Self::Headless(headless) => headless.fullscreen.get(),
        }
    }

    fn winit(&self) -> Option<&Arc<WinitWindow>> {
        match self {
            Self::Winit(window) => Some(window),
            Self::Headless(_) => None,
        }
    }
}

/// A window which can be used for displaying the terminal.
///
/// Wraps the underlying windowing library to provide a stable API in Vivido. A window is either
/// backed by the windowing system or, in headless mode, by nothing at all: it owns its geometry
/// directly and every presentation-only operation becomes a no-op.
pub struct Window {
    /// Flag tracking that we have a frame we can draw.
    pub has_frame: bool,

    /// Cached scale factor for quickly scaling pixel sizes.
    pub scale_factor: f64,

    /// Flag indicating whether redraw was requested.
    pub requested_redraw: bool,

    /// Hold the window when terminal exits.
    pub hold: bool,

    backend: Backend,

    /// Current window title.
    title: String,

    current_mouse_cursor: CursorIcon,
    mouse_visible: bool,
    ime_inhibitor: ImeInhibitor,
    ime_cursor_area: Cell<Option<(PhysicalPosition<f64>, PhysicalSize<f64>)>>,
}

impl Window {
    /// Create a new window.
    ///
    /// This creates a window and fully initializes a window.
    pub fn new(
        event_loop: &ActiveEventLoop,
        config: &UiConfig,
        identity: &Identity,
        options: &mut WindowOptions,
    ) -> Result<Window> {
        let identity = identity.clone();
        let mut window_attributes = Window::get_platform_window(
            &identity,
            &config.window,
            #[cfg(target_os = "macos")]
            &options.window_tabbing_id.take(),
        );

        if let Some(position) = config.window.position {
            window_attributes = window_attributes
                .with_position(PhysicalPosition::<i32>::from((position.x, position.y)));
        }

        #[cfg(not(any(target_os = "macos", windows)))]
        if let Some(token) = options
            .activation_token
            .take()
            .map(ActivationToken::from_raw)
            .or_else(|| event_loop.read_token_from_env())
        {
            log::debug!("Activating window with token: {token:?}");
            window_attributes = window_attributes.with_activation_token(token);

            // Remove the token from the env.
            startup_notify::reset_activation_token_env();
        }

        window_attributes = window_attributes
            .with_title(&identity.title)
            .with_theme(config.window.theme())
            .with_visible(false)
            .with_transparent(true)
            .with_blur(config.window.blur)
            .with_maximized(config.window.maximized())
            .with_fullscreen(config.window.fullscreen())
            .with_window_level(config.window.level.into());

        #[cfg(windows)]
        {
            window_attributes = window_attributes.with_no_redirection_bitmap(true);
        }

        #[cfg(windows)]
        if options.no_activate {
            window_attributes = window_attributes.with_active(false);
        }

        #[cfg(any(target_os = "macos", windows))]
        if let Some(parent) = options.parent_window {
            // SAFETY: `ParentWindowHandle::new` requires the caller to keep the parent alive until
            // after its children. Window creation and all subsequent access happen on this active
            // event-loop thread.
            window_attributes = unsafe { window_attributes.with_parent_window(Some(parent.raw())) };
        }

        let window = Arc::new(event_loop.create_window(window_attributes)?);

        // Text cursor.
        let current_mouse_cursor = CursorIcon::Text;
        window.set_cursor(current_mouse_cursor);

        // Enable IME.
        window.set_ime_allowed(true);
        window.set_ime_purpose(ImePurpose::Terminal);

        // Set initial transparency hint.
        window.set_transparent(config.window_opacity() < 1.);

        #[cfg(target_os = "macos")]
        use_srgb_color_space(&window);

        let scale_factor = window.scale_factor();
        log::info!("Window scale factor: {scale_factor}");
        Ok(Self {
            hold: options.terminal_options.hold,
            requested_redraw: false,
            title: identity.title,
            current_mouse_cursor,
            mouse_visible: true,
            has_frame: true,
            scale_factor,
            backend: Backend::Winit(window),
            ime_inhibitor: Default::default(),
            ime_cursor_area: Cell::new(None),
        })
    }

    /// Create a window with no windowing system behind it.
    ///
    /// The window owns its own geometry and scale factor, so it is immediately "mapped" at
    /// `size` and never receives resize, focus, or occlusion events from a compositor.
    pub fn headless(
        config: &UiConfig,
        identity: &Identity,
        options: &WindowOptions,
        size: PhysicalSize<u32>,
        scale_factor: f64,
    ) -> Window {
        let identity = identity.clone();
        let headless = HeadlessWindow::new(size, config.window.theme(), false);
        log::info!("Headless window {:?} at {}x{}", headless.id, size.width, size.height);

        Self {
            hold: options.terminal_options.hold,
            requested_redraw: false,
            title: identity.title,
            current_mouse_cursor: CursorIcon::Text,
            mouse_visible: true,
            has_frame: true,
            scale_factor,
            backend: Backend::Headless(headless),
            ime_inhibitor: Default::default(),
            ime_cursor_area: Cell::new(None),
        }
    }

    /// Create a window whose frames are composited into an in-process host surface.
    pub fn embedded(
        config: &UiConfig,
        identity: &Identity,
        options: &WindowOptions,
        size: PhysicalSize<u32>,
        scale_factor: f64,
    ) -> Window {
        let identity = identity.clone();
        let embedded = HeadlessWindow::new(size, config.window.theme(), true);
        log::info!("Embedded window {:?} at {}x{}", embedded.id, size.width, size.height);
        Self {
            hold: options.terminal_options.hold,
            requested_redraw: false,
            title: identity.title,
            current_mouse_cursor: CursorIcon::Text,
            mouse_visible: true,
            has_frame: true,
            scale_factor,
            backend: Backend::Headless(embedded),
            ime_inhibitor: Default::default(),
            ime_cursor_area: Cell::new(None),
        }
    }

    /// Whether this window has no windowing system behind it.
    #[inline]
    pub fn is_headless(&self) -> bool {
        matches!(&self.backend, Backend::Headless(window) if !window.embedded)
    }

    #[inline]
    pub fn is_embedded(&self) -> bool {
        matches!(&self.backend, Backend::Headless(window) if window.embedded)
    }

    pub fn embedded_input_state(&self) -> Option<EmbeddedInputState> {
        self.is_embedded().then_some(EmbeddedInputState {
            cursor: self.current_mouse_cursor,
            cursor_visible: self.mouse_visible,
            ime_allowed: self.ime_inhibitor.is_empty(),
            ime_cursor_area: self.ime_cursor_area.get(),
        })
    }

    #[cfg(any(target_os = "linux", windows))]
    pub(crate) fn winit_window(&self) -> Option<&WinitWindow> {
        self.backend.winit().map(AsRef::as_ref)
    }

    #[cfg(any(target_os = "macos", windows))]
    #[inline]
    pub fn raw_window_handle(&self) -> Option<RawWindowHandle> {
        Some(self.backend.winit()?.window_handle().unwrap().as_raw())
    }

    /// The source the renderer should present into.
    #[inline]
    pub fn render_source(&self) -> RenderSource {
        match &self.backend {
            Backend::Winit(window) => RenderSource::Surface(Arc::clone(window)),
            Backend::Headless(window) if window.embedded => RenderSource::Embedded,
            Backend::Headless(_) => RenderSource::Offscreen,
        }
    }

    #[inline]
    pub fn request_inner_size(&self, size: PhysicalSize<u32>) {
        match &self.backend {
            Backend::Winit(window) => {
                let _ = window.request_inner_size(size);
            },
            // Nothing can refuse the request, so it takes effect immediately.
            Backend::Headless(headless) => headless.size.set(size),
        }
    }

    #[inline]
    pub fn inner_size(&self) -> PhysicalSize<u32> {
        self.backend.inner_size()
    }

    /// Current window scale factor.
    #[inline]
    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }

    /// Move the window's outer frame to a physical screen position.
    #[inline]
    pub fn set_outer_position(&self, position: PhysicalPosition<i32>) {
        match &self.backend {
            Backend::Winit(window) => window.set_outer_position(position),
            // Nothing can refuse the request, so it takes effect immediately.
            Backend::Headless(headless) => headless.position.set(position),
        }
    }

    /// Physical screen position of the window's outer frame.
    ///
    /// [`None`] when the windowing system refuses to report one.
    #[inline]
    pub fn outer_position(&self) -> Option<PhysicalPosition<i32>> {
        self.backend.outer_position()
    }

    #[inline]
    pub fn set_visible(&self, visibility: bool) {
        match &self.backend {
            Backend::Winit(window) => window.set_visible(visibility),
            Backend::Headless(headless) => headless.visible.set(visibility),
        }
    }

    /// Whether the window is currently mapped.
    ///
    /// [`None`] when the windowing system cannot answer.
    #[inline]
    pub fn is_visible(&self) -> Option<bool> {
        match &self.backend {
            Backend::Winit(window) => window.is_visible(),
            Backend::Headless(headless) => Some(headless.visible.get()),
        }
    }

    /// Set the window's stacking level relative to other windows.
    #[inline]
    pub fn set_window_level(&self, level: WindowLevel) {
        if let Some(window) = self.backend.winit() {
            window.set_window_level(level);
        }
    }

    /// Map the window without making it key.
    ///
    /// Winit's `set_visible(true)` is `makeKeyAndOrderFront:`, which steals focus from whichever
    /// application is active. A window used as another application's pane must appear without
    /// taking the keyboard away from the host it is being attached to.
    #[cfg(target_os = "macos")]
    pub fn order_front_without_focus(&self) {
        let view = match self.raw_window_handle() {
            Some(RawWindowHandle::AppKit(handle)) => {
                assert!(MainThreadMarker::new().is_some());
                unsafe { handle.ns_view.cast::<NSView>().as_ref() }
            },
            _ => return,
        };

        // `orderFrontRegardless` also works across applications, which `orderFront:` does not do
        // while another application is active — exactly the case a pane is created in.
        view.window().unwrap().orderFrontRegardless();
    }

    #[inline]
    #[cfg(any(unix, windows))]
    pub fn focus_window(&self) {
        let Some(window) = self.backend.winit() else { return };

        // Direct focus is intentionally a no-op on Wayland. Winit's attention request uses
        // xdg_activation_v1 to obtain and apply a compositor-approved activation token.
        #[cfg(target_os = "linux")]
        window.request_user_attention(Some(UserAttentionType::Critical));
        window.focus_window();
    }

    /// Set the window title.
    #[inline]
    pub fn set_title(&mut self, title: String) {
        self.title = title;
        if let Some(window) = self.backend.winit() {
            window.set_title(&self.title);
        }
    }

    /// Get the window title.
    #[inline]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[inline]
    pub fn request_redraw(&mut self) {
        if !self.requested_redraw {
            self.requested_redraw = true;
            // Headless windows have no windowing system to ask; `requested_redraw` alone is what
            // the headless event loop polls to decide whether to draw.
            if let Some(window) = self.backend.winit() {
                window.request_redraw();
            }
        }
    }

    #[inline]
    pub fn set_mouse_cursor(&mut self, cursor: CursorIcon) {
        if cursor != self.current_mouse_cursor {
            self.current_mouse_cursor = cursor;
            if let Some(window) = self.backend.winit() {
                window.set_cursor(cursor);
            }
        }
    }

    /// Set mouse cursor visible.
    pub fn set_mouse_visible(&mut self, visible: bool) {
        if visible != self.mouse_visible {
            self.mouse_visible = visible;
            if let Some(window) = self.backend.winit() {
                window.set_cursor_visible(visible);
            }
        }
    }

    #[inline]
    pub fn mouse_visible(&self) -> bool {
        self.mouse_visible
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn get_platform_window(
        identity: &Identity,
        window_config: &WindowConfig,
    ) -> WindowAttributes {
        WinitWindow::default_attributes()
            .with_name(&identity.class.general, &identity.class.instance)
            .with_decorations(window_config.decorations != Decorations::None)
    }

    #[cfg(windows)]
    pub fn get_platform_window(_: &Identity, window_config: &WindowConfig) -> WindowAttributes {
        let icon = winit::window::Icon::from_resource(IDI_ICON, None);

        WinitWindow::default_attributes()
            .with_decorations(window_config.decorations != Decorations::None)
            .with_window_icon(icon.as_ref().ok().cloned())
            .with_taskbar_icon(icon.ok())
    }

    #[cfg(target_os = "macos")]
    pub fn get_platform_window(
        _: &Identity,
        window_config: &WindowConfig,
        tabbing_id: &Option<String>,
    ) -> WindowAttributes {
        let mut window =
            WinitWindow::default_attributes().with_option_as_alt(window_config.option_as_alt());

        if let Some(tabbing_id) = tabbing_id {
            window = window.with_tabbing_identifier(tabbing_id);
        }

        match window_config.decorations {
            Decorations::Full => window,
            Decorations::Transparent => window
                .with_title_hidden(true)
                .with_titlebar_transparent(true)
                .with_fullsize_content_view(true),
            Decorations::Buttonless => window
                .with_title_hidden(true)
                .with_titlebar_buttons_hidden(true)
                .with_titlebar_transparent(true)
                .with_fullsize_content_view(true),
            Decorations::None => window.with_titlebar_hidden(true),
        }
    }

    pub fn set_urgent(&self, is_urgent: bool) {
        let Some(window) = self.backend.winit() else { return };
        let attention = if is_urgent { Some(UserAttentionType::Critical) } else { None };

        window.request_user_attention(attention);
    }

    pub fn id(&self) -> WindowId {
        self.backend.id()
    }

    pub fn set_transparent(&self, transparent: bool) {
        if let Some(window) = self.backend.winit() {
            window.set_transparent(transparent);
        }
    }

    pub fn set_blur(&self, blur: bool) {
        if let Some(window) = self.backend.winit() {
            window.set_blur(blur);
        }
    }

    pub fn set_maximized(&self, maximized: bool) {
        match &self.backend {
            Backend::Winit(window) => window.set_maximized(maximized),
            Backend::Headless(headless) => headless.maximized.set(maximized),
        }
    }

    pub fn set_minimized(&self, minimized: bool) {
        if let Some(window) = self.backend.winit() {
            window.set_minimized(minimized);
        }
    }

    pub fn set_resize_increments(&self, increments: PhysicalSize<f32>) {
        if let Some(window) = self.backend.winit() {
            window.set_resize_increments(Some(increments));
        }
    }

    /// Toggle the window's fullscreen state.
    pub fn toggle_fullscreen(&self) {
        self.set_fullscreen(!self.backend.is_fullscreen());
    }

    /// Toggle the window's maximized state.
    pub fn toggle_maximized(&self) {
        self.set_maximized(!self.backend.is_maximized());
    }

    /// Inform windowing system about presenting to the window.
    ///
    /// Should be called right before presenting the rendered frame to the window surface.
    pub fn pre_present_notify(&self) {
        if let Some(window) = self.backend.winit() {
            window.pre_present_notify();
        }
    }

    pub fn set_theme(&self, theme: Option<Theme>) {
        match &self.backend {
            Backend::Winit(window) => window.set_theme(theme),
            Backend::Headless(headless) => headless.theme.set(theme),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn toggle_simple_fullscreen(&self) {
        let Some(window) = self.backend.winit() else { return };
        self.set_simple_fullscreen(!window.simple_fullscreen());
    }

    #[cfg(target_os = "macos")]
    pub fn set_option_as_alt(&self, option_as_alt: OptionAsAlt) {
        if let Some(window) = self.backend.winit() {
            window.set_option_as_alt(option_as_alt);
        }
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        match &self.backend {
            Backend::Winit(window) if fullscreen => {
                window.set_fullscreen(Some(Fullscreen::Borderless(None)))
            },
            Backend::Winit(window) => window.set_fullscreen(None),
            // Headless geometry is fixed by the caller; fullscreen is bookkeeping only.
            Backend::Headless(headless) => headless.fullscreen.set(fullscreen),
        }
    }

    pub fn current_monitor(&self) -> Option<MonitorHandle> {
        self.backend.winit()?.current_monitor()
    }

    #[cfg(target_os = "macos")]
    pub fn set_simple_fullscreen(&self, simple_fullscreen: bool) {
        if let Some(window) = self.backend.winit() {
            window.set_simple_fullscreen(simple_fullscreen);
        }
    }

    /// Set IME inhibitor state and disable IME while any are present.
    ///
    /// IME is re-enabled once all inhibitors are unset.
    pub fn set_ime_inhibitor(&mut self, inhibitor: ImeInhibitor, inhibit: bool) {
        if self.ime_inhibitor.contains(inhibitor) != inhibit {
            self.ime_inhibitor.set(inhibitor, inhibit);
            if let Some(window) = self.backend.winit() {
                window.set_ime_allowed(self.ime_inhibitor.is_empty());
            }
        }
    }

    /// Adjust the IME editor position according to the new location of the cursor.
    pub fn update_ime_position(&self, point: Point<usize>, size: &SizeInfo) {
        let nspot_x = f64::from(size.padding_x() + point.column.0 as f32 * size.cell_width());
        let nspot_y = f64::from(size.padding_y() + point.line as f32 * size.cell_height());

        // NOTE: some compositors don't like excluding too much and try to render popup at the
        // bottom right corner of the provided area, so exclude just the full-width char to not
        // obscure the cursor and not render popup at the end of the window.
        let width = size.cell_width() as f64 * 2.;
        let height = size.cell_height as f64;

        let position = PhysicalPosition::new(nspot_x, nspot_y);
        let size = PhysicalSize::new(width, height);
        self.ime_cursor_area.set(Some((position, size)));

        if let Some(window) = self.backend.winit() {
            window.set_ime_cursor_area(position, size);
        }
    }

    /// Disable macOS window shadows.
    ///
    /// This prevents rendering artifacts from showing up when the window is transparent.
    #[cfg(target_os = "macos")]
    pub fn set_has_shadow(&self, has_shadows: bool) {
        let view = match self.raw_window_handle() {
            Some(RawWindowHandle::AppKit(handle)) => {
                assert!(MainThreadMarker::new().is_some());
                unsafe { handle.ns_view.cast::<NSView>().as_ref() }
            },
            _ => return,
        };

        view.window().unwrap().setHasShadow(has_shadows);
    }

    /// Select tab at the given `index`.
    #[cfg(target_os = "macos")]
    pub fn select_tab_at_index(&self, index: usize) {
        if let Some(window) = self.backend.winit() {
            window.select_tab_at_index(index);
        }
    }

    /// Select the last tab.
    #[cfg(target_os = "macos")]
    pub fn select_last_tab(&self) {
        if let Some(window) = self.backend.winit() {
            window.select_tab_at_index(window.num_tabs() - 1);
        }
    }

    /// Select next tab.
    #[cfg(target_os = "macos")]
    pub fn select_next_tab(&self) {
        if let Some(window) = self.backend.winit() {
            window.select_next_tab();
        }
    }

    /// Select previous tab.
    #[cfg(target_os = "macos")]
    pub fn select_previous_tab(&self) {
        if let Some(window) = self.backend.winit() {
            window.select_previous_tab();
        }
    }

    #[cfg(target_os = "macos")]
    pub fn tabbing_id(&self) -> String {
        self.backend.winit().map(|window| window.tabbing_identifier()).unwrap_or_default()
    }
}

bitflags! {
    /// IME inhibition sources.
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct ImeInhibitor: u8 {
        const FOCUS = 1;
        const TOUCH = 1 << 1;
        const VI    = 1 << 2;
    }
}

#[cfg(target_os = "macos")]
fn use_srgb_color_space(window: &WinitWindow) {
    let view = match window.window_handle().unwrap().as_raw() {
        RawWindowHandle::AppKit(handle) => {
            assert!(MainThreadMarker::new().is_some());
            unsafe { handle.ns_view.cast::<NSView>().as_ref() }
        },
        _ => return,
    };

    view.window().unwrap().setColorSpace(Some(&NSColorSpace::sRGBColorSpace()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_window_retains_host_managed_state() {
        let config = UiConfig::default();
        let mut window = Window::embedded(
            &config,
            &Identity::default(),
            &WindowOptions::default(),
            PhysicalSize::new(640, 480),
            2.0,
        );
        assert!(window.is_embedded());
        assert!(!window.is_headless());
        assert!(matches!(window.render_source(), RenderSource::Embedded));

        window.request_inner_size(PhysicalSize::new(320, 200));
        window.set_outer_position(PhysicalPosition::new(10, 20));
        window.set_visible(false);
        window.set_mouse_cursor(CursorIcon::Pointer);
        assert_eq!(window.inner_size(), PhysicalSize::new(320, 200));
        assert_eq!(window.outer_position(), Some(PhysicalPosition::new(10, 20)));
        assert_eq!(window.is_visible(), Some(false));
        let input = window.embedded_input_state().unwrap();
        assert_eq!(input.cursor, CursorIcon::Pointer);
        assert!(input.cursor_visible);
        assert!(input.ime_allowed);
    }
}
