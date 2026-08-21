//! Vivido terminal emulator library.

#![warn(rust_2018_idioms, future_incompatible)]
#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]
#![cfg_attr(clippy, deny(warnings))]

#[cfg(all(not(feature = "wayland"), not(any(target_os = "macos", windows))))]
compile_error!(r#"the "wayland" feature must be enabled on Linux and other Unix desktops"#);

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
mod accessibility;
mod automation;
pub mod cli;
mod clipboard;
#[macro_use]
mod config_derive;
pub mod config;
mod daemon;
pub mod display;
pub mod event;
mod headless;
mod input;
mod logging;
#[cfg(target_os = "macos")]
mod macos;
mod message_bar;
mod osc_notification;
#[cfg(windows)]
mod panic;
mod polling;
mod scheduler;
mod screenshot;
mod serde_replace;
mod session;
#[cfg(any(target_os = "linux", windows))]
pub mod shell;
mod string;
pub mod terminal;
mod vivid;
pub mod window_context;

#[cfg(any(target_os = "macos", windows))]
pub use crate::cli::ParentWindowHandle;
pub use crate::cli::WindowOptions;
pub use crate::display::window::Window;
pub use crate::event::{Event, EventSink, EventType, LoopHandle, Processor};
pub use crate::serde_replace::SerdeReplace;
pub use crate::terminal::tty;
pub use crate::window_context::WindowContext;
pub use config::UiConfig;
pub use config::monitor::ConfigMonitor;

/// Surface an in-process host needs to run and extend Vivido's automation endpoint.
///
/// Embedding [`Processor`] gives a host windows and terminals but no automation service: the
/// listener is started by the caller. Spawning [`IoListener`] offers the documented IPC endpoint on
/// behalf of the embedding process, and [`Processor::claim_ipc_methods`] lets the host answer
/// methods Vivido cannot — or place a window itself rather than letting `create_window` build a
/// top-level one.
#[cfg(any(unix, windows))]
pub mod host {
    pub use crate::polling::ipc::{IpcConnection, IpcError, IpcRequest, default_endpoint};
    pub use crate::polling::transport::LocalStream;
    pub use crate::polling::{IoListener, IoListenerHandle};
}

/// Internal exports used by the package's executable targets.
#[doc(hidden)]
pub mod binary {
    pub mod headless {
        pub use crate::headless::run;
        #[cfg(any(target_os = "macos", windows))]
        pub use crate::headless::run_reexec;
    }

    pub mod logging {
        pub use crate::logging::initialize;
    }

    #[cfg(target_os = "macos")]
    pub mod macos {
        pub use crate::macos::disable_autofill;

        pub mod locale {
            pub use crate::macos::locale::set_locale_environment;
        }
    }

    #[cfg(windows)]
    pub mod panic {
        pub use crate::panic::attach_handler;
    }

    pub mod polling {
        pub use crate::polling::IoListener;
        pub use crate::polling::transport::{LocalListener, LocalStream};

        pub mod ipc {
            pub use crate::polling::ipc::{request_once, send_message};
        }
    }

    pub mod session {
        pub use crate::session::{
            RegistryGuard, SessionPaths, list_registries, print_sessions, terminate_session,
        };
    }
}
