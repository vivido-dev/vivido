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

use base64::Engine;
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
#[cfg(not(any(target_os = "macos", windows)))]
use vivido::cli::SocketMessage as ActivationSocketMessage;
use vivido::cli::Subcommands;
use vivido::cli::{
    DebugBundleOptions, DoctorOptions, IpcDiagnose, IpcGetGrid, IpcScreenshot, IpcTarget,
    IpcTranscript, ListOptions, MessageOptions, Options, SocketMessage,
};
use vivido::config;
use vivido::config::UiConfig;
#[cfg(any(target_os = "linux", windows))]
use vivido::config::window::Decorations;
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
        Some(Subcommands::List(options)) => list_instances(options)?,
        Some(Subcommands::Doctor(options)) => doctor(options)?,
        Some(Subcommands::DebugBundle(options)) => write_debug_bundle(options)?,
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
    if let ActivationSocketMessage::CreateWindow(window_options) = &mut options.message {
        window_options.activation_token =
            env::var("XDG_ACTIVATION_TOKEN").or_else(|_| env::var("DESKTOP_STARTUP_ID")).ok();
    }
    ipc::send_message(options).map_err(|err| err.into())
}

fn list_instances(options: ListOptions) -> Result<(), Box<dyn Error>> {
    if !options.json && !options.all {
        session::print_sessions()?;
        return Ok(());
    }
    let instances = session::list_registries()?
        .into_iter()
        .filter(|instance| options.all || instance.headless)
        .collect::<Vec<_>>();
    if options.json {
        serde_json::to_writer(
            io::stdout().lock(),
            &serde_json::json!({"schema_version": 1, "instances": instances}),
        )?;
        writeln!(io::stdout())?;
    } else {
        for instance in instances {
            println!(
                "{}\tpid {}\t{}x{}\t{}",
                instance.name,
                instance.pid,
                instance.columns,
                instance.lines,
                instance.socket.display()
            );
        }
    }
    Ok(())
}

fn doctor(options: DoctorOptions) -> Result<(), Box<dyn Error>> {
    let message = SocketMessage::Diagnose(IpcDiagnose { window_id: None, trace_limit: 128 });
    let (capabilities, diagnose) = ipc::request_once(None, Some(&options.target), &message)?;
    let frame_ready = diagnose["renderer"]["has_presented_frame"].as_bool().unwrap_or(false);
    let track_count = diagnose["presenter"]["tracks"].as_array().map_or(0, Vec::len);
    let status = if frame_ready { "ok" } else { "warning" };
    let report = serde_json::json!({
        "schema_version": 1,
        "status": status,
        "target": options.target,
        "checks": {
            "registry_and_ipc": "ok",
            "renderer_frame_ready": frame_ready,
            "presenter_track_count": track_count,
        },
        "capabilities": capabilities,
        "diagnose": diagnose,
    });
    if options.json {
        serde_json::to_writer(io::stdout().lock(), &report)?;
        writeln!(io::stdout())?;
    }
    Ok(())
}

fn write_debug_bundle(options: DebugBundleOptions) -> Result<(), Box<dyn Error>> {
    let message = SocketMessage::Diagnose(IpcDiagnose { window_id: None, trace_limit: 512 });
    let (capabilities, diagnose) = ipc::request_once(None, Some(&options.target), &message)?;
    let manifest = serde_json::json!({
        "schema_version": 1,
        "product": "vivido",
        "target": options.target,
        "metadata_only_default": true,
        "included": {
            "screenshot": options.include_screenshot,
            "grid": options.include_grid,
            "transcript": options.include_transcript,
            "log": options.include_log,
        },
    });
    let mut entries = vec![
        ("manifest.json".to_owned(), serde_json::to_vec_pretty(&manifest)?),
        ("capabilities.json".to_owned(), serde_json::to_vec_pretty(&capabilities)?),
        ("diagnose.json".to_owned(), serde_json::to_vec_pretty(&diagnose)?),
    ];
    if options.include_screenshot {
        let (_, result) = ipc::request_once(
            None,
            Some(&options.target),
            &SocketMessage::Screenshot(IpcScreenshot::default()),
        )?;
        let path = result["path"].as_str().ok_or("screenshot reply is missing its path")?;
        let bytes = fs::read(path)?;
        entries.push(("content/screenshot.png".to_owned(), bytes));
        let _ = fs::remove_file(path);
    }
    if options.include_grid {
        let (_, grid) = ipc::request_once(
            None,
            Some(&options.target),
            &SocketMessage::GetGrid(IpcGetGrid::default()),
        )?;
        entries.push(("content/grid.json".to_owned(), serde_json::to_vec_pretty(&grid)?));
    }
    if options.include_transcript {
        let (_, transcript) = ipc::request_once(
            None,
            Some(&options.target),
            &SocketMessage::Transcript(IpcTranscript {
                after_offset: None,
                max_bytes: 1024 * 1024,
                raw: false,
                target: IpcTarget::default(),
            }),
        )?;
        let encoded =
            transcript["data"].as_str().ok_or("transcript reply is missing bounded data")?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        entries.push(("content/transcript.bin".to_owned(), bytes));
    }
    if options.include_log {
        let registry = session::list_registries()?
            .into_iter()
            .find(|instance| instance.name == options.target)
            .ok_or("registered Vivido instance disappeared while collecting its bundle")?;
        let path = env::temp_dir().join(format!("Vivido-{}.log", registry.pid));
        if let Ok(bytes) = fs::read(path) {
            let start = bytes.len().saturating_sub(1024 * 1024);
            entries.push(("logs/vivido.log".to_owned(), bytes[start..].to_vec()));
        }
    }
    write_stored_zip(&options.output, &entries)?;
    println!("{}", options.output.display());
    Ok(())
}

fn write_stored_zip(path: &std::path::Path, entries: &[(String, Vec<u8>)]) -> io::Result<()> {
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("diagnostic bundle already exists: {}", path.display()),
        ));
    }
    let temporary = path.with_extension(format!("zip.{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary)?;
    let result = (|| {
        let mut central = Vec::new();
        let mut offset = 0_u32;
        for (name, bytes) in entries {
            let name = name.as_bytes();
            let name_length = u16::try_from(name.len())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP name is too long"))?;
            let size = u32::try_from(bytes.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "ZIP entry exceeds 4 GiB")
            })?;
            let crc = crc32(bytes);
            let mut local = Vec::with_capacity(30 + name.len());
            local.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
            local.extend_from_slice(&20_u16.to_le_bytes());
            local.extend_from_slice(&0_u16.to_le_bytes());
            local.extend_from_slice(&0_u16.to_le_bytes());
            local.extend_from_slice(&0_u16.to_le_bytes());
            local.extend_from_slice(&0_u16.to_le_bytes());
            local.extend_from_slice(&crc.to_le_bytes());
            local.extend_from_slice(&size.to_le_bytes());
            local.extend_from_slice(&size.to_le_bytes());
            local.extend_from_slice(&name_length.to_le_bytes());
            local.extend_from_slice(&0_u16.to_le_bytes());
            local.extend_from_slice(name);
            file.write_all(&local)?;
            file.write_all(bytes)?;

            central.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes());
            central.extend_from_slice(&20_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&name_length.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u16.to_le_bytes());
            central.extend_from_slice(&0_u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name);
            offset = offset
                .checked_add(u32::try_from(local.len() + bytes.len()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "ZIP archive exceeds 4 GiB")
                })?)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "ZIP archive exceeds 4 GiB")
                })?;
        }
        let central_offset = offset;
        file.write_all(&central)?;
        let central_size = u32::try_from(central.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP index is too large"))?;
        let count = u16::try_from(entries.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many ZIP entries"))?;
        file.write_all(&0x0605_4b50_u32.to_le_bytes())?;
        file.write_all(&0_u16.to_le_bytes())?;
        file.write_all(&0_u16.to_le_bytes())?;
        file.write_all(&count.to_le_bytes())?;
        file.write_all(&count.to_le_bytes())?;
        file.write_all(&central_size.to_le_bytes())?;
        file.write_all(&central_offset.to_le_bytes())?;
        file.write_all(&0_u16.to_le_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
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
    // Every headed process gets a stable same-user rendezvous.  Explicit names make automation
    // scripts readable; the PID-derived default remains collision-free without configuration.
    let automation_name =
        options.automation_name.clone().unwrap_or_else(|| format!("vivido-{}", std::process::id()));
    let automation_socket = options
        .socket
        .clone()
        .unwrap_or(session::SessionPaths::for_session(&automation_name)?.socket);
    let automation_paths =
        session::SessionPaths::for_endpoint(&automation_name, automation_socket.clone())?;
    automation_paths.prepare_endpoint(&automation_name)?;
    options.automation_name = Some(automation_name.clone());
    options.socket = Some(automation_socket);

    // Setup winit event loop.
    #[cfg(not(target_os = "macos"))]
    let window_event_loop = {
        let mut builder = EventLoop::<Event>::with_user_event();
        #[cfg(target_os = "linux")]
        {
            use winit::platform::wayland::EventLoopBuilderExtWayland;
            builder.with_wayland();
        }
        builder.build()?
    };

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
    let mut config = config::load(&mut options);
    // Discovery is a process-level contract, so a user config cannot silently turn off the IPC
    // endpoint required to address a headed instance by name.
    config.ipc_socket = Some(true);
    log_config_path(&config);

    // Update the log level from config.
    log::set_max_level(config.debug.log_level);

    #[cfg(any(target_os = "linux", windows))]
    let chrome_config = config.clone();
    #[cfg(any(target_os = "linux", windows))]
    {
        // Terminal panes are hosted inside the shell's integrated chrome.
        config.window.decorations = Decorations::None;
        config.window.resize_increments = false;
    }

    // Set tty environment variables.
    let _terminfo_guard = tty::setup_env();

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
    let ipc_endpoint =
        IoListener::spawn(&config, &options, EventSink::Winit(window_event_loop.create_proxy()))?
            .ipc_socket_path;

    // Setup automatic RAII cleanup for our files.
    let log_cleanup = log_file.filter(|_| !config.debug.persistent_logging);
    let _files = TemporaryFiles {
        #[cfg(unix)]
        socket_path: ipc_endpoint,
        log_file: log_cleanup,
    };
    #[cfg(windows)]
    let _ = ipc_endpoint;

    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let registry = automation_paths.write_registry(&automation_name, &nonce, (1, 1), false)?;
    let _registry_guard = session::RegistryGuard::new(automation_paths, registry);

    // Event processor.
    #[allow(unused_mut)]
    let mut processor = Processor::new(config, options, &window_event_loop);

    // Start event loop and block until shutdown.
    #[cfg(any(target_os = "linux", windows))]
    let (mut processor, result) =
        vivido::shell::TabbedApplication::new(processor, chrome_config).run(window_event_loop);
    #[cfg(target_os = "macos")]
    let result = processor.run(window_event_loop);

    // `Processor` must be dropped before calling `FreeConsole` so the window contexts and their
    // PTY event-loop senders are gone first. The PTY itself is owned by its I/O thread; the
    // ConPTY-versus-conout-pipe drop order is enforced structurally by `ConptyBackend`, so no
    // drop-order requirement is left on this function.

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
