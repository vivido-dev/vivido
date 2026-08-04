use std::error::Error;
use std::ffi::c_int;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::{Duration, Instant};

use crate::cli::Options;
use crate::session::SessionPaths;

use super::{
    MAX_DIAGNOSTIC_BYTES, READINESS_TIMEOUT, Readiness, parse_readiness_report, report_failure,
    scrub_environment, serve,
};

pub(super) fn spawn_detached(
    options: Options,
    session: String,
    paths: SessionPaths,
) -> Result<(), Box<dyn Error>> {
    let (read, write) = readiness_pipe()?;
    // SAFETY: This process is still single-threaded — no logger, listener, or PTY thread has been
    // started yet — so the child may safely run Rust code between fork and its own setup.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error().into()),
        0 => {
            drop(read);
            let code = match detach_and_serve(options, session, paths, write) {
                Ok(()) => 0,
                Err(_) => 1,
            };
            // The child must never unwind back into the parent's call stack.
            std::process::exit(code);
        },
        child => {
            drop(write);
            await_readiness(read, child, &session, &paths)
        },
    }
}

/// Child side of the fork: detach from the terminal, then serve.
fn detach_and_serve(
    options: Options,
    session: String,
    paths: SessionPaths,
    readiness: File,
) -> Result<(), Box<dyn Error>> {
    // Leave the parent's session and controlling terminal so the daemon survives its shell.
    if unsafe { libc::setsid() } < 0 {
        let error = io::Error::last_os_error();
        let _ = report_failure(&readiness, &format!("setsid failed: {error}"));
        return Err(error.into());
    }

    redirect_standard_streams()?;
    scrub_environment();

    let readiness = Readiness::new(readiness);
    match serve(options, session, paths, Some(&readiness)) {
        Ok(()) => Ok(()),
        Err(error) => {
            // If the failure happened before readiness was reported, the parent is still waiting.
            readiness.failure(&error.to_string());
            Err(error)
        },
    }
}

/// Create a close-on-exec readiness pipe.
///
/// The child eventually execs a shell, so both descriptors must be close-on-exec. `pipe2` is not
/// available on every supported Unix (notably macOS), and this runs before any thread is started,
/// making portable `pipe` plus `fcntl` race-free here.
pub(super) fn readiness_pipe() -> io::Result<(File, File)> {
    let mut descriptors = [0 as c_int; 2];
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe` returned two fresh descriptors and ownership transfers to these files.
    let read = unsafe { File::from_raw_fd(descriptors[0]) };
    let write = unsafe { File::from_raw_fd(descriptors[1]) };
    set_close_on_exec(&read)?;
    set_close_on_exec(&write)?;
    Ok((read, write))
}

fn set_close_on_exec(file: &File) -> io::Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Block until the daemon reports readiness, then print its endpoint.
fn await_readiness(
    mut read: File,
    child: libc::pid_t,
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
            // The daemon is still running; killing it here could destroy a session that is merely
            // slow to build a software renderer.
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
    // A pipe does not honour SO_RCVTIMEO, so poll for readability instead.
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

/// Point the standard streams at `/dev/null` so the daemon holds no terminal.
fn redirect_standard_streams() -> io::Result<()> {
    let null = File::options().read(true).write(true).open("/dev/null")?;
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::dup2(null.as_raw_fd(), target) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
