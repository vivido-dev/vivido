//! Local IPC and shutdown event listeners.

#[cfg(unix)]
use polling::{Event as PollEvent, Events, Poller};
#[cfg(windows)]
use std::io;
use std::io::Error as IoError;
use std::path::PathBuf;

use log::error;
use std::result::Result;

use crate::terminal::thread;

use crate::UiConfig;
use crate::cli::Options;
use crate::event::EventSink;
#[cfg(windows)]
use crate::event::{Event, EventType};
use crate::polling::ipc::IpcListener;
#[cfg(unix)]
use crate::polling::signal::SignalListener;

pub mod ipc;
#[cfg(unix)]
mod signal;
pub(crate) mod transport;

/// Polling key for signal read events.
#[cfg(unix)]
const SIGNAL_READ_KEY: usize = 1;

/// Polling key for IPC read events.
#[cfg(unix)]
const IPC_READ_KEY: usize = 0;

/// Unix I/O event listener.
#[cfg(unix)]
pub struct IoListener {
    ipc_listener: Option<IpcListener>,
    signal_listener: SignalListener,
    events: Events,
    poller: Poller,
}

#[cfg(unix)]
impl IoListener {
    /// Create background thread to listen for I/O events.
    pub fn spawn(
        config: &UiConfig,
        options: &Options,
        event_proxy: EventSink,
    ) -> Result<IoListenerHandle, IoError> {
        let poller = Poller::new()?;
        let events = Events::new();

        // Create socket listener for IPC messages.
        let (ipc_socket_path, ipc_listener) = if config.ipc_socket() {
            let ipc_socket_path = options.socket.clone().unwrap_or_else(ipc::default_endpoint);
            let ipc_listener = IpcListener::new(options, event_proxy.clone(), &ipc_socket_path)?;
            (Some(ipc_socket_path), Some(ipc_listener))
        } else {
            (None, None)
        };

        // Create listener for Unix signals.
        let signal_listener = SignalListener::new(event_proxy)?;

        // SAFETY: Correct drop order is taken care of by `Drop` implementation.
        unsafe { poller.add(&signal_listener.pipe, PollEvent::readable(SIGNAL_READ_KEY))? };
        if let Some(ipc_listener) = &ipc_listener {
            unsafe { poller.add(&ipc_listener.socket, PollEvent::readable(IPC_READ_KEY))? };
        }

        let mut listener = Self { signal_listener, ipc_listener, events, poller };

        thread::spawn_named("io event listener", move || {
            loop {
                if let Err(err) = listener.poll() {
                    error!("Failed to poll for I/O events: {err}");
                }
            }
        });

        Ok(IoListenerHandle { ipc_socket_path })
    }

    /// Process the next I/O event.
    fn poll(&mut self) -> Result<(), IoError> {
        // Ensure interests are present for the next poll.
        self.poller.modify(&self.signal_listener.pipe, PollEvent::readable(SIGNAL_READ_KEY))?;
        if let Some(ipc_listener) = &self.ipc_listener {
            self.poller.modify(&ipc_listener.socket, PollEvent::readable(IPC_READ_KEY))?;
        }

        // Wait for the next event to be ready.
        self.events.clear();
        self.poller.wait(&mut self.events, None)?;

        for event in self.events.iter() {
            if event.key == IPC_READ_KEY {
                if let Some(ipc_listener) = &mut self.ipc_listener {
                    ipc_listener.process_message()?;
                }
            } else if event.key == SIGNAL_READ_KEY {
                self.signal_listener.process_signal()?;
            }
        }

        Ok(())
    }
}

#[cfg(unix)]
impl Drop for IoListener {
    fn drop(&mut self) {
        if let Err(err) = self.poller.delete(&self.signal_listener.pipe) {
            error!("Failed to remove signal listener interest: {err}");
        }
        if let Some(ipc_listener) = &self.ipc_listener
            && let Err(err) = self.poller.delete(&ipc_listener.socket)
        {
            error!("Failed to remove IPC listener interest: {err}");
        }
    }
}

/// Windows uses a dedicated blocking named-pipe accept thread.
#[cfg(windows)]
pub struct IoListener;

#[cfg(windows)]
impl IoListener {
    pub fn spawn(
        config: &UiConfig,
        options: &Options,
        event_proxy: EventSink,
    ) -> Result<IoListenerHandle, IoError> {
        install_console_handler(event_proxy.clone())?;
        if !config.ipc_socket() {
            return Ok(IoListenerHandle { ipc_socket_path: None });
        }

        let endpoint = options.socket.clone().unwrap_or_else(ipc::default_endpoint);
        let mut listener = IpcListener::new(options, event_proxy, &endpoint)?;
        thread::spawn_named("IPC accept listener", move || {
            loop {
                if let Err(error) = listener.process_message() {
                    error!("Failed to accept IPC connection: {error}");
                }
            }
        });
        Ok(IoListenerHandle { ipc_socket_path: Some(endpoint) })
    }
}

#[cfg(windows)]
static CONSOLE_EVENTS: std::sync::OnceLock<EventSink> = std::sync::OnceLock::new();

#[cfg(windows)]
fn install_console_handler(event_proxy: EventSink) -> io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    let _ = CONSOLE_EVENTS.set(event_proxy);
    // SAFETY: the callback is process-global, has the required ABI, and only accesses OnceLock.
    if unsafe { SetConsoleCtrlHandler(Some(console_handler), 1) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
unsafe extern "system" fn console_handler(_event: u32) -> i32 {
    if let Some(proxy) = CONSOLE_EVENTS.get() {
        let _ = proxy.send_event(Event::new(EventType::Shutdown, None));
    }
    1
}

/// Public I/O event listener state.
pub struct IoListenerHandle {
    pub ipc_socket_path: Option<PathBuf>,
}
