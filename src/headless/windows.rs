use std::error::Error;
use std::fs::File;
use std::io::{self, Read};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use crate::cli::Options;
use crate::session::{SessionPaths, validate_session_name};

use super::{
    MAX_DIAGNOSTIC_BYTES, READINESS_TIMEOUT, Readiness, parse_readiness_report, scrub_environment,
    serve,
};

/// Re-enter the daemon after `spawn_detached` re-execs this executable.
pub fn run_reexec(
    mut options: Options,
    session: String,
    readiness_handle: usize,
) -> Result<(), Box<dyn Error>> {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };

    validate_session_name(&session)?;
    let paths = SessionPaths::for_session(&session)?;
    options.session = Some(session.clone());
    options.socket = Some(paths.socket.clone());
    let raw_readiness = readiness_handle as HANDLE;
    let mut handle_flags = 0;
    if raw_readiness.is_null()
        || unsafe { GetHandleInformation(raw_readiness, &mut handle_flags) } == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "internal headless readiness handle is invalid",
        )
        .into());
    }
    // SAFETY: the parent explicitly made this pipe handle inheritable and transferred sole child
    // ownership by numeric value in the hidden option; GetHandleInformation validated it above.
    let readiness = unsafe { File::from_raw_handle(raw_readiness as RawHandle) };
    // The handle had to cross this re-exec, but the shell must never inherit it. Otherwise the
    // parent cannot observe EOF after readiness and waits for the shell to exit.
    if unsafe { SetHandleInformation(readiness.as_raw_handle() as HANDLE, HANDLE_FLAG_INHERIT, 0) }
        == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    scrub_environment();
    let readiness = Readiness::new(readiness);
    match serve(options, session, paths, Some(&readiness)) {
        Ok(()) => Ok(()),
        Err(error) => {
            readiness.failure(&error.to_string());
            Err(error)
        },
    }
}

pub(super) fn spawn_detached(
    _options: Options,
    session: String,
    _paths: SessionPaths,
) -> Result<(), Box<dyn Error>> {
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    let (read, write) = readiness_pipe()
        .map_err(|error| io::Error::new(error.kind(), format!("readiness pipe: {error}")))?;
    make_standard_handles_non_inheritable()?;
    let readiness_handle = write.as_raw_handle() as usize;
    let mut command = Command::new(std::env::current_exe()?);
    let arguments = crate::cli::headless_reexec_args(
        std::env::args_os().skip(1).collect(),
        readiness_handle,
        &session,
    );
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    let child = command
        .spawn()
        .map_err(|error| io::Error::new(error.kind(), format!("headless re-exec: {error}")))?;
    drop(write);

    let paths = SessionPaths::for_session(&session)
        .map_err(|error| io::Error::new(error.kind(), format!("session paths: {error}")))?;
    await_readiness(read, child.id(), &session, &paths)
}

fn make_standard_handles_non_inheritable() -> io::Result<()> {
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let handle = unsafe { GetStdHandle(kind) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(super) fn readiness_pipe() -> io::Result<(File, File)> {
    use std::ptr;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Pipes::CreatePipe;

    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 8 * 1024) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Only the write end crosses re-exec. If the child inherited the read end, the readiness
    // protocol could not reliably observe writer closure after an early startup failure.
    if unsafe { SetHandleInformation(read, HANDLE_FLAG_INHERIT, 0) } == 0 {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(read);
            windows_sys::Win32::Foundation::CloseHandle(write);
        }
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned two fresh handles and ownership transfers to File.
    Ok(unsafe {
        (File::from_raw_handle(read as RawHandle), File::from_raw_handle(write as RawHandle))
    })
}

fn await_readiness(
    mut read: File,
    child: u32,
    session: &str,
    paths: &SessionPaths,
) -> Result<(), Box<dyn Error>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut report = String::new();
        let result = Read::by_ref(&mut read)
            .take(MAX_DIAGNOSTIC_BYTES + 64)
            .read_to_string(&mut report)
            .map(|_| report);
        let _ = sender.send(result);
    });

    let report = match receiver.recv_timeout(READINESS_TIMEOUT) {
        Ok(result) => result?,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "vivido session {session:?} did not report readiness within {}s; it is still \
                     running as pid {child}. Check it with `vivido list`.",
                    READINESS_TIMEOUT.as_secs()
                ),
            )
            .into());
        },
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return Err(io::Error::other("readiness reader stopped unexpectedly").into());
        },
    };
    parse_readiness_report(&report, session, paths)
}
