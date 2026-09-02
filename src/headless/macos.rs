use std::error::Error;
use std::ffi::c_int;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cli::Options;
use crate::session::{SessionPaths, validate_session_name};

use super::{
    MAX_DIAGNOSTIC_BYTES, READINESS_TIMEOUT, Readiness, parse_readiness_report, scrub_environment,
    serve,
};

/// Re-enter the daemon after [`spawn_detached`] starts a fresh executable image.
///
/// Metal cannot be initialized reliably in a process that has returned from `fork`, even when the
/// parent was single-threaded. Re-executing before wgpu starts gives the daemon a clean process.
pub fn run_reexec(
    mut options: Options,
    session: String,
    readiness_handle: usize,
) -> Result<(), Box<dyn Error>> {
    validate_session_name(&session)?;
    let paths = SessionPaths::for_session(&session)?;
    options.session = Some(session.clone());
    options.socket = Some(paths.socket.clone());

    let raw_readiness = c_int::try_from(readiness_handle).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "internal readiness descriptor is invalid")
    })?;
    if unsafe { libc::fcntl(raw_readiness, libc::F_GETFD) } < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "internal headless readiness descriptor is invalid",
        )
        .into());
    }
    // SAFETY: the parent deliberately left this descriptor open across exec and transferred sole
    // child ownership through the hidden option after validating that it fits in `c_int`.
    let readiness = unsafe { File::from_raw_fd(raw_readiness) };
    set_close_on_exec(&readiness, true)?;

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
    let (read, write) = readiness_pipe()
        .map_err(|error| io::Error::new(error.kind(), format!("readiness pipe: {error}")))?;
    let readiness_handle = write.as_raw_fd() as usize;
    let mut command = Command::new(std::env::current_exe()?);
    let arguments = crate::cli::headless_reexec_args(
        std::env::args_os().skip(1).collect(),
        readiness_handle,
        &session,
    );
    command.args(arguments).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and this is the only operation performed between the
    // `Command` implementation's fork and exec. All Rust and Metal initialization happens after
    // the fresh executable image starts.
    unsafe {
        command.pre_exec(
            || {
                if libc::setsid() < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
            },
        );
    }
    let child = command
        .spawn()
        .map_err(|error| io::Error::new(error.kind(), format!("headless re-exec: {error}")))?;
    drop(write);

    let paths = SessionPaths::for_session(&session)
        .map_err(|error| io::Error::new(error.kind(), format!("session paths: {error}")))?;
    await_readiness(read, child.id(), &session, &paths)
}

/// Create a readiness pipe whose write end alone crosses the re-exec.
pub(super) fn readiness_pipe() -> io::Result<(File, File)> {
    let mut descriptors = [0 as c_int; 2];
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe` returned two fresh descriptors and ownership transfers to these files.
    let read = unsafe { File::from_raw_fd(descriptors[0]) };
    let write = unsafe { File::from_raw_fd(descriptors[1]) };
    set_close_on_exec(&read, true)?;
    set_close_on_exec(&write, false)?;
    Ok((read, write))
}

fn set_close_on_exec(file: &File, close_on_exec: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if close_on_exec { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC };
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn await_readiness(
    mut read: File,
    child: u32,
    session: &str,
    paths: &SessionPaths,
) -> Result<(), Box<dyn Error>> {
    set_read_timeout(&read, READINESS_TIMEOUT)?;

    let mut report = String::new();
    let outcome =
        Read::by_ref(&mut read).take(MAX_DIAGNOSTIC_BYTES + 64).read_to_string(&mut report);
    match outcome {
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
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
        Err(error) => return Err(error.into()),
    }

    parse_readiness_report(&report, session, paths)
}

pub(super) fn set_read_timeout(file: &File, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut poll_fd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let timeout_millis = remaining.as_millis().min(c_int::MAX as u128) as c_int;
        let result = unsafe { libc::poll(&raw mut poll_fd, 1, timeout_millis) };
        match result {
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            },
            0 => return Err(io::Error::from(io::ErrorKind::WouldBlock)),
            _ => return Ok(()),
        }
    }
}
