//! Native Win32 child-window pane host.

use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetActiveWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetParent, SWP_NOACTIVATE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos,
};
use winit::event_loop::ActiveEventLoop;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

use super::{PaneHost, PhysicalRect};
use crate::cli::TerminalOptions;
use crate::{ParentWindowHandle, Processor, WindowOptions};

/// Hosts native Vivido child windows inside one Win32 chrome window.
#[derive(Clone)]
pub struct NativePaneHost {
    chrome: Arc<Window>,
}

impl NativePaneHost {
    pub fn new(chrome: Arc<Window>) -> Self {
        Self { chrome }
    }

    fn native_windows(&self, processor: &Processor, pane: WindowId) -> Option<(HWND, HWND)> {
        Some((
            hwnd(self.chrome.window_handle().ok()?.as_raw())?,
            hwnd(processor.window(pane)?.display.window.raw_window_handle()?)?,
        ))
    }
}

impl PaneHost for NativePaneHost {
    fn create_pane(
        &self,
        processor: &mut Processor,
        event_loop: &ActiveEventLoop,
        cwd: &Path,
        terminal_options: &TerminalOptions,
    ) -> Result<WindowId, Box<dyn Error>> {
        // SAFETY: the host retains the parent window until every child pane is destroyed.
        let parent = unsafe { ParentWindowHandle::new(self.chrome.window_handle()?.as_raw()) };
        let mut options = WindowOptions::default();
        options.terminal_options = terminal_options.clone();
        options.no_activate = true;
        options.parent_window = Some(parent);
        options.terminal_options.working_directory = Some(cwd.to_owned());
        processor.create_hosted_pane(crate::LoopHandle::Winit(event_loop), options)
    }

    fn create_pane_with_options(
        &self,
        processor: &mut Processor,
        event_loop: &ActiveEventLoop,
        mut options: WindowOptions,
    ) -> Result<WindowId, Box<dyn Error>> {
        // SAFETY: the host retains the parent window until every child pane is destroyed.
        let parent = unsafe { ParentWindowHandle::new(self.chrome.window_handle()?.as_raw()) };
        options.no_activate = true;
        options.parent_window = Some(parent);
        processor.create_hosted_pane(crate::LoopHandle::Winit(event_loop), options)
    }

    fn move_pane(&self, processor: &mut Processor, pane: WindowId, rect: PhysicalRect) {
        let Some((_, pane)) = self.native_windows(processor, pane) else { return };
        let (Ok(width), Ok(height)) = (i32::try_from(rect.width), i32::try_from(rect.height))
        else {
            return;
        };
        // SAFETY: the live child HWND belongs to this event-loop thread and uses parent-relative
        // client coordinates.
        unsafe {
            SetWindowPos(
                pane,
                std::ptr::null_mut(),
                rect.x,
                rect.y,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
    }

    fn reveal(&self, processor: &mut Processor, pane: WindowId, visible: bool) {
        if let Some(pane) = processor.window_mut(pane) {
            pane.set_automation_visible(visible);
        }
    }

    fn focus(&self, processor: &mut Processor, pane: WindowId) {
        let Some((chrome, pane)) = self.native_windows(processor, pane) else { return };
        // SAFETY: both HWNDs belong to the event-loop thread. Activate the top-level host before
        // assigning keyboard focus to its child pane.
        unsafe {
            SetForegroundWindow(chrome);
            SetActiveWindow(chrome);
            SetFocus(pane);
        }
    }

    fn is_attached(&self, processor: &Processor, pane: WindowId) -> bool {
        self.native_windows(processor, pane)
            // SAFETY: both HWNDs are live for the duration of this synchronous query.
            .is_some_and(|(chrome, pane)| unsafe { GetParent(pane) == chrome })
    }
}

fn hwnd(raw: RawWindowHandle) -> Option<HWND> {
    let RawWindowHandle::Win32(handle) = raw else { return None };
    Some(handle.hwnd.get() as HWND)
}
