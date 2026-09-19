//! Native update dialogs, always invoked from the graphical event-loop thread.

use semver::Version;

/// Result of the download confirmation dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DownloadChoice {
    Download,
    Skip,
    Cancel,
}

/// Ask whether to download, skip, or cancel an available update.
#[cfg(windows)]
pub(crate) fn choose_download(version: &Version) -> DownloadChoice {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDNO, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TASKMODAL, MB_YESNOCANCEL, MessageBoxW,
    };

    use crate::terminal::tty::windows::win32_string;

    let message = win32_string(&format!(
        "Vivido will quit to install {version}.\n\nYes: Download & Install\nNo: Skip This Version\nCancel: Keep using Vivido"
    ));
    let title = win32_string("Vivido Update");
    // SAFETY: both strings are live, NUL-terminated UTF-16 buffers; a null owner makes this an
    // application-modal dialog, matching the existing terminal recovery prompt.
    match unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_ICONQUESTION | MB_YESNOCANCEL | MB_SETFOREGROUND | MB_TASKMODAL,
        )
    } {
        IDYES => DownloadChoice::Download,
        IDNO => DownloadChoice::Skip,
        _ => DownloadChoice::Cancel,
    }
}

/// Ask whether to hand a verified installer to the platform installer.
#[cfg(windows)]
pub(crate) fn confirm_install(version: &Version) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDOK, MB_ICONQUESTION, MB_OKCANCEL, MB_SETFOREGROUND, MB_TASKMODAL, MessageBoxW,
    };

    use crate::terminal::tty::windows::win32_string;

    let message = win32_string(&format!(
        "Vivido {version} is ready to install. Open the installer and quit Vivido?"
    ));
    let title = win32_string("Vivido Update Ready");
    // SAFETY: both strings are live, NUL-terminated UTF-16 buffers for the duration of the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_ICONQUESTION | MB_OKCANCEL | MB_SETFOREGROUND | MB_TASKMODAL,
        ) == IDOK
    }
}

/// Present a user-requested update result.
#[cfg(windows)]
pub(crate) fn information(title: &str, message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TASKMODAL, MessageBoxW,
    };

    use crate::terminal::tty::windows::win32_string;

    let message = win32_string(message);
    let title = win32_string(title);
    // SAFETY: both strings are live, NUL-terminated UTF-16 buffers for the duration of the call.
    let _ = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_ICONINFORMATION | MB_OK | MB_SETFOREGROUND | MB_TASKMODAL,
        )
    };
}

#[cfg(target_os = "macos")]
fn macos_alert(
    title: &str,
    message: &str,
    buttons: &[&str],
) -> Option<objc2_app_kit::NSModalResponse> {
    use objc2_app_kit::{NSAlert, NSAlertStyle};
    use objc2_foundation::{MainThreadMarker, NSString};

    let mtm = MainThreadMarker::new()?;
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    for title in buttons {
        alert.addButtonWithTitle(&NSString::from_str(title));
    }
    Some(alert.runModal())
}

/// Ask whether to download, skip, or cancel an available update.
#[cfg(target_os = "macos")]
pub(crate) fn choose_download(version: &Version) -> DownloadChoice {
    use objc2_app_kit::{NSAlertFirstButtonReturn, NSAlertSecondButtonReturn};

    let message = format!("Vivido will quit to install {version}.");
    let Some(response) = macos_alert(
        "Vivido Update",
        &message,
        &["Download & Install", "Skip This Version", "Cancel"],
    ) else {
        return DownloadChoice::Cancel;
    };
    if response == NSAlertFirstButtonReturn {
        DownloadChoice::Download
    } else if response == NSAlertSecondButtonReturn {
        DownloadChoice::Skip
    } else {
        DownloadChoice::Cancel
    }
}

/// Ask whether to hand a verified installer to the platform installer.
#[cfg(target_os = "macos")]
pub(crate) fn confirm_install(version: &Version) -> bool {
    use objc2_app_kit::NSAlertFirstButtonReturn;

    let message = format!("Vivido {version} is ready to install.");
    macos_alert("Vivido Update Ready", &message, &["Open Installer", "Cancel"])
        == Some(NSAlertFirstButtonReturn)
}

/// Present a user-requested update result.
#[cfg(target_os = "macos")]
pub(crate) fn information(title: &str, message: &str) {
    let _ = macos_alert(title, message, &["OK"]);
}

/// Linux has no native update dialog because the suite does not ship a Linux installer.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn choose_download(_version: &Version) -> DownloadChoice {
    DownloadChoice::Cancel
}

/// Linux has no native update dialog because the suite does not ship a Linux installer.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn confirm_install(_version: &Version) -> bool {
    false
}
