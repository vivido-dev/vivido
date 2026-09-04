//! Wayland pane host using Vivido's in-process embedded render targets.

use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

use super::{PaneHost, PhysicalRect};
use crate::cli::TerminalOptions;
use crate::{Processor, WindowOptions};

/// Hosts offscreen Vivido panes inside one Wayland chrome surface.
#[derive(Clone)]
pub struct NativePaneHost {
    chrome: Arc<Window>,
}

impl NativePaneHost {
    pub fn new(chrome: Arc<Window>) -> Self {
        Self { chrome }
    }
}

impl PaneHost for NativePaneHost {
    fn create_pane(
        &self,
        processor: &mut Processor,
        _event_loop: &ActiveEventLoop,
        cwd: &Path,
        terminal_options: &TerminalOptions,
    ) -> Result<WindowId, Box<dyn Error>> {
        let mut options = WindowOptions::default();
        options.terminal_options = terminal_options.clone();
        options.no_activate = true;
        options.terminal_options.working_directory = Some(cwd.to_owned());
        let chrome_size = self.chrome.inner_size();
        processor.create_embedded_pane(
            PhysicalSize::new(chrome_size.width.max(160), chrome_size.height.max(80)),
            self.chrome.scale_factor(),
            options,
        )
    }

    fn create_pane_with_options(
        &self,
        processor: &mut Processor,
        _event_loop: &ActiveEventLoop,
        mut options: WindowOptions,
    ) -> Result<WindowId, Box<dyn Error>> {
        options.no_activate = true;
        let size = self.chrome.inner_size();
        processor.create_embedded_pane(
            PhysicalSize::new(size.width.max(160), size.height.max(80)),
            self.chrome.scale_factor(),
            options,
        )
    }

    fn move_pane(&self, processor: &mut Processor, pane: WindowId, rect: PhysicalRect) {
        if let Some(pane) = processor.window_mut(pane) {
            pane.display.window.set_outer_position(PhysicalPosition::new(rect.x, rect.y));
        }
        processor.resize_embedded_window(pane, PhysicalSize::new(rect.width, rect.height));
    }

    fn reveal(&self, processor: &mut Processor, pane: WindowId, visible: bool) {
        processor.set_embedded_window_visible(pane, visible);
        self.chrome.request_redraw();
    }

    fn focus(&self, _processor: &mut Processor, _pane: WindowId) {
        self.chrome.focus_window();
    }

    fn is_attached(&self, processor: &Processor, pane: WindowId) -> bool {
        processor.window(pane).is_some_and(|pane| pane.display.window.is_embedded())
    }
}
