//! Logging for Vivido.
//!
//! The main executable is supposed to call `initialize()` exactly once during
//! startup. All logging messages are written to stdout, given that their
//! log-level is sufficient for the level configured in `cli::Options`.

use std::fs::{File, OpenOptions};
use std::io::{self, LineWriter, Stdout, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use std::{env, process};

use log::{Level, LevelFilter};

use crate::cli::Options;
use crate::event::{Event, EventSink, EventType};
use crate::message_bar::{Message, MessageType};

/// Logging target for IPC config error messages.
pub const LOG_TARGET_IPC_CONFIG: &str = "vivido_log_window_config";

/// Logging target for config error messages.
pub const LOG_TARGET_CONFIG: &str = "vivido_config_derive";

/// Logging target for winit events.
pub const LOG_TARGET_WINIT: &str = "vivido_winit_event";

/// Name for the environment variable containing extra logging targets.
///
/// The targets are semicolon separated.
const VIVIDO_EXTRA_LOG_TARGETS_ENV: &str = "VIVIDO_EXTRA_LOG_TARGETS";

/// User configurable extra log targets to include.
fn extra_log_targets() -> &'static [String] {
    static EXTRA_LOG_TARGETS: OnceLock<Vec<String>> = OnceLock::new();

    EXTRA_LOG_TARGETS.get_or_init(|| {
        env::var(VIVIDO_EXTRA_LOG_TARGETS_ENV)
            .map_or(Vec::new(), |targets| targets.split(';').map(ToString::to_string).collect())
    })
}

/// List of targets which will be logged by Vivido.
const ALLOWED_TARGETS: &[&str] = &[
    LOG_TARGET_IPC_CONFIG,
    LOG_TARGET_CONFIG,
    LOG_TARGET_WINIT,
    "vivido_config_derive",
    "vivido_terminal",
    "vivido",
];

/// Initialize the logger to its defaults.
///
/// # Errors
///
/// Returns an error when another global logger has already been installed.
pub fn initialize(
    options: &Options,
    event_proxy: EventSink,
) -> Result<Option<PathBuf>, log::SetLoggerError> {
    log::set_max_level(options.log_level());

    let logger = Logger::new(event_proxy);
    let path = logger.file_path();
    log::set_boxed_logger(Box::new(logger))?;

    Ok(path)
}

pub struct Logger {
    logfile: Mutex<OnDemandLogFile>,
    stdout: Mutex<LineWriter<Stdout>>,
    event_proxy: Mutex<EventSink>,
    start: Instant,
}

impl Logger {
    fn new(event_proxy: EventSink) -> Self {
        let logfile = Mutex::new(OnDemandLogFile::new());
        let stdout = Mutex::new(LineWriter::new(io::stdout()));

        Logger { logfile, stdout, event_proxy: Mutex::new(event_proxy), start: Instant::now() }
    }

    fn file_path(&self) -> Option<PathBuf> {
        let logfile_lock = self.logfile.lock().ok()?;
        Some(logfile_lock.path().clone())
    }

    /// Log a record to the message bar.
    fn message_bar_log(&self, record: &log::Record<'_>, logfile_path: &str) {
        let message_type = match record.level() {
            Level::Error => MessageType::Error,
            Level::Warn => MessageType::Warning,
            _ => return,
        };

        let event_proxy = match self.event_proxy.lock() {
            Ok(event_proxy) => event_proxy,
            Err(_) => return,
        };

        let message = format!(
            "[{}] {}\nSee log at {}",
            record.level(),
            redact(&record.args().to_string()),
            logfile_path,
        );

        let mut message = Message::new(message, message_type);
        message.set_target(record.target().to_owned());

        let _ = event_proxy.send_event(Event::new(EventType::Message(message), None));
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        // Get target crate.
        let index = record.target().find(':').unwrap_or_else(|| record.target().len());
        let target = &record.target()[..index];

        // Only log our own crates, except when logging at Level::Trace.
        if !self.enabled(record.metadata()) || !is_allowed_target(record.level(), target) {
            return;
        }

        // Create log message for the given `record` and `target`.
        let message = create_log_message(record, target, self.start);

        if let Ok(mut logfile) = self.logfile.lock() {
            // Write to logfile.
            let _ = logfile.write_all(message.as_ref());

            // Log relevant entries to message bar.
            self.message_bar_log(record, &logfile.path.to_string_lossy());
        }

        // Write to stdout.
        if let Ok(mut stdout) = self.stdout.lock() {
            let _ = stdout.write_all(message.as_ref());
        }
    }

    fn flush(&self) {}
}

/// Redact sensitive keys, identifying paths, URLs, and long hexadecimal capability material.
///
/// This is defense in depth for legacy messages. Call sites must never interpolate secrets or
/// arbitrary client text; use named scalar fields and stable event messages instead.
fn redact(message: &str) -> String {
    let mut result = String::new();
    let mut redact_next = false;
    for word in message.split_whitespace() {
        if result.len() >= 4096 {
            result.push_str(" [truncated]");
            break;
        }
        let lower = word.to_ascii_lowercase();
        let sensitive =
            ["secret", "password", "token", "ticket", "channel_key", "lease_key", "resume_key"]
                .iter()
                .any(|key| lower.contains(key));
        let path_or_url = word.trim_start_matches(['\"', '\'', '(', '[']).starts_with('/')
            || word.contains(":\\")
            || word.contains("=/")
            || word.contains("https://")
            || word.contains("http://");
        let capability = word.split(|c: char| !c.is_ascii_hexdigit()).any(|part| part.len() >= 32);
        if !result.is_empty() {
            result.push(' ');
        }
        if redact_next || sensitive || path_or_url || capability {
            result.push_str("[redacted]");
        } else {
            // The JSON encoder escapes control characters; bound each retained token as well.
            for ch in word.chars().take(256) {
                result.push(ch);
            }
        }
        redact_next = sensitive && !word.contains('=');
    }
    result
}

struct LogFields(serde_json::Map<String, serde_json::Value>);

impl<'kvs> log::kv::VisitSource<'kvs> for LogFields {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        // Unknown fields are private by default, including paths, commands and credentials.
        let name = key.as_str();
        let value = if [
            "event",
            "error_kind",
            "fault_id",
            "class",
            "window_id",
            "connection_id",
            "count",
            "bytes",
            "elapsed_ms",
        ]
        .contains(&name)
        {
            if let Some(value) = value.to_u64() {
                serde_json::json!(value)
            } else if let Some(value) = value.to_bool() {
                serde_json::json!(value)
            } else {
                serde_json::json!(redact(&value.to_string()))
            }
        } else {
            serde_json::json!("[redacted]")
        };
        if self.0.len() < 32 {
            self.0.insert(name.chars().take(64).collect(), value);
        }
        Ok(())
    }
}

fn create_log_message(record: &log::Record<'_>, _target: &str, start: Instant) -> String {
    let mut fields = LogFields(serde_json::Map::new());
    let _ = record.key_values().visit(&mut fields);
    let event = fields.0.remove("event").unwrap_or_else(|| serde_json::json!("legacy_message"));
    let record = serde_json::json!({
        "elapsed_ms": start.elapsed().as_millis(),
        "level": record.level().as_str(),
        "target": record.target(),
        "event": event,
        "message": redact(&record.args().to_string()),
        "fields": fields.0,
    });
    format!("{record}\n")
}

/// Check if log messages from a crate should be logged.
fn is_allowed_target(level: Level, target: &str) -> bool {
    match (level, log::max_level()) {
        (Level::Error, LevelFilter::Trace) | (Level::Warn, LevelFilter::Trace) => true,
        _ => ALLOWED_TARGETS.contains(&target) || extra_log_targets().iter().any(|t| t == target),
    }
}

struct OnDemandLogFile {
    file: Option<LineWriter<File>>,
    created: Arc<AtomicBool>,
    path: PathBuf,
}

impl OnDemandLogFile {
    fn new() -> Self {
        let mut path = env::temp_dir();
        path.push(format!("Vivido-{}.log", process::id()));

        OnDemandLogFile { path, file: None, created: Arc::new(AtomicBool::new(false)) }
    }

    fn file(&mut self) -> Result<&mut LineWriter<File>, io::Error> {
        // Allow to recreate the file if it has been deleted at runtime.
        if self.file.is_some() && !self.path.as_path().exists() {
            self.file = None;
        }

        // Create the file if it doesn't exist yet.
        if self.file.is_none() {
            let file = OpenOptions::new().append(true).create_new(true).open(&self.path);

            match file {
                Ok(file) => {
                    self.file = Some(io::LineWriter::new(file));
                    self.created.store(true, Ordering::Relaxed);
                    let _ =
                        writeln!(io::stdout(), "Created log file at \"{}\"", self.path.display());
                },
                Err(e) => {
                    let _ = writeln!(io::stdout(), "Unable to create log file: {e}");
                    return Err(e);
                },
            }
        }

        Ok(self.file.as_mut().unwrap())
    }

    fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl Write for OnDemandLogFile {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        self.file()?.write(buf)
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        self.file()?.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_fields_survive_and_sensitive_fields_are_private_by_default() {
        let fields = [
            ("event", "client_fault"),
            ("fault_id", "17"),
            ("password", "synthetic-password"),
            ("unclassified", "synthetic-secret"),
        ];
        let record = log::Record::builder()
            .args(format_args!("Client worker stopped"))
            .target("vivido::worker")
            .level(Level::Error)
            .key_values(&fields)
            .build();
        let encoded = create_log_message(&record, "vivido", Instant::now());
        let decoded: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded["event"], "client_fault");
        assert_eq!(decoded["fields"]["fault_id"], "17");
        assert!(!encoded.contains("synthetic-password"));
        assert!(!encoded.contains("synthetic-secret"));
    }

    #[test]
    fn legacy_context_redacts_paths_capabilities_and_named_credentials() {
        let message = redact(
            "Failed /Users/private/config.toml token=synthetic-token password: synthetic-password 0123456789abcdef0123456789abcdef",
        );
        assert!(!message.contains("private"));
        assert!(!message.contains("synthetic"));
        assert!(!message.contains("012345"));
        assert!(message.starts_with("Failed"));
    }
}
