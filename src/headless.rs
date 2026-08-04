//! Headless startup: detaching, readiness reporting, and the windowless main loop.
//!
//! The detach protocol follows `vvland/src/linux/serve.rs`: the parent blocks until the daemon
//! either reports readiness or reports why it failed, so `vivido --headless` never exits 0 leaving
//! a session that was never usable.
//!
//! Unlike vvland this forks rather than re-execing a hidden subcommand. vvland re-execs because it
//! must reconstruct a compositor configuration from flags; here the daemon needs the exact options
//! the parent already parsed, and forking before any thread is spawned keeps them without a
//! serialize/parse round trip that could silently drop one.

use std::error::Error;
#[cfg(unix)]
use std::ffi::c_int;
use std::fs::File;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;
use std::{env, process};

use log::info;
use winit::dpi::PhysicalSize;

use crate::cli::{HeadlessSize, Options};
use crate::event::{EventSink, HeadlessLoop, Processor};
use crate::polling::IoListener;
use crate::session::{RegistryGuard, SessionPaths, validate_session_name};
use crate::{config, logging, tty};

/// How long the parent waits for the daemon to report readiness.
///
/// Generous because the daemon must create a GPU device and, on a machine with no hardware
/// adapter, fall back to a software renderer that is slow to initialize.
const READINESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on a failure diagnostic, so a wedged daemon cannot make the parent read forever.
const MAX_DIAGNOSTIC_BYTES: u64 = 4 * 1024;

/// Render size when neither `--headless-size` nor `window.dimensions` says otherwise.
const DEFAULT_HEADLESS_SIZE: PhysicalSize<u32> = PhysicalSize::new(1280, 720);

/// Fallback scale factor: headless has no monitor to ask.
const HEADLESS_SCALE_FACTOR: f64 = 1.0;

/// Run Vivido with no window and no compositor.
///
/// Returns in the parent once the daemon is serving; the daemon itself only returns when its loop
/// ends. With `--foreground` there is no daemon and this blocks until shutdown.
pub fn run(mut options: Options) -> Result<(), Box<dyn Error>> {
    let session = match options.session.as_deref() {
        Some(session) => {
            validate_session_name(session)?;
            session.to_owned()
        },
        // Distinct by construction, so concurrent unnamed sessions never collide.
        None => format!("vivido-{}", process::id()),
    };
    // Record the resolved name, including a derived one, so the daemon reports it in `hello`.
    options.session = Some(session.clone());

    let paths = SessionPaths::for_session(&session)
        .map_err(|error| io::Error::new(error.kind(), format!("session paths: {error}")))?;
    paths
        .prepare_endpoint(&session)
        .map_err(|error| io::Error::new(error.kind(), format!("session endpoint: {error}")))?;

    // The IPC socket is the session's socket, so `msg --target` and `-s` name the same thing.
    options.socket = Some(paths.socket.clone());

    if options.foreground {
        return serve(options, session, paths, None);
    }

    #[cfg(windows)]
    return spawn_detached(options, session);

    #[cfg(unix)]
    {
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
                process::exit(code);
            },
            child => {
                drop(write);
                await_readiness(read, child, &session, &paths)
            },
        }
    }
}

/// Re-enter the Windows daemon after `spawn_detached` re-execs this executable.
#[cfg(windows)]
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

#[cfg(windows)]
fn spawn_detached(_options: Options, session: String) -> Result<(), Box<dyn Error>> {
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    let (read, write) = readiness_pipe()
        .map_err(|error| io::Error::new(error.kind(), format!("readiness pipe: {error}")))?;
    make_standard_handles_non_inheritable()?;
    let readiness_handle = write.as_raw_handle() as usize;
    let mut command = Command::new(env::current_exe()?);
    let arguments = crate::cli::headless_reexec_args(
        env::args_os().skip(1).collect(),
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
    await_readiness_windows(read, child.id(), &session, &paths)
}

#[cfg(windows)]
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

#[cfg(windows)]
fn readiness_pipe() -> io::Result<(ReadinessPipe, ReadinessPipe)> {
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

#[cfg(windows)]
fn await_readiness_windows(
    mut read: ReadinessPipe,
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

/// Child side of the fork: detach from the terminal, then serve.
#[cfg(unix)]
fn detach_and_serve(
    options: Options,
    session: String,
    paths: SessionPaths,
    readiness: OwnedFd,
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
        Err(err) => {
            // If the failure happened before readiness was reported, the parent is still waiting.
            readiness.failure(&err.to_string());
            Err(err)
        },
    }
}

/// Bring up the whole headless stack and run its loop until shutdown.
fn serve(
    mut options: Options,
    session: String,
    paths: SessionPaths,
    readiness: Option<&Readiness>,
) -> Result<(), Box<dyn Error>> {
    let (proxy, events) = EventSink::headless();

    let log_file =
        logging::initialize(&options, proxy.clone()).expect("Unable to initialize logger");

    info!("Welcome to Vivido");
    info!("Version {}", env!("VERSION"));
    info!("Running headless as session {session:?}");

    let pixel_size = apply_headless_size(&mut options);
    let config = config::load(&mut options);
    log::set_max_level(config.debug.log_level);

    tty::setup_env();
    for (key, value) in config.env.iter() {
        unsafe { env::set_var(key, value) };
    }

    // Bind the IPC socket before publishing the registry: a client that finds a registry must find
    // a socket it can actually connect to.
    let handle = IoListener::spawn(&config, &options, proxy.clone())?;
    #[cfg(windows)]
    let _ = &handle;

    let headless_loop = HeadlessLoop::new(pixel_size, HEADLESS_SCALE_FACTOR);
    let mut processor = Processor::new_headless(config, options, proxy);

    // Build the window before publishing, so the registry records the geometry the session
    // actually has rather than the geometry that was asked for.
    let (columns, lines) = processor.start_headless(&headless_loop)?;

    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let registry = paths.write_registry(&session, &nonce, (columns, lines))?;
    let _registry_guard = RegistryGuard::new(paths.clone(), registry);

    // The socket is bound, the window exists, and the registry is published: a client that
    // connects from here on will find a working session.
    let socket = paths.socket.display().to_string();
    match readiness {
        Some(readiness) => readiness.success(&socket, &session),
        None => print_endpoint(&socket, &session),
    }

    let result = processor.run_headless(&events, &headless_loop);

    drop(processor);
    #[cfg(unix)]
    if let Some(path) = handle.ipc_socket_path {
        let _ = std::fs::remove_file(path);
    }
    if let Some(log_file) = log_file {
        let _ = std::fs::remove_file(log_file);
    }
    result
}

/// Resolve `--headless-size` into the config, returning the initial render size.
///
/// Cells go through the ordinary `window.dimensions` config path so the pixel size is derived from
/// real font metrics, exactly as it would be for a window. Pixels bypass that and become the
/// window's size directly, which a headless window accepts verbatim because nothing can refuse it.
fn apply_headless_size(options: &mut Options) -> PhysicalSize<u32> {
    match options.headless_size {
        Some(HeadlessSize::Pixels { width, height }) => PhysicalSize::new(width, height),
        Some(HeadlessSize::Cells { columns, lines }) => {
            options.window_options.push_override(format!("window.dimensions.columns={columns}"));
            options.window_options.push_override(format!("window.dimensions.lines={lines}"));
            // `Options::new` parsed the overrides before these were added.
            options.config_options = options.window_options.config_overrides();
            DEFAULT_HEADLESS_SIZE
        },
        // A configured `window.dimensions` still wins: `Display::new` re-requests the size it
        // implies, and a headless window applies that verbatim. This is only the fallback.
        None => DEFAULT_HEADLESS_SIZE,
    }
}

/// Reports startup success or failure back to the waiting parent, exactly once.
struct Readiness {
    pipe: std::cell::RefCell<Option<ReadinessPipe>>,
}

impl Readiness {
    fn new(pipe: ReadinessPipe) -> Self {
        Self { pipe: std::cell::RefCell::new(Some(pipe)) }
    }

    fn success(&self, socket: &str, session: &str) {
        if let Some(pipe) = self.pipe.borrow_mut().take() {
            let _ = write_all(&pipe, format!("OK\n{socket}\n{session}\n").as_bytes());
        }
    }

    fn failure(&self, diagnostic: &str) {
        if let Some(pipe) = self.pipe.borrow_mut().take() {
            let _ = report_failure(&pipe, diagnostic);
        }
    }
}

fn report_failure(pipe: &ReadinessPipe, diagnostic: &str) -> io::Result<()> {
    let truncated: String = diagnostic
        .chars()
        .take(MAX_DIAGNOSTIC_BYTES as usize)
        .collect::<String>()
        .replace('\n', " ");
    write_all(pipe, format!("ERR\n{truncated}\n").as_bytes())
}

#[cfg(unix)]
type ReadinessPipe = OwnedFd;

#[cfg(windows)]
type ReadinessPipe = File;

#[cfg(unix)]
fn write_all(pipe: &ReadinessPipe, bytes: &[u8]) -> io::Result<()> {
    // SAFETY: `pipe` owns the descriptor and is not closed for the duration of the write.
    let mut file = unsafe { File::from_raw_fd(pipe.as_raw_fd()) };
    let result = file.write_all(bytes).and_then(|()| file.flush());
    // The `File` borrowed the descriptor; leak it so `OwnedFd` remains the sole owner.
    std::mem::forget(file);
    result
}

#[cfg(windows)]
fn write_all(mut pipe: &ReadinessPipe, bytes: &[u8]) -> io::Result<()> {
    pipe.write_all(bytes).and_then(|()| pipe.flush())
}

/// Block until the daemon reports readiness, then print its endpoint.
#[cfg(unix)]
fn await_readiness(
    read: OwnedFd,
    child: libc::pid_t,
    session: &str,
    paths: &SessionPaths,
) -> Result<(), Box<dyn Error>> {
    set_read_timeout(&read, READINESS_TIMEOUT)?;

    // SAFETY: `read` owns the descriptor and lives until this function returns.
    let mut file = unsafe { File::from_raw_fd(read.as_raw_fd()) };
    let mut report = String::new();
    let outcome =
        Read::by_ref(&mut file).take(MAX_DIAGNOSTIC_BYTES + 64).read_to_string(&mut report);
    std::mem::forget(file);

    match outcome {
        Ok(_) => (),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
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
        Err(err) => return Err(err.into()),
    }

    parse_readiness_report(&report, session, paths)
}

fn parse_readiness_report(
    report: &str,
    session: &str,
    paths: &SessionPaths,
) -> Result<(), Box<dyn Error>> {
    let mut lines = report.lines();
    match lines.next() {
        Some("OK") => {
            let socket = lines.next().unwrap_or_default();
            let session = lines.next().unwrap_or(session);
            print_endpoint(socket, session);
            Ok(())
        },
        Some("ERR") => {
            let diagnostic = lines.next().unwrap_or("no diagnostic was reported");
            Err(io::Error::other(format!(
                "vivido session {session:?} failed to start: {diagnostic}"
            ))
            .into())
        },
        // The pipe closed without a report: the daemon died before it could say why.
        _ => {
            let _ = paths.socket;
            Err(io::Error::other(format!(
                "vivido session {session:?} exited before reporting readiness"
            ))
            .into())
        },
    }
}

/// Print the endpoint in a form a shell can evaluate.
fn print_endpoint(socket: &str, session: &str) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "VIVIDO_SOCKET={socket}; export VIVIDO_SOCKET");
    let _ = writeln!(stdout, "VIVIDO_SESSION={session}; export VIVIDO_SESSION");
    let _ = stdout.flush();
}

/// Create the readiness pipe.
///
/// `O_CLOEXEC` is essential, not hygiene: the daemon goes on to exec a shell, and an inherited
/// write end would keep the pipe open forever, so the parent would never see EOF and would block
/// until its timeout even though the session came up fine.
#[cfg(unix)]
fn readiness_pipe() -> io::Result<(ReadinessPipe, ReadinessPipe)> {
    let mut fds = [0 as c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` filled both entries with new descriptors this process owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

#[cfg(unix)]
fn set_read_timeout(fd: &OwnedFd, timeout: Duration) -> io::Result<()> {
    let value = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    // A pipe does not honour SO_RCVTIMEO, so poll for readability instead.
    let deadline = Instant::now() + timeout;
    let _ = value;
    let mut poll_fd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let result = unsafe { libc::poll(&raw mut poll_fd, 1, remaining.as_millis() as c_int) };
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
#[cfg(unix)]
fn redirect_standard_streams() -> io::Result<()> {
    let null = File::options().read(true).write(true).open("/dev/null")?;
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::dup2(null.as_raw_fd(), target) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Drop environment a nested launch must not inherit.
///
/// A headless Vivido is frequently started from inside another Vivid session or a multiplexer;
/// inheriting those would make its child shell talk to the wrong producer.
fn scrub_environment() {
    for key in
        ["VIVID_ENDPOINT_CONTROL", "VIVID_ENDPOINT_BULK", "VIVID_ROOT_SECRET", "VIVID_REMOTE"]
    {
        unsafe { env::remove_var(key) };
    }
    for key in ["TMUX", "TMUX_PANE", "STY", "VIVIDO_SOCKET", "VIVIDO_SESSION"] {
        unsafe { env::remove_var(key) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagnostic_is_bounded_and_single_line() {
        let (read, write) = readiness_pipe().unwrap();
        let long = format!("line one\nline two{}", "x".repeat(8 * 1024));
        report_failure(&write, &long).unwrap();
        drop(write);

        #[cfg(unix)]
        let mut file = unsafe { File::from_raw_fd(read.as_raw_fd()) };
        #[cfg(windows)]
        let mut file = read;
        let mut report = String::new();
        file.read_to_string(&mut report).unwrap();
        #[cfg(unix)]
        std::mem::forget(file);

        let mut lines = report.lines();
        assert_eq!(lines.next(), Some("ERR"));
        let diagnostic = lines.next().expect("a diagnostic line");
        assert!(diagnostic.len() <= MAX_DIAGNOSTIC_BYTES as usize);
        assert!(diagnostic.starts_with("line one line two"), "newlines are folded: {diagnostic:?}");
        assert_eq!(lines.next(), None, "the diagnostic is exactly one line");
    }

    #[test]
    fn readiness_reports_success_once() {
        let (read, write) = readiness_pipe().unwrap();
        let readiness = Readiness::new(write);
        readiness.success("/run/vivido/session.sock", "build");
        // A later failure must not overwrite a success the parent may already have acted on.
        readiness.failure("too late");
        drop(readiness);

        #[cfg(unix)]
        let mut file = unsafe { File::from_raw_fd(read.as_raw_fd()) };
        #[cfg(windows)]
        let mut file = read;
        let mut report = String::new();
        file.read_to_string(&mut report).unwrap();
        #[cfg(unix)]
        std::mem::forget(file);

        assert_eq!(report, "OK\n/run/vivido/session.sock\nbuild\n");
    }

    #[test]
    #[cfg(unix)]
    fn waiting_on_a_silent_daemon_times_out_without_killing_it() {
        let (read, write) = readiness_pipe().unwrap();
        let error = set_read_timeout(&read, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(write);
    }
}
