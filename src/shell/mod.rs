//! Shared host primitives for applications which embed Vivido terminal panes.

#[cfg(any(target_os = "linux", windows))]
use std::error::Error;
#[cfg(any(target_os = "linux", windows))]
use std::path::Path;

#[cfg(any(target_os = "linux", windows))]
use winit::event_loop::ActiveEventLoop;
use winit::window::WindowId;
use winit::window::WindowLevel;

#[cfg(any(target_os = "linux", windows))]
use crate::Processor;
#[cfg(any(target_os = "linux", windows))]
use crate::cli::TerminalOptions;
use crate::cli::WindowOptions;

/// Maximum number of shell actions retained between event-loop turns.
pub const MAX_PENDING_SHELL_ACTIONS: usize = 64;

/// A window-management operation which must be performed by an embedding chrome.
#[derive(Clone, Debug)]
pub enum ShellAction {
    /// Create a terminal tab using the supplied options.
    CreateTab(Box<WindowOptions>),
    /// Select the next tab.
    SelectNextTab,
    /// Select the previous tab.
    SelectPreviousTab,
    /// Select a tab by index.
    SelectTab(usize),
    /// Select the last tab.
    SelectLastTab,
    /// Minimize the host window.
    Minimize,
    /// Toggle the host window's maximized state.
    ToggleMaximized,
    /// Toggle the host window's fullscreen state.
    ToggleFullscreen,
    /// Hide the host window.
    Hide,
    /// Activate the host window.
    Activate,
    /// Resize the host window in physical pixels.
    Resize {
        /// Width in this value's coordinate system.
        width: u32,
        /// Height in this value's coordinate system.
        height: u32,
    },
    /// Move the host window in physical pixels.
    SetPosition {
        /// Horizontal coordinate.
        x: i32,
        /// Vertical coordinate.
        y: i32,
    },
    /// Change host window visibility.
    SetVisible(bool),
    /// Change the host window stacking level.
    SetLevel(WindowLevel),
}

/// One shell action together with the terminal pane which originated it.
#[derive(Clone, Debug)]
pub struct ShellActionRequest {
    /// Window that originated this shell action.
    pub source: WindowId,
    /// Action to execute.
    pub action: ShellAction,
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(target_os = "linux", windows))]
mod tabs;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(any(target_os = "linux", windows))]
pub use tabs::{Tab, Tabs, VisibleTabs};
#[cfg(any(target_os = "linux", windows))]
mod chrome;
#[cfg(any(target_os = "linux", windows))]
pub use chrome::{ChromeHitMap, ChromeLayout, ChromeRenderer, TAB_BAR_LOGICAL, compute_layout};
pub mod close;
mod launch;
#[cfg(windows)]
mod touch;
pub use close::{CloseConfirmation, confirm_close};
pub use launch::{LaunchAction, LaunchEntry, entries as launch_entries};
#[cfg(windows)]
pub use touch::touch_click_events;
#[cfg(any(target_os = "linux", windows))]
mod menu;
#[cfg(any(target_os = "linux", windows))]
pub use menu::NewTabMenu;
#[cfg(any(target_os = "linux", windows))]
mod accessibility;
#[cfg(windows)]
mod menu_window;
#[cfg(any(target_os = "linux", windows))]
mod tabbed;
#[cfg(any(target_os = "linux", windows))]
pub use tabbed::TabbedApplication;

#[cfg(target_os = "linux")]
pub use linux::NativePaneHost;
#[cfg(target_os = "windows")]
pub use windows::NativePaneHost;

/// A pane rectangle in physical pixels relative to its host's client area.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysicalRect {
    /// Horizontal coordinate.
    pub x: i32,
    /// Vertical coordinate.
    pub y: i32,
    /// Width in this value's coordinate system.
    pub width: u32,
    /// Height in this value's coordinate system.
    pub height: u32,
}

impl PhysicalRect {
    /// Whether this rectangle contains the physical point `(x, y)`.
    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.x) + f64::from(self.width)
            && y < f64::from(self.y) + f64::from(self.height)
    }

    /// The exclusive right edge, clamped to the coordinate range.
    pub fn right(self) -> i32 {
        self.x.saturating_add(i32::try_from(self.width).unwrap_or(i32::MAX))
    }

    /// The exclusive bottom edge, clamped to the coordinate range.
    pub fn bottom(self) -> i32 {
        self.y.saturating_add(i32::try_from(self.height).unwrap_or(i32::MAX))
    }
}

/// Platform boundary used by a chrome window to own Vivido terminal panes.
#[cfg(any(target_os = "linux", windows))]
pub trait PaneHost {
    /// Create one terminal pane attached to this host.
    fn create_pane(
        &self,
        processor: &mut Processor,
        event_loop: &ActiveEventLoop,
        cwd: &Path,
        terminal_options: &TerminalOptions,
    ) -> Result<WindowId, Box<dyn Error>>;

    /// Create a pane while preserving the complete public window options.
    fn create_pane_with_options(
        &self,
        processor: &mut Processor,
        event_loop: &ActiveEventLoop,
        mut options: WindowOptions,
    ) -> Result<WindowId, Box<dyn Error>> {
        let cwd = options.terminal_options.working_directory.take().unwrap_or_default();
        self.create_pane(processor, event_loop, &cwd, &options.terminal_options)
    }

    /// Move and resize a pane within the host's physical client area.
    fn move_pane(&self, processor: &mut Processor, pane: WindowId, rect: PhysicalRect);

    /// Change whether a pane participates in rendering and automation visibility.
    fn reveal(&self, processor: &mut Processor, pane: WindowId, visible: bool);

    /// Give keyboard focus to a pane through its owning chrome window.
    fn focus(&self, processor: &mut Processor, pane: WindowId);

    /// Whether the pane is still attached to this host.
    fn is_attached(&self, processor: &Processor, pane: WindowId) -> bool;
}

#[cfg(test)]
mod tests {
    use super::PhysicalRect;

    #[test]
    fn physical_rect_edges_are_bounded_and_exclusive() {
        let rect = PhysicalRect { x: 10, y: 20, width: 30, height: 40 };
        assert!(rect.contains(10.0, 20.0));
        assert!(rect.contains(39.9, 59.9));
        assert!(!rect.contains(40.0, 60.0));
        assert_eq!(rect.right(), 40);
        assert_eq!(rect.bottom(), 60);
    }
}
