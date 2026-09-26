//! Ask before a user-initiated close stops a program that is still running.
//!
//! A terminal sitting at its shell prompt closes silently; one running `vim`, a build, or an agent
//! asks first. Callers collect the running programs with
//! [`crate::Processor::running_programs`] and call [`confirm_close`] only when that list is not
//! empty. Programmatic closes — IPC, automation, a process exiting on its own — never ask.
//!
//! Windows uses a task-modal message box and macOS an application-modal alert. Linux has no
//! native dialog under Wayland without a toolkit dependency, so there a close proceeds without
//! asking, as it always has.

/// What a close would stop, phrased for the confirmation dialog.
#[derive(Clone, Copy, Debug)]
pub struct CloseConfirmation<'a> {
    /// The dialog title, phrased as the question, such as `Close this tab?`.
    pub title: &'a str,
    /// What is being closed, completing "still running in …", such as `this tab`.
    pub subject: &'a str,
    /// The confirming button on platforms that label it, such as `Close` or `Quit`.
    pub action: &'a str,
    /// Names of the running programs, one per terminal; duplicates are listed once.
    pub programs: &'a [String],
}

/// Longest list of program names spelled out before the rest are counted.
const LISTED_PROGRAMS: usize = 4;

impl CloseConfirmation<'_> {
    /// The dialog body naming what would be stopped.
    pub fn message(&self) -> String {
        let mut names: Vec<&str> = Vec::new();
        for program in self.programs {
            if !names.contains(&program.as_str()) {
                names.push(program);
            }
        }
        match names.as_slice() {
            [] => format!("Closing {} will stop the programs running in it.", self.subject),
            [name] if self.programs.len() == 1 => {
                format!("{name} is still running in {}. Closing will stop it.", self.subject)
            },
            _ => {
                let mut list =
                    names.iter().take(LISTED_PROGRAMS).copied().collect::<Vec<_>>().join(", ");
                if names.len() > LISTED_PROGRAMS {
                    list.push_str(&format!(", and {} more", names.len() - LISTED_PROGRAMS));
                }
                format!(
                    "{} programs are still running in {}: {list}. Closing will stop them.",
                    self.programs.len(),
                    self.subject
                )
            },
        }
    }
}

/// Ask whether to go ahead with a close; `true` means close.
///
/// `owner` parents the dialog when the caller has a native window; without one the dialog is
/// still modal to the application.
pub fn confirm_close(
    owner: Option<&dyn winit::raw_window_handle::HasWindowHandle>,
    confirmation: &CloseConfirmation<'_>,
) -> bool {
    platform_confirm(owner, confirmation)
}

#[cfg(windows)]
fn platform_confirm(
    owner: Option<&dyn winit::raw_window_handle::HasWindowHandle>,
    confirmation: &CloseConfirmation<'_>,
) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TASKMODAL, MB_YESNO, MessageBoxW,
    };

    let owner = owner_hwnd(owner);
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let message = wide(&confirmation.message());
    let title = wide(confirmation.title);
    // SAFETY: `owner` is null or a live window on this thread, and both UTF-16 buffers stay alive
    // and NUL-terminated for the modal call. Button two (No) is the default, so Enter keeps the
    // program running.
    unsafe {
        MessageBoxW(
            owner,
            message.as_ptr(),
            title.as_ptr(),
            MB_ICONWARNING | MB_YESNO | MB_DEFBUTTON2 | MB_SETFOREGROUND | MB_TASKMODAL,
        ) == IDYES
    }
}

/// The HWND a Win32 dialog should belong to, or null to make it modal to the application.
#[cfg(windows)]
pub(crate) fn owner_hwnd(
    owner: Option<&dyn winit::raw_window_handle::HasWindowHandle>,
) -> *mut std::ffi::c_void {
    use winit::raw_window_handle::RawWindowHandle;

    owner
        .and_then(|owner| owner.window_handle().ok())
        .and_then(|handle| match handle.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as *mut std::ffi::c_void),
            _ => None,
        })
        .unwrap_or(std::ptr::null_mut())
}

#[cfg(target_os = "macos")]
fn platform_confirm(
    _owner: Option<&dyn winit::raw_window_handle::HasWindowHandle>,
    confirmation: &CloseConfirmation<'_>,
) -> bool {
    use objc2_app_kit::{NSAlert, NSAlertSecondButtonReturn, NSAlertStyle};
    use objc2_foundation::{MainThreadMarker, NSString};

    let Some(mtm) = MainThreadMarker::new() else { return false };
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(confirmation.title));
    alert.setInformativeText(&NSString::from_str(&confirmation.message()));
    // Cancel comes first so Return keeps the program running.
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.addButtonWithTitle(&NSString::from_str(confirmation.action));
    alert.runModal() == NSAlertSecondButtonReturn
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_confirm(
    _owner: Option<&dyn winit::raw_window_handle::HasWindowHandle>,
    _confirmation: &CloseConfirmation<'_>,
) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(subject: &str, programs: &[&str]) -> String {
        let programs = programs.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>();
        CloseConfirmation { title: "Close?", subject, action: "Close", programs: &programs }
            .message()
    }

    #[test]
    fn one_program_is_named() {
        assert_eq!(
            message("this terminal", &["vim"]),
            "vim is still running in this terminal. Closing will stop it."
        );
    }

    #[test]
    fn several_programs_are_counted_listed_once_and_truncated() {
        assert_eq!(
            message("this tab", &["cargo", "cargo"]),
            "2 programs are still running in this tab: cargo. Closing will stop them."
        );
        assert_eq!(
            message("Vivida", &["vim", "cargo", "python", "node", "ssh", "top"]),
            "6 programs are still running in Vivida: vim, cargo, python, node, and 2 more. \
             Closing will stop them."
        );
    }
}
