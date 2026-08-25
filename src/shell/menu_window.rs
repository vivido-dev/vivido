//! Child window which presents the `+` menu above a Windows pane.
//!
//! Panes are real child windows of the chrome (`super::windows::NativePaneHost`) and always compose
//! above the chrome's own DirectComposition content, so a menu painted into the chrome scene would
//! be hidden behind the terminal. Giving the menu a sibling child window puts it back on top, using
//! the same hosting mechanism the panes already rely on.

use std::error::Error;
use std::sync::Arc;

use vello::Scene;
use vello::peniko::Color;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{SWP_NOACTIVATE, SetWindowPos};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::platform::windows::WindowAttributesExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorIcon, Window, WindowId};

use crate::ParentWindowHandle;
use crate::config::UiConfig;
use crate::display::renderer::SceneRenderer;
use crate::display::text::TextSystem;
use crate::display::window::RenderSource;

use super::chrome::text_system;
use super::menu::NewTabMenu;

/// A hidden-by-default popup, created once and reused for every menu the chrome opens.
///
/// Building a wgpu surface and a vello renderer costs far more than a menu is open for, so the
/// window outlives each individual dropdown.
pub(super) struct MenuWindow {
    window: Arc<Window>,
    renderer: SceneRenderer,
    text: TextSystem,
    scale: f64,
    origin: PhysicalPosition<i32>,
}

impl MenuWindow {
    pub(super) fn new(
        event_loop: &ActiveEventLoop,
        chrome: &Arc<Window>,
        config: &UiConfig,
    ) -> Result<Self, Box<dyn Error>> {
        let scale = chrome.scale_factor();
        // SAFETY: the chrome window owns this popup and is dropped only after it.
        let parent = unsafe { ParentWindowHandle::new(chrome.window_handle()?.as_raw()) };
        let attributes = Window::default_attributes()
            .with_decorations(false)
            .with_visible(false)
            .with_transparent(true)
            .with_inner_size(PhysicalSize::new(1_u32, 1_u32))
            .with_no_redirection_bitmap(true)
            .with_active(false);
        // SAFETY: as above — the parent outlives this child.
        let attributes = unsafe { attributes.with_parent_window(Some(parent.raw())) };
        let window = Arc::new(event_loop.create_window(attributes)?);
        window.set_cursor(CursorIcon::Default);
        let renderer = SceneRenderer::new(
            RenderSource::Surface(Arc::clone(&window)),
            window.inner_size(),
            true,
        )?;
        Ok(Self {
            window,
            renderer,
            text: text_system(config, scale),
            scale,
            origin: PhysicalPosition::new(0, 0),
        })
    }

    pub(super) fn id(&self) -> WindowId {
        self.window.id()
    }

    pub(super) fn request_redraw(&self) {
        self.window.request_redraw();
    }

    pub(super) fn rescale(&mut self, scale: f64, config: &UiConfig) {
        if (self.scale - scale).abs() <= f64::EPSILON {
            return;
        }
        self.scale = scale;
        self.text = text_system(config, scale);
    }

    /// Place the popup over the pane and give it the keyboard.
    ///
    /// Focus is what makes `Esc`, arrow keys, and dismiss-on-focus-loss work; the chrome hands it
    /// back to the active pane when the menu closes. Only `SetFocus` is used: the chrome is already
    /// the foreground window when a right-click opens a menu, and re-activating it would bounce
    /// focus straight back to the pane through the chrome's own activation handler.
    pub(super) fn show(&mut self, menu: &NewTabMenu) {
        let rect = menu.rect();
        let size = PhysicalSize::new(rect.width, rect.height);
        self.origin = PhysicalPosition::new(rect.x, rect.y);
        if let (Some(window), Ok(width), Ok(height)) =
            (self.hwnd(), i32::try_from(rect.width), i32::try_from(rect.height))
        {
            // A null insert-after with no `SWP_NOZORDER` raises the popup above its pane sibling.
            // SAFETY: the child HWND belongs to this event-loop thread and takes parent-relative
            // client coordinates, exactly as the pane host's own `SetWindowPos` does.
            unsafe {
                SetWindowPos(
                    window,
                    std::ptr::null_mut(),
                    rect.x,
                    rect.y,
                    width,
                    height,
                    SWP_NOACTIVATE,
                );
            }
        }
        self.renderer.resize(size);
        self.window.set_visible(true);
        if let Some(window) = self.hwnd() {
            // SAFETY: the popup HWND is live and belongs to this event-loop thread.
            unsafe {
                SetFocus(window);
            }
        }
        self.window.request_redraw();
    }

    pub(super) fn hide(&self) {
        self.window.set_visible(false);
    }

    pub(super) fn render(&mut self, menu: &NewTabMenu) {
        let mut scene = Scene::new();
        menu.paint(
            &mut scene,
            &mut self.text,
            self.scale,
            (-self.origin.x as f32, -self.origin.y as f32),
        );
        if let Err(error) = self.renderer.render(&scene, Color::from_rgba8(0, 0, 0, 0)) {
            log::error!("could not render the Vivido tab menu: {error}");
        }
    }

    /// Translate a point in this popup's coordinates into the chrome's.
    pub(super) fn to_chrome(&self, x: f64, y: f64) -> (f64, f64) {
        (x + f64::from(self.origin.x), y + f64::from(self.origin.y))
    }

    fn hwnd(&self) -> Option<HWND> {
        let RawWindowHandle::Win32(handle) = self.window.window_handle().ok()?.as_raw() else {
            return None;
        };
        Some(handle.hwnd.get() as HWND)
    }
}
