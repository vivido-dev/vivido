use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::{debug, error, warn};
use notify::event::{ModifyKind, RenameMode};
use notify::{
    Config, Error as NotifyError, Event as NotifyEvent, EventKind, RecommendedWatcher,
    RecursiveMode, Watcher,
};

use crate::terminal::thread;

use crate::event::{Event, EventSink, EventType};

// Coalesce an editor's save/rename burst within ten milliseconds without waiting a full frame.
const DEBOUNCE_DELAY: Duration = Duration::from_millis(10);

/// The fallback for `RecommendedWatcher` polling.
const FALLBACK_POLLING_TIMEOUT: Duration = Duration::from_secs(1);

/// Config file update monitor.
pub struct ConfigMonitor {
    backend: MonitorBackend,
    watched_hash: Option<u64>,
}

#[derive(Debug)]
enum MonitorBackend {
    Native {
        thread: JoinHandle<()>,
        shutdown_tx: Sender<Result<NotifyEvent, NotifyError>>,
    },
    #[cfg(feature = "test-util")]
    Mocked(MonitorControl),
}

#[derive(Debug)]
struct DebounceState {
    paths: Vec<PathBuf>,
    deadline: Option<Duration>,
    pending: bool,
    stopped: bool,
}

impl DebounceState {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths, deadline: None, pending: false, stopped: false }
    }

    fn receive(&mut self, event: NotifyEvent, now: Duration) {
        if self.stopped {
            return;
        }
        match event.kind {
            EventKind::Other if event.info() == Some("shutdown") => self.stopped = true,
            EventKind::Modify(ModifyKind::Name(RenameMode::From | RenameMode::Both)) => (),
            EventKind::Any | EventKind::Create(_) | EventKind::Modify(_) | EventKind::Other
                if event.paths.iter().any(|path| self.paths.contains(path)) =>
            {
                self.pending = true;
                self.deadline.get_or_insert(now.saturating_add(DEBOUNCE_DELAY));
            },
            _ => (),
        }
    }

    fn flush(&mut self, now: Duration, sink: &EventSink) {
        if !self.stopped && self.deadline.is_some_and(|deadline| now >= deadline) {
            self.deadline = None;
            if std::mem::take(&mut self.pending)
                && let Some(primary) = self.paths.first()
            {
                let _ = sink.send_event(Event::new(EventType::ConfigReload(primary.clone()), None));
            }
        }
    }
}

/// Deterministic filesystem notifications and virtual time for one mocked monitor.
///
/// This controller creates no watcher or thread. Clones share only their originating monitor.
#[cfg(feature = "test-util")]
#[derive(Clone)]
pub struct MonitorControl {
    state: std::sync::Arc<std::sync::Mutex<(DebounceState, Duration)>>,
    sink: EventSink,
}

#[cfg(feature = "test-util")]
impl std::fmt::Debug for MonitorControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorControl").finish_non_exhaustive()
    }
}

#[cfg(feature = "test-util")]
impl MonitorControl {
    /// Deliver a notification without accessing the filesystem.
    ///
    /// # Panics
    ///
    /// Panics if a prior panic poisoned the mock monitor state.
    pub fn notify(&self, event: NotifyEvent) {
        let mut state = self.state.lock().expect("mock monitor state poisoned");
        let now = state.1;
        state.0.receive(event, now);
    }

    /// Advance virtual time and deliver any due reload notification.
    ///
    /// # Panics
    ///
    /// Panics if a prior panic poisoned the mock monitor state.
    pub fn advance(&self, elapsed: Duration) {
        let mut state = self.state.lock().expect("mock monitor state poisoned");
        state.1 = state.1.saturating_add(elapsed);
        let now = state.1;
        state.0.flush(now, &self.sink);
    }
}

impl ConfigMonitor {
    /// Watch existing configuration files and reload the primary file after save bursts.
    ///
    /// Returns `None` if no regular files remain or the native watcher cannot be created.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot start the monitor worker thread.
    pub fn new(mut paths: Vec<PathBuf>, event_proxy: EventSink) -> Option<Self> {
        // Don't monitor config if there is no path to watch.
        if paths.is_empty() {
            return None;
        }

        // Calculate the hash for the unmodified list of paths.
        let watched_hash = Self::hash_paths(&paths);

        // Exclude char devices like `/dev/null`, sockets, and so on, by checking that file type is
        // a regular file.
        paths.retain(|path| {
            // Call `metadata` to resolve symbolic links.
            path.metadata().is_ok_and(|metadata| metadata.file_type().is_file())
        });

        if paths.is_empty() {
            return None;
        }

        // Canonicalize paths, keeping the base paths for symlinks.
        for i in 0..paths.len() {
            if let Ok(canonical_path) = paths[i].canonicalize() {
                match paths[i].symlink_metadata() {
                    Ok(metadata) if metadata.file_type().is_symlink() => paths.push(canonical_path),
                    _ => paths[i] = canonical_path,
                }
            }
        }

        // The Duration argument is a debouncing period.
        let (tx, rx) = mpsc::channel();
        let Ok(mut watcher) = RecommendedWatcher::new(
            tx.clone(),
            Config::default().with_poll_interval(FALLBACK_POLLING_TIMEOUT),
        ) else {
            error!(event = "config_watcher_start_failed"; "Unable to create configuration watcher");
            return None;
        };

        let join_handle = thread::spawn_named("config watcher", move || {
            // Get all unique parent directories.
            let mut parents = paths
                .iter()
                .map(|path| {
                    let mut path = path.clone();
                    path.pop();
                    path
                })
                .collect::<Vec<PathBuf>>();
            parents.sort_unstable();
            parents.dedup();

            // Watch all configuration file directories.
            for parent in &parents {
                if watcher.watch(parent, RecursiveMode::NonRecursive).is_err() {
                    debug!(event = "config_watch_failed"; "Unable to watch a configuration directory");
                }
            }

            // Keep one pending bit rather than accumulating an unbounded event vector.
            let origin = Instant::now();
            let mut state = DebounceState::new(paths);
            while !state.stopped {
                let event = match state.deadline {
                    Some(deadline) => rx.recv_timeout(deadline.saturating_sub(origin.elapsed())),
                    None => rx.recv().map_err(Into::into),
                };
                match event {
                    Ok(Ok(event)) => state.receive(event, origin.elapsed()),
                    Ok(Err(_)) => debug!("Config watcher reported a notification error"),
                    Err(RecvTimeoutError::Timeout) => (),
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                state.flush(origin.elapsed(), &event_proxy);
            }
        });

        Some(Self {
            watched_hash,
            backend: MonitorBackend::Native { thread: join_handle, shutdown_tx: tx },
        })
    }

    /// Synchronously shut down the monitor.
    ///
    /// # Panics
    ///
    /// Panics if a prior panic poisoned the mock monitor state.
    pub fn shutdown(self) {
        match self.backend {
            MonitorBackend::Native { thread, shutdown_tx } => {
                let event = NotifyEvent::new(EventKind::Other).set_info("shutdown");
                let _ = shutdown_tx.send(Ok(event));
                if thread.join().is_err() {
                    warn!("Config monitor worker panicked during shutdown");
                }
            },
            #[cfg(feature = "test-util")]
            MonitorBackend::Mocked(control) => {
                control.state.lock().expect("mock monitor state poisoned").0.stopped = true;
            },
        }
    }

    /// Create an isolated monitor driven entirely by notifications and virtual time.
    ///
    /// Paths are accepted literally, without metadata calls or canonicalization. The first path
    /// is the primary configuration file, matching native reload behavior.
    #[cfg(feature = "test-util")]
    pub fn new_mocked(paths: Vec<PathBuf>, sink: EventSink) -> (Self, MonitorControl) {
        let watched_hash = Self::hash_paths(&paths);
        let control = MonitorControl {
            state: std::sync::Arc::new(std::sync::Mutex::new((
                DebounceState::new(paths),
                Duration::ZERO,
            ))),
            sink,
        };
        (Self { watched_hash, backend: MonitorBackend::Mocked(control.clone()) }, control)
    }

    /// Check if the config monitor needs to be restarted.
    ///
    /// This checks the supplied list of files against the monitored files to determine if a
    /// restart is necessary.
    pub fn needs_restart(&self, files: &[PathBuf]) -> bool {
        Self::hash_paths(files).is_none_or(|hash| Some(hash) != self.watched_hash)
    }

    /// Generate the hash for a list of paths.
    fn hash_paths(files: &[PathBuf]) -> Option<u64> {
        // Use file count limit to avoid allocations.
        const MAX_PATHS: usize = 1024;
        if files.len() > MAX_PATHS {
            return None;
        }

        // Sort files to avoid restart on order change.
        let mut sorted_files = [None; MAX_PATHS];
        for (i, file) in files.iter().enumerate() {
            sorted_files[i] = Some(file);
        }
        sorted_files.sort_unstable();

        // Calculate hash for the paths, regardless of order.
        let mut hasher = DefaultHasher::new();
        Hash::hash_slice(&sorted_files, &mut hasher);
        Some(hasher.finish())
    }
}

// Debug never traverses watched paths or acquires the mock controller's lock.
impl std::fmt::Debug for ConfigMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigMonitor")
            .field("watched_hash", &self.watched_hash)
            .finish_non_exhaustive()
    }
}
