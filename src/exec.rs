//! One-shot command execution inside a window's working directory.
//!
//! `exec` runs a shell command with piped output and reports back when it completes, so
//! agents get exit codes and split stdout/stderr without simulating keypresses and scraping
//! the viewport. The event loop never blocks: [`spawn_exec_command`] owns the whole wait,
//! including the timeout kill, on a worker thread and replies from there.

use std::io::{Error as IoError, ErrorKind, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// Maximum shell command bytes accepted by `exec`.
pub const MAX_EXEC_COMMAND_BYTES: usize = 64 * 1024;
/// Maximum bytes captured per output stream. Readers keep draining past the cap so the
/// child never blocks on a full pipe; the excess is discarded and reported as truncated.
pub const MAX_EXEC_OUTPUT_BYTES: usize = 1024 * 1024;
/// Default `exec` wait: one minute, as in the automation plan's example.
pub const DEFAULT_EXEC_TIMEOUT_MS: u64 = 60_000;
/// Maximum `exec` wait: 24 hours, matching long-horizon capture timeouts.
pub const MAX_EXEC_TIMEOUT_MS: u64 = 86_400_000;
/// Grace period for output readers after the child exits. Readers normally hit EOF
/// immediately; the grace only bounds grandchildren holding the pipes open, which agents
/// should avoid by redirecting daemon output.
const READER_GRACE: Duration = Duration::from_secs(2);
/// Child poll interval while waiting for exit or the deadline.
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Pipe read chunk size.
const READ_CHUNK: usize = 8192;

/// Outcome of one `exec` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutcome {
    /// Process exit code; `None` when no code is available (spawn failure aside, a signal).
    pub exit_code: Option<i32>,
    /// Signal number on Unix when the process died from one.
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Either stream exceeded the capture cap, or readers were still draining at grace expiry.
    pub truncated: bool,
    pub timed_out: bool,
    /// Wall time from spawn to reaped child and drained pipes.
    pub duration_ms: u64,
    /// Set instead of every other field when the child could not be spawned.
    pub spawn_error: Option<String>,
}

/// Run a shell command to completion, killing it at the deadline.
///
/// Blocking: callers serve it from a worker thread (see [`spawn_exec_command`]).
pub fn run_exec_command(command: &str, cwd: Option<&Path>, timeout: Duration) -> ExecOutcome {
    run_exec_command_with_cap(command, cwd, timeout, MAX_EXEC_OUTPUT_BYTES)
}

fn run_exec_command_with_cap(
    command: &str,
    cwd: Option<&Path>,
    timeout: Duration,
    cap: usize,
) -> ExecOutcome {
    let started = Instant::now();
    let mut shell = if cfg!(windows) {
        let mut command_line = Command::new("cmd");
        command_line.args(["/C", command]);
        command_line
    } else {
        let mut shell = Command::new("sh");
        shell.args(["-c", command]);
        shell
    };
    shell.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        shell.current_dir(cwd);
    }
    let mut child = match shell.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ExecOutcome {
                exit_code: None,
                signal: None,
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                duration_ms: millis_since(started),
                spawn_error: Some(error.to_string()),
            };
        },
    };
    let stdout = spawn_stream_reader(child.stdout.take(), cap);
    let stderr = spawn_stream_reader(child.stderr.take(), cap);

    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(POLL_INTERVAL);
            },
            Err(_) => {
                let _ = child.kill();
                break child.wait().ok();
            },
        }
    };
    let (exit_code, signal) = match status {
        Some(status) => {
            #[cfg(unix)]
            let signal = status.signal();
            #[cfg(not(unix))]
            let signal: Option<i32> = None;
            (status.code(), signal)
        },
        None => (None, None),
    };
    let (stdout, stdout_truncated) = collect_stream(stdout);
    let (stderr, stderr_truncated) = collect_stream(stderr);
    ExecOutcome {
        exit_code,
        signal,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated: stdout_truncated || stderr_truncated,
        timed_out,
        duration_ms: millis_since(started),
        spawn_error: None,
    }
}

/// Drain one piped stream on its own thread so the child never blocks on a full pipe.
///
/// Only the first `cap` bytes are kept; the rest still drains into the void.
fn spawn_stream_reader(
    pipe: Option<impl Read + Send + 'static>,
    cap: usize,
) -> mpsc::Receiver<(Vec<u8>, usize)> {
    let (sender, receiver) = mpsc::channel();
    // A reader that never starts drops the sender with its closure, which the collector
    // reads as truncated output rather than hanging the worker.
    let _ = std::thread::Builder::new().name("vivido-exec-reader".into()).spawn(move || {
        let mut kept = Vec::new();
        let mut total = 0usize;
        if let Some(mut pipe) = pipe {
            let mut chunk = [0u8; READ_CHUNK];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        total = total.saturating_add(read);
                        let room = cap.saturating_sub(kept.len());
                        kept.extend_from_slice(&chunk[..read.min(room)]);
                    },
                    Err(error) if error.kind() == ErrorKind::Interrupted => {},
                    Err(_) => break,
                }
            }
        }
        let _ = sender.send((kept, total));
    });
    receiver
}

/// Collect one stream, waiting only a bounded grace past child exit for slow EOFs.
fn collect_stream(receiver: mpsc::Receiver<(Vec<u8>, usize)>) -> (Vec<u8>, bool) {
    match receiver.recv_timeout(READER_GRACE) {
        Ok((bytes, total)) => {
            let truncated = total > bytes.len();
            (bytes, truncated)
        },
        Err(_) => (Vec::new(), true),
    }
}

fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Run one `exec` request on a worker thread and reply when it completes.
///
/// The event loop never blocks: the worker owns the whole wait, including the timeout kill.
/// Returns an error only when the worker itself cannot be spawned, in which case the caller
/// replies immediately.
pub fn spawn_exec_command(
    connection: crate::polling::ipc::IpcConnection,
    request_id: u64,
    command: String,
    cwd: Option<std::path::PathBuf>,
    timeout_ms: u64,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("vivido-exec".into())
        .spawn(move || {
            let outcome =
                run_exec_command(&command, cwd.as_deref(), Duration::from_millis(timeout_ms));
            if let Some(error) = outcome.spawn_error {
                connection
                    .error(request_id, crate::polling::ipc::IpcError::new("exec_failed", error));
                return;
            }
            let result = serde_json::json!({
                "exit_code": outcome.exit_code,
                "signal": outcome.signal,
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
                "truncated": outcome.truncated,
                "duration_ms": outcome.duration_ms,
            });
            if outcome.timed_out {
                connection.error(
                    request_id,
                    crate::polling::ipc::IpcError::new(
                        "timeout",
                        format!("exec timed out after {} ms", outcome.duration_ms),
                    )
                    .with_data(result),
                );
            } else {
                connection.reply(request_id, result);
            }
        })
        .map(|_| ())
        .map_err(|error| IoError::new(error.kind(), format!("cannot spawn exec worker: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_reports_exit_code_and_split_streams() {
        let outcome = run_exec_command("echo hello", None, Duration::from_secs(30));
        assert_eq!(outcome.spawn_error, None);
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.stdout.trim(), "hello");
        assert_eq!(outcome.stderr, "");
        assert!(!outcome.truncated);
    }

    #[test]
    fn exec_reports_nonzero_exit() {
        let outcome = run_exec_command("exit 3", None, Duration::from_secs(30));
        assert_eq!(outcome.exit_code, Some(3));
        assert!(!outcome.timed_out);
    }

    #[test]
    fn exec_kills_at_the_deadline() {
        // `timeout.exe` refuses redirected stdin, so sleep with PowerShell instead.
        #[cfg(windows)]
        let sleep = "powershell -noprofile -command Start-Sleep -Seconds 30";
        #[cfg(not(windows))]
        let sleep = "sleep 30";
        let outcome = run_exec_command(sleep, None, Duration::from_millis(300));
        assert!(outcome.timed_out);
        assert!(outcome.duration_ms >= 300);
        assert!(outcome.duration_ms < 30_000);
    }

    #[test]
    fn exec_truncates_streams_past_the_cap() {
        let outcome = run_exec_command_with_cap("echo hello", None, Duration::from_secs(30), 4);
        assert!(outcome.truncated);
        assert_eq!(outcome.stdout, "hell");
    }

    #[test]
    fn exec_reports_spawn_failure() {
        #[cfg(windows)]
        let missing = Path::new("Z:\\definitely\\not\\a\\vivido-exec-test-dir");
        #[cfg(not(windows))]
        let missing = Path::new("/definitely/not/a/vivido-exec-test-dir");
        let outcome = run_exec_command("echo hello", Some(missing), Duration::from_secs(30));
        assert!(outcome.spawn_error.is_some());
        assert_eq!(outcome.exit_code, None);
    }
}
