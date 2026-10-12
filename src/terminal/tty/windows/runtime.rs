//! Load the pinned, app-local Microsoft console host.

// Rust guideline compliant 2026-10-07
use std::io;

use libloading::Library;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::core::HRESULT;

type Create = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
type Resize = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type Close = unsafe extern "system" fn(HPCON);

#[derive(Debug)]
pub(super) struct Runtime {
    pub create: Create,
    pub resize: Resize,
    pub close: Close,
    // Keep all exported function pointers live through the last pseudoconsole close.
    _library: Library,
}

impl Runtime {
    pub(super) fn load() -> io::Result<Self> {
        let executable = std::env::current_exe()?;
        let directory =
            executable.parent().ok_or_else(|| io::Error::other("executable has no directory"))?;
        let dll = directory.join("conpty.dll");
        if !directory.join("OpenConsole.exe").is_file() {
            return Err(io::Error::other(
                "bundled OpenConsole.exe is missing; keep it beside the executable",
            ));
        }
        // SAFETY: This absolute app-local path names the pinned Microsoft DLL staged
        // with its matching helper. No library is discovered through PATH.
        let library = unsafe { Library::new(&dll) }.map_err(|error| {
            io::Error::other(format!("cannot load bundled ConPTY at {}: {error}; keep conpty.dll and OpenConsole.exe beside the executable", dll.display()))
        })?;
        // SAFETY: These exports and signatures come from the pinned package's conpty.h.
        // The owned Library outlives the copied function pointers and every PTY handle.
        let runtime = unsafe {
            Self {
                create: *library
                    .get::<Create>(b"ConptyCreatePseudoConsole\0")
                    .map_err(io::Error::other)?,
                resize: *library
                    .get::<Resize>(b"ConptyResizePseudoConsole\0")
                    .map_err(io::Error::other)?,
                close: *library
                    .get::<Close>(b"ConptyClosePseudoConsole\0")
                    .map_err(io::Error::other)?,
                _library: library,
            }
        };
        Ok(runtime)
    }
}
