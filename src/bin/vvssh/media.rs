//! Supervision of the separate realtime and bulk media SSH connections.
//!
//! Each lane is its own SSH connection holding one remote socket forward. Started once and left
//! alone, a lane that drops (a NAT timeout, a network change, a restarted sshd) leaves the
//! interactive session alive but with a remote socket that refuses connections, so every nested
//! producer — `vivi`, `vvrd`, anything in a `vvmux` pane — sees `Connection refused` and shows
//! nothing for the rest of the session. This restarts a lane that ended until the session does.

use std::ffi::OsString;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::askpass::UnattendedCredentials;

const FIRST_WAIT: Duration = Duration::from_secs(1);
const LONGEST_WAIT: Duration = Duration::from_secs(30);
/// A lane that stayed up this long was healthy: its next drop starts from `FIRST_WAIT` again.
const HEALTHY: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(50);

/// A running lane. Stopping it ends the SSH connection and the supervising thread.
pub(super) struct MediaLane {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MediaLane {
    /// Supervise `initial`, which already passed its readiness check, and start `lane` again from
    /// `ssh arguments` whenever it ends. A restart retries only with credentials the session
    /// already has, so it never prompts in the terminal the person is working in.
    pub(super) fn start(
        lane: &'static str,
        ssh: OsString,
        arguments: Vec<OsString>,
        credentials: UnattendedCredentials,
        initial: Child,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::Builder::new()
            .name(format!("vvssh-media-{lane}"))
            .spawn(move || {
                let mut child = initial;
                let mut wait = FIRST_WAIT;
                let mut attempt = 0_u32;
                loop {
                    let started = Instant::now();
                    if !wait_for_exit(&mut child, &stopping) {
                        return;
                    }
                    if started.elapsed() >= HEALTHY {
                        wait = FIRST_WAIT;
                    }
                    if !sleep_unless_stopped(wait, &stopping) {
                        return;
                    }
                    wait = (wait * 2).min(LONGEST_WAIT);
                    attempt = attempt.saturating_add(1);
                    let mut command = Command::new(&ssh);
                    command
                        .args(&arguments)
                        // The remote command waits on this pipe, and the readiness marker it
                        // prints is a few bytes that fit the pipe unread.
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null());
                    credentials.apply(&mut command, lane, attempt);
                    match command.spawn() {
                        Ok(next) => child = next,
                        Err(_) => return,
                    }
                }
            })
            .ok();
        Self { stop, worker }
    }
}

impl Drop for MediaLane {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Block until `child` exits (true) or a stop is requested, which kills it (false).
fn wait_for_exit(child: &mut Child, stopping: &AtomicBool) -> bool {
    loop {
        if stopping.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        match child.try_wait() {
            Ok(None) => std::thread::sleep(POLL),
            Ok(Some(_)) | Err(_) => return true,
        }
    }
}

fn sleep_unless_stopped(duration: Duration, stopping: &AtomicBool) -> bool {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        if stopping.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(POLL);
    }
    !stopping.load(Ordering::SeqCst)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn credentials() -> UnattendedCredentials {
        super::super::askpass::CredentialBroker::new(std::process::id(), 1)
            .expect("broker")
            .unattended()
    }

    #[test]
    fn a_lane_that_ends_is_started_again_and_stops_with_the_session() {
        let directory = std::env::temp_dir().join(format!("vvssh-media-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let marker = directory.join("starts");
        let _ = std::fs::remove_file(&marker);
        // Stands in for ssh: record one start and exit at once, so the supervisor must keep
        // restarting it.
        let script = directory.join("fake-ssh");
        std::fs::write(&script, format!("#!/bin/sh\necho x >> {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let initial =
            Command::new("true").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let lane = MediaLane::start(
            "realtime",
            script.into_os_string(),
            Vec::new(),
            credentials(),
            initial,
        );
        let started = Instant::now();
        while std::fs::read_to_string(&marker).map(|text| text.lines().count()).unwrap_or(0) < 2
            && started.elapsed() < Duration::from_secs(10)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let restarts = std::fs::read_to_string(&marker).unwrap_or_default().lines().count();
        drop(lane);
        let _ = std::fs::remove_dir_all(&directory);
        assert!(restarts >= 2, "lane was restarted {restarts} times");
    }
}
