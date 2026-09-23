use std::io::Write;
use std::{io, panic, ptr};

use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TASKMODAL, MessageBoxW,
};

use crate::terminal::tty::windows::win32_string;

// Install a panic handler that renders the panic in a classical Windows error
// dialog box as well as writes the panic to STDERR.
pub fn attach_handler() {
    panic::set_hook(Box::new(|panic_info| {
        // Worker boundaries handling untrusted terminal, Vivid, and IPC input catch their own
        // panics and retire only the offending client. A process-wide modal dialog here would make
        // that successful containment look like a Vivido crash.
        if crate::client_fault::is_contained() {
            let _ = writeln!(io::stderr(), "Vivido contained a client worker fault");
            return;
        }
        let _ = writeln!(io::stderr(), "{}", panic_info);
        let msg = format!("{}\n\nPress Ctrl-C to Copy", panic_info);
        unsafe {
            MessageBoxW(
                ptr::null_mut(),
                win32_string(&msg).as_ptr(),
                win32_string("Vivido: Runtime Error").as_ptr(),
                MB_ICONERROR | MB_OK | MB_SETFOREGROUND | MB_TASKMODAL,
            );
        }
    }));
}
