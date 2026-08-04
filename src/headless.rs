//! Headless startup: detaching, readiness reporting, and the windowless main loop.
//!
//! The detach protocol follows `vvland/src/linux/serve.rs`: the parent blocks until the daemon
//! either reports readiness or reports why it failed, so `vivido --headless` never exits 0 leaving
//! a session that was never usable.
//!
//! Unix forks before any thread is started so the daemon keeps the already-parsed options. Windows
//! re-execs with an inherited readiness handle because it has no equivalent fork operation. Those
//! platform launch mechanisms live in separate modules below this shared orchestration layer.

use std::error::Error;
use std::fs::File;
use std::io::{self, Write};
use std::time::Duration;
use std::{env, process};

use log::info;
use winit::dpi::PhysicalSize;

use crate::cli::{HeadlessSize, Options};
use crate::event::{EventSink, HeadlessLoop, Processor};
use crate::polling::IoListener;
use crate::session::{RegistryGuard, SessionPaths, validate_session_name};
use crate::{config, logging, tty};

#[cfg(unix)]
#[path = "headless/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "headless/windows.rs"]
mod platform;

#[cfg(windows)]
pub use platform::run_reexec;

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

    platform::spawn_detached(options, session, paths)
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
    pipe: std::cell::RefCell<Option<File>>,
}

impl Readiness {
    fn new(pipe: File) -> Self {
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

fn report_failure(pipe: &File, diagnostic: &str) -> io::Result<()> {
    let truncated: String = diagnostic
        .chars()
        .take(MAX_DIAGNOSTIC_BYTES as usize)
        .collect::<String>()
        .replace('\n', " ");
    write_all(pipe, format!("ERR\n{truncated}\n").as_bytes())
}

fn write_all(mut pipe: &File, bytes: &[u8]) -> io::Result<()> {
    pipe.write_all(bytes).and_then(|()| pipe.flush())
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
    use std::io::Read;

    #[test]
    fn a_diagnostic_is_bounded_and_single_line() {
        let (mut read, write) = platform::readiness_pipe().unwrap();
        let long = format!("line one\nline two{}", "x".repeat(8 * 1024));
        report_failure(&write, &long).unwrap();
        drop(write);

        let mut report = String::new();
        read.read_to_string(&mut report).unwrap();

        let mut lines = report.lines();
        assert_eq!(lines.next(), Some("ERR"));
        let diagnostic = lines.next().expect("a diagnostic line");
        assert!(diagnostic.len() <= MAX_DIAGNOSTIC_BYTES as usize);
        assert!(diagnostic.starts_with("line one line two"), "newlines are folded: {diagnostic:?}");
        assert_eq!(lines.next(), None, "the diagnostic is exactly one line");
    }

    #[test]
    fn readiness_reports_success_once() {
        let (mut read, write) = platform::readiness_pipe().unwrap();
        let readiness = Readiness::new(write);
        readiness.success("/run/vivido/session.sock", "build");
        // A later failure must not overwrite a success the parent may already have acted on.
        readiness.failure("too late");
        drop(readiness);

        let mut report = String::new();
        read.read_to_string(&mut report).unwrap();

        assert_eq!(report, "OK\n/run/vivido/session.sock\nbuild\n");
    }

    #[test]
    #[cfg(unix)]
    fn waiting_on_a_silent_daemon_times_out_without_killing_it() {
        let (read, write) = platform::readiness_pipe().unwrap();
        let error = platform::set_read_timeout(&read, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(write);
    }
}
