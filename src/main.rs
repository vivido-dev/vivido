//! Vivido - The GPU Enhanced Terminal.

#![warn(rust_2018_idioms, future_incompatible)]
#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]
#![cfg_attr(clippy, deny(warnings))]
// With the default subsystem, 'console', windows creates an additional console
// window for the program.
// This is silently ignored on non-windows systems.
// See https://msdn.microsoft.com/en-us/library/4cc7ya5b.aspx for more details.
#![windows_subsystem = "windows"]

use std::error::Error;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::{env, fs};

use log::info;
#[cfg(windows)]
use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, FreeConsole};
use winit::event_loop::EventLoop;

#[cfg(target_os = "macos")]
use vivido::binary::macos::{self, locale};
#[cfg(windows)]
use vivido::binary::panic;
use vivido::binary::polling::{IoListener, ipc};
use vivido::binary::{headless, logging, session};
use vivido::cli::MessageOptions;
use vivido::cli::Options;
#[cfg(not(any(target_os = "macos", windows)))]
use vivido::cli::SocketMessage;
use vivido::cli::Subcommands;
use vivido::config;
use vivido::config::UiConfig;
use vivido::event::{Event, EventSink, Processor};
use vivido::terminal::tty;

fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    panic::attach_handler();

    // When linked with the windows subsystem windows won't automatically attach
    // to the console of the parent process, so we do it explicitly. This fails
    // silently if the parent has no console.
    #[cfg(windows)]
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }

    // Load command line options.
    let mut options = Options::new();

    #[cfg(any(target_os = "macos", windows))]
    if let Some(readiness_handle) = options.headless_server_handle {
        let resolved_session = options
            .resolved_session
            .take()
            .ok_or("internal headless server is missing its resolved session")?;
        return headless::run_reexec(options, resolved_session, readiness_handle);
    }

    match options.subcommands.take() {
        Some(Subcommands::Msg(options)) => msg(options)?,
        Some(Subcommands::List) => session::print_sessions()?,
        Some(Subcommands::KillSession { target }) => session::terminate_session(&target)?,
        // A headless run never builds a winit event loop, so it must branch before `vivido`.
        None if options.headless => headless::run(options)?,
        None => vivido(options)?,
    }

    Ok(())
}

/// `msg` subcommand entrypoint.
#[allow(unused_mut)]
fn msg(mut options: MessageOptions) -> Result<(), Box<dyn Error>> {
    #[cfg(not(any(target_os = "macos", windows)))]
    if let SocketMessage::CreateWindow(window_options) = &mut options.message {
        window_options.activation_token =
            env::var("XDG_ACTIVATION_TOKEN").or_else(|_| env::var("DESKTOP_STARTUP_ID")).ok();
    }
    ipc::send_message(options).map_err(|err| err.into())
}

/// Temporary files stored for Vivido.
///
/// This stores temporary files to automate their destruction through its `Drop` implementation.
struct TemporaryFiles {
    #[cfg(unix)]
    socket_path: Option<PathBuf>,
    log_file: Option<PathBuf>,
}

impl Drop for TemporaryFiles {
    fn drop(&mut self) {
        // Clean up the IPC socket file.
        #[cfg(unix)]
        if let Some(socket_path) = self.socket_path.as_deref() {
            let _ = fs::remove_file(socket_path);
        }

        // Clean up logfile.
        if let Some(log_file) = &self.log_file
            && fs::remove_file(log_file).is_ok()
        {
            let _ = writeln!(io::stdout(), "Deleted log file at \"{}\"", log_file.display());
        }
    }
}

/// Run main Vivido entrypoint.
///
/// Creates a window, the terminal state, PTY, I/O event loop, input processor,
/// config change monitor, and runs the main display loop.
fn vivido(mut options: Options) -> Result<(), Box<dyn Error>> {
    // Setup winit event loop.
    #[cfg(not(target_os = "macos"))]
    let window_event_loop = EventLoop::<Event>::with_user_event().build()?;

    // An accessory instance exists to serve another application's windows, so it takes neither a
    // Dock icon nor the activation the frontmost application currently holds.
    #[cfg(target_os = "macos")]
    let window_event_loop = {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

        let mut builder = EventLoop::<Event>::with_user_event();
        if options.accessory {
            builder
                .with_activation_policy(ActivationPolicy::Accessory)
                .with_activate_ignoring_other_apps(false);
        }
        builder.build()?
    };

    // Initialize the logger as soon as possible as to capture output from other subsystems.
    let log_file =
        logging::initialize(&options, EventSink::Winit(window_event_loop.create_proxy()))
            .expect("Unable to initialize logger");

    info!("Welcome to Vivido");
    info!("Version {}", env!("VERSION"));

    #[cfg(not(any(target_os = "macos", windows)))]
    info!("Running on Wayland");

    // Load configuration file.
    let config = config::load(&mut options);
    log_config_path(&config);

    // Update the log level from config.
    log::set_max_level(config.debug.log_level);

    // Set tty environment variables.
    tty::setup_env();

    // Set env vars from config.
    for (key, value) in config.env.iter() {
        unsafe { env::set_var(key, value) };
    }

    // Switch to home directory.
    #[cfg(target_os = "macos")]
    env::set_current_dir(home::home_dir().unwrap()).unwrap();

    // Set macOS locale.
    #[cfg(target_os = "macos")]
    locale::set_locale_environment();

    #[cfg(target_os = "macos")]
    macos::disable_autofill();

    // Spawn the Unix I/O event polling thread.
    #[cfg(any(unix, windows))]
    let ipc_endpoint = match IoListener::spawn(
        &config,
        &options,
        EventSink::Winit(window_event_loop.create_proxy()),
    ) {
        Ok(handle) => handle.ipc_socket_path,
        Err(err) if options.daemon => return Err(err.into()),
        Err(err) => {
            log::warn!("Unable to create socket: {err:?}");
            None
        },
    };

    // Setup automatic RAII cleanup for our files.
    let log_cleanup = log_file.filter(|_| !config.debug.persistent_logging);
    let _files = TemporaryFiles {
        #[cfg(unix)]
        socket_path: ipc_endpoint,
        log_file: log_cleanup,
    };
    #[cfg(windows)]
    let _ = ipc_endpoint;

    // Event processor.
    let mut processor = Processor::new(config, options, &window_event_loop);

    // Start event loop and block until shutdown.
    let result = processor.run(window_event_loop);

    // `Processor` must be dropped before calling `FreeConsole`.
    //
    // This is needed for ConPTY backend. Otherwise a deadlock can occur.
    // The cause:
    //   - Drop for ConPTY will deadlock if the conout pipe has already been dropped
    //   - ConPTY is dropped when the last of processor and window context are dropped, because both
    //     of them own an Arc<ConPTY>
    //
    // The fix is to ensure that processor is dropped first. That way, when window context (i.e.
    // PTY) is dropped, it can ensure ConPTY is dropped before the conout pipe in the PTY drop
    // order.
    //
    // FIXME: Change PTY API to enforce the correct drop order with the typesystem.

    // Terminate the config monitor.
    if let Some(config_monitor) = processor.config_monitor.take() {
        config_monitor.shutdown();
    }

    // Without explicitly detaching the console cmd won't redraw it's prompt.
    #[cfg(windows)]
    unsafe {
        FreeConsole();
    }

    info!("Goodbye");

    result
}

fn log_config_path(config: &UiConfig) {
    if config.config_paths.is_empty() {
        return;
    }

    let mut msg = String::from("Configuration files loaded from:");
    for path in &config.config_paths {
        let _ = write!(msg, "\n  {:?}", path.display());
    }

    info!("{msg}");
}
