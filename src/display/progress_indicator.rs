//! Mirror OSC 9;4 progress onto the platform's application indicator.
//!
//! Windows draws progress in each top-level window's taskbar button through `ITaskbarList3`, so
//! [`set_taskbar_progress`] takes the window. The macOS Dock tile belongs to the application, so
//! [`set_dock_progress`] takes one combined state (see [`super::progress::most_urgent`]) and shows
//! it as a badge. Wayland has no portable equivalent, so Linux has neither.
//!
//! Both are best-effort: a missing shell or a window without a taskbar button leaves the
//! in-surface bar as the only indicator. Callers cache the last state they applied and call again
//! only when it changes.

#[cfg(any(windows, target_os = "macos"))]
use super::progress::Progress;
#[cfg(windows)]
use super::progress::ProgressKind;

/// Show `progress` in the taskbar button of `window`, or clear it with `None`.
#[cfg(windows)]
pub fn set_taskbar_progress(
    window: &impl winit::raw_window_handle::HasWindowHandle,
    progress: Option<Progress>,
) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{
        TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED,
    };
    use winit::raw_window_handle::RawWindowHandle;

    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
    let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
    let Some(taskbar) = taskbar_list() else { return };

    let (state, percent) = match progress {
        None => (TBPF_NOPROGRESS, None),
        Some(Progress { kind, percent }) => match kind {
            ProgressKind::Normal => (TBPF_NORMAL, percent),
            ProgressKind::Indeterminate => (TBPF_INDETERMINATE, None),
            ProgressKind::Paused => (TBPF_PAUSED, percent),
            ProgressKind::Error => (TBPF_ERROR, percent),
        },
    };
    // SAFETY: `hwnd` is the live window `window` owns, and the COM object belongs to this thread.
    // Setting a value switches an idle button to normal progress, so the value goes first and the
    // state then selects the colour.
    unsafe {
        if let Some(percent) = percent {
            let _ = taskbar.SetProgressValue(hwnd, u64::from(percent), 100);
        }
        let _ = taskbar.SetProgressState(hwnd, state);
    }
}

/// The UI thread's taskbar COM object, created on first use; `None` when the shell has none.
#[cfg(windows)]
fn taskbar_list() -> Option<windows::Win32::UI::Shell::ITaskbarList3> {
    use std::cell::OnceCell;

    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};

    thread_local! {
        static TASKBAR: OnceCell<Option<ITaskbarList3>> = const { OnceCell::new() };
    }

    TASKBAR.with(|taskbar| {
        taskbar
            .get_or_init(|| {
                // SAFETY: plain COM initialization and activation on the calling thread. The
                // apartment stays initialized for the thread's life, like winit's own OLE setup;
                // an existing apartment of either model can still create the in-process object.
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                    let taskbar: ITaskbarList3 =
                        CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok()?;
                    taskbar.HrInit().ok()?;
                    Some(taskbar)
                }
            })
            .clone()
    })
}

/// Show `progress` as the application's Dock badge, or clear it with `None`.
#[cfg(target_os = "macos")]
pub fn set_dock_progress(progress: Option<Progress>) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    let Some(mtm) = MainThreadMarker::new() else { return };
    let label =
        progress.map(|progress| NSString::from_str(&super::progress::badge_label(progress)));
    NSApplication::sharedApplication(mtm).dockTile().setBadgeLabel(label.as_deref());
}
