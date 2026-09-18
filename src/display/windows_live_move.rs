//! Keep GPU window content attached to the native drag rectangle, including when Windows has
//! disabled full-window dragging. This is window-local; never change the desktop-wide preference.

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetWindowLongW, GetWindowRect, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    SetWindowPos, WM_MOVING, WM_NCDESTROY, WS_CHILD,
};

const SUBCLASS_ID: usize = 0x5649_5649;

/// Called on the thread which created this live HWND. The subclass owns no Rust state and removes
/// itself at native destruction, so renderer recreation cannot leave a dangling callback context.
pub(super) unsafe fn install(hwnd: HWND) -> windows::core::Result<()> {
    // SAFETY: the caller owns the live window on this thread. Hosted child surfaces move with their
    // top-level host and must not participate in the native title-bar move loop themselves.
    unsafe {
        if GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_CHILD != 0 {
            return Ok(());
        }
        if SetWindowSubclass(hwnd, Some(window_proc), SUBCLASS_ID, 0) == 0 {
            return Err(windows::core::Error::from_thread());
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    _data: usize,
) -> LRESULT {
    // SAFETY: Windows invokes this callback on the owning thread with the documented message
    // payload. Forward first so any native constraint handlers can adjust the drag rectangle.
    unsafe {
        if message == WM_NCDESTROY {
            RemoveWindowSubclass(hwnd, Some(window_proc), subclass_id);
        }
        let result = DefSubclassProc(hwnd, message, wparam, lparam);
        if message == WM_MOVING && lparam != 0 {
            let proposed = *(lparam as *const RECT);
            let mut current = RECT::default();
            if GetWindowRect(hwnd, &mut current) != 0
                && (current.left != proposed.left || current.top != proposed.top)
            {
                // WM_MOVE/WindowEvent::Moved arrives only on release in outline-drag mode.
                // Apply the proposed position now; DWM moves the complete retained visual tree
                // with its HWND without reshaping text, submitting scenes, or uploading assets.
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    proposed.left,
                    proposed.top,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
            return 1;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, SendMessageW, WS_POPUP,
    };

    struct TestWindow(HWND);

    impl TestWindow {
        fn new(parent: HWND) -> Self {
            // SAFETY: STATIC is a built-in window class. These invisible windows belong to the
            // test thread and are destroyed before their optional parent, without touching UI.
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    windows::core::w!("STATIC").as_ptr(),
                    std::ptr::null(),
                    if parent.is_null() { WS_POPUP } else { WS_CHILD },
                    100,
                    100,
                    320,
                    180,
                    parent,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            assert!(!hwnd.is_null());
            Self(hwnd)
        }

        fn rect(&self) -> (i32, i32, i32, i32) {
            let mut rect = RECT::default();
            // SAFETY: the guard owns this live HWND on the current thread.
            assert_ne!(unsafe { GetWindowRect(self.0, &mut rect) }, 0);
            (rect.left, rect.top, rect.right, rect.bottom)
        }

        fn moving(&self, x: i32, y: i32) {
            let mut proposed = RECT { left: x, top: y, right: x + 320, bottom: y + 180 };
            // SAFETY: synchronous delivery keeps the writable WM_MOVING payload alive until the
            // entire subclass chain has returned, as it would during the native drag loop.
            unsafe {
                SendMessageW(self.0, WM_MOVING, 0, (&mut proposed as *mut RECT) as LPARAM);
            }
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: this guard owns the window and is dropped on its creating thread.
            unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn drag_rectangle_moves_content_before_release_without_affecting_other_windows() {
        let window = TestWindow::new(std::ptr::null_mut());
        let other = TestWindow::new(std::ptr::null_mut());
        // Reproduce outline mode: WM_MOVING alone does not normally move the HWND.
        window.moving(250, 180);
        assert_eq!(window.rect(), (100, 100, 420, 280));
        // SAFETY: both installs operate on our live, thread-local test window. Reinstalling the
        // same callback/ID also covers renderer recreation without stacked duplicate subclasses.
        unsafe {
            install(window.0).unwrap();
            install(window.0).unwrap();
        }
        for (x, y) in [(250, 180), (275, 200), (-100, -50), (100, 100)] {
            window.moving(x, y);
            assert_eq!(window.rect(), (x, y, x + 320, y + 180));
            assert_eq!(other.rect(), (100, 100, 420, 280));
        }
    }

    #[test]
    fn hosted_child_does_not_install_a_title_bar_drag_handler() {
        let parent = TestWindow::new(std::ptr::null_mut());
        let child = TestWindow::new(parent.0);
        let before = child.rect();
        // SAFETY: the parent and child are live and owned by the current test thread.
        unsafe {
            install(child.0).unwrap();
        }
        child.moving(250, 180);
        assert_eq!(child.rect(), before);
    }
}
