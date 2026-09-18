//! The agent mesh lane: a `vvagent bridge` to the remote host's agent mesh, supervised for as long
//! as the session lasts (`docs/vvagent-inter-host-plan.md` §8.3).
//!
//! `vvssh` links no mesh crate. It starts `vvagent bridge --dial` with an SSH carrier and decides,
//! from how each attempt ended, whether another is worth making. The bridge reports that in its
//! exit status; retrying is this side's job because this is where the session's credentials live,
//! and it retries only with credentials the session already has — a reconnect never prompts in the
//! terminal the person is working in. Nothing here writes to that terminal either: whether the
//! mesh is connected is `vvagent peer list`'s to say.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::RemotePlatform;
use super::askpass::UnattendedCredentials;

/// `vvagent bridge` exit statuses (see `vvagent bridge --help`).
const CLOSED: i32 = 0;
const UNREACHABLE: i32 = 69;
const BROKEN: i32 = 74;
const STANDBY: i32 = 75;

/// The local `vvagent`, if the mesh is installed and not switched off.
pub(super) fn local_vvagent() -> Option<PathBuf> {
    if std::env::var("AGENT_MESH_WATCH").as_deref() == Ok("off") {
        return None;
    }
    if let Some(explicit) = std::env::var_os("AGENT_MESH_BIN") {
        return Some(PathBuf::from(explicit));
    }
    let name = if cfg!(windows) { "vvagent.exe" } else { "vvagent" };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// `vvagent`'s arguments for one attempt: dial the peer named by the SSH destination, over an SSH
/// connection of its own that runs `vvagent bridge --serve` on the far side.
pub(super) fn lane_arguments(
    ssh: &OsStr,
    connection: &[OsString],
    platform: RemotePlatform,
    parent: u32,
) -> Vec<OsString> {
    let destination = connection.last().cloned().unwrap_or_default();
    let mut arguments: Vec<OsString> = vec![
        "bridge".into(),
        "--dial".into(),
        destination,
        "--stdin-leash".into(),
        "--parent-pid".into(),
        parent.to_string().into(),
        "--".into(),
        ssh.to_owned(),
        // Its own connection, like the media lanes: no control master, so a dropped mesh lane
        // cannot take the interactive session with it, and keepalives so a dead link is noticed.
        "-T".into(),
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "ControlPath=none".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];
    arguments.extend(connection.iter().cloned());
    arguments.push(remote_command(platform).into());
    arguments
}

/// What the far side runs. A non-interactive SSH command often has a shorter `PATH` than a login
/// shell, so a POSIX host that cannot find `vvagent` directly is asked again through one.
fn remote_command(platform: RemotePlatform) -> &'static str {
    match platform {
        RemotePlatform::Posix => {
            "command -v vvagent >/dev/null 2>&1 && exec vvagent bridge --serve; \
             exec \"${SHELL:-/bin/sh}\" -lc 'exec vvagent bridge --serve'"
        },
        RemotePlatform::Windows => "vvagent bridge --serve",
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RetryPolicy {
    /// First wait after a drop, doubled on each drop in a row up to `longest`.
    pub first: Duration,
    pub longest: Duration,
    /// A session that lasted this long was healthy: the next drop starts from `first` again.
    pub healthy: Duration,
    /// How long to wait when another window's bridge already serves this peer.
    pub standby: Duration,
    /// How long a bridge asked to stop may take to release its leases before it is killed.
    pub shutdown: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(1),
            longest: Duration::from_secs(60),
            healthy: Duration::from_secs(60),
            standby: Duration::from_secs(30),
            shutdown: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Next {
    Retry(Duration),
    GiveUp,
}

/// What the lane does after an attempt ended, and how it remembers it.
#[derive(Clone, Copy, Debug)]
pub(super) struct Backoff {
    wait: Duration,
    connected: bool,
}

impl Backoff {
    pub(super) fn new(policy: &RetryPolicy) -> Self {
        Self { wait: policy.first, connected: false }
    }

    /// Decide from one attempt's exit status and how long it ran.
    ///
    /// - It closed after working (0), or broke mid-session (74): the network or the far side went
    ///   away. Retry with backoff. An unclean drop can leave the far side's lease live for a while,
    ///   during which the far side stands aside and the next attempt closes at once; the backoff is
    ///   what carries the lane across that.
    /// - Another window serves this peer (75): wait longer, and take over if it goes.
    /// - Nothing answered (69): if this session never connected, the far side has no `vvagent` or
    ///   needs a credential nobody has given — retrying will not change that. If it has connected
    ///   before, the host is probably just unreachable for now.
    /// - Refused, or anything else: a pinned label answered by another store, a protocol the two
    ///   ends do not share, or a crash. Give up rather than hammer it.
    pub(super) fn after(
        &mut self,
        status: Option<i32>,
        ran: Duration,
        policy: &RetryPolicy,
    ) -> Next {
        match status {
            Some(CLOSED | BROKEN) => {
                self.connected = true;
                if ran >= policy.healthy {
                    self.wait = policy.first;
                }
                self.step(policy)
            },
            Some(STANDBY) => Next::Retry(policy.standby),
            Some(UNREACHABLE) if self.connected => self.step(policy),
            _ => Next::GiveUp,
        }
    }

    fn step(&mut self, policy: &RetryPolicy) -> Next {
        let wait = self.wait;
        self.wait = (self.wait * 2).min(policy.longest);
        Next::Retry(wait)
    }
}

/// A running lane. Stopping it closes the bridge's stdin, which ends it cleanly.
pub(super) struct Lane {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<u32>>,
}

impl Lane {
    /// Start the real lane for this session.
    pub(super) fn start_vvagent(
        vvagent: PathBuf,
        arguments: Vec<OsString>,
        credentials: UnattendedCredentials,
    ) -> Self {
        Self::start(
            move |attempt| {
                let mut command = Command::new(&vvagent);
                command
                    .args(&arguments)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                credentials.apply(&mut command, "agent-mesh", attempt);
                command.spawn()
            },
            RetryPolicy::default(),
        )
    }

    /// Supervise whatever `spawn` starts, one attempt at a time, until told to stop or until an
    /// attempt ends in a way retrying cannot fix.
    pub(super) fn start(
        mut spawn: impl FnMut(u32) -> std::io::Result<Child> + Send + 'static,
        policy: RetryPolicy,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::Builder::new()
            .name("vvssh-agent-mesh".into())
            .spawn(move || {
                let mut backoff = Backoff::new(&policy);
                let mut attempts = 0;
                while !stopping.load(Ordering::SeqCst) {
                    attempts += 1;
                    let Ok(mut child) = spawn(attempts) else {
                        return attempts;
                    };
                    let started = Instant::now();
                    let status = loop {
                        if stopping.load(Ordering::SeqCst) {
                            end(&mut child, policy.shutdown);
                            return attempts;
                        }
                        match child.try_wait() {
                            Ok(Some(status)) => break status.code(),
                            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                            Err(_) => {
                                end(&mut child, policy.shutdown);
                                return attempts;
                            },
                        }
                    };
                    match backoff.after(status, started.elapsed(), &policy) {
                        Next::GiveUp => return attempts,
                        Next::Retry(wait) => {
                            let until = Instant::now() + wait;
                            while Instant::now() < until && !stopping.load(Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        },
                    }
                }
                attempts
            })
            .ok();
        Self { stop, worker }
    }

    /// Stop, and report how many attempts were made.
    pub(super) fn stop(mut self) -> u32 {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().and_then(|worker| worker.join().ok()).unwrap_or(0)
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Close the bridge's stdin so it leaves cleanly and releases both leases; kill it only if it has
/// not gone within `grace`.
fn end(child: &mut Child, grace: Duration) {
    drop(child.stdin.take());
    let deadline = Instant::now() + grace;
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick() -> RetryPolicy {
        RetryPolicy {
            first: Duration::from_millis(10),
            longest: Duration::from_millis(40),
            healthy: Duration::from_secs(60),
            standby: Duration::from_millis(30),
            shutdown: Duration::from_millis(300),
        }
    }

    #[test]
    fn the_lane_dials_the_destination_over_its_own_connection_and_carries_no_secret() {
        let connection: Vec<OsString> =
            vec!["-p".into(), "2222".into(), "-J".into(), "bastion".into(), "user@buildbox".into()];
        let arguments = lane_arguments(OsStr::new("ssh"), &connection, RemotePlatform::Posix, 4242);
        let text: Vec<String> =
            arguments.iter().map(|argument| argument.to_string_lossy().into_owned()).collect();
        let split = text.iter().position(|argument| argument == "--").unwrap();
        assert_eq!(
            &text[..split],
            ["bridge", "--dial", "user@buildbox", "--stdin-leash", "--parent-pid", "4242"]
        );
        let carrier = &text[split + 1..];
        assert_eq!(carrier[0], "ssh");
        assert!(carrier.windows(2).any(|pair| pair == ["-o", "ControlMaster=no"]));
        assert!(carrier.windows(2).any(|pair| pair == ["-o", "ControlPath=none"]));
        assert!(carrier.contains(&"-T".to_owned()));
        // The user's own connection options, in order, then the far side's command.
        let options = &carrier[carrier.len() - 6..carrier.len() - 1];
        assert_eq!(options, ["-p", "2222", "-J", "bastion", "user@buildbox"]);
        assert!(carrier.last().unwrap().contains("vvagent bridge --serve"));
        for argument in &text {
            assert!(
                !argument.contains("VIVID_ROOT_SECRET") && !argument.contains(".secret"),
                "{argument}"
            );
        }

        let windows = lane_arguments(OsStr::new("ssh"), &connection, RemotePlatform::Windows, 1);
        assert_eq!(windows.last().unwrap(), "vvagent bridge --serve");
    }

    #[test]
    fn drops_are_retried_with_backoff_and_hopeless_ends_are_not() {
        let policy = quick();
        let quickly = Duration::from_millis(1);

        // Never connected and nothing answered: no vvagent over there. One attempt, no more.
        let mut fresh = Backoff::new(&policy);
        assert_eq!(fresh.after(Some(UNREACHABLE), quickly, &policy), Next::GiveUp);

        // Worked, then dropped: back off 10, 20, 40, 40 ms …
        let mut dropping = Backoff::new(&policy);
        let waits: Vec<_> =
            (0..4).map(|_| dropping.after(Some(CLOSED), quickly, &policy)).collect();
        assert_eq!(
            waits,
            [
                Next::Retry(Duration::from_millis(10)),
                Next::Retry(Duration::from_millis(20)),
                Next::Retry(Duration::from_millis(40)),
                Next::Retry(Duration::from_millis(40)),
            ]
        );
        // … and once it has connected, an unreachable host is worth trying again.
        assert!(matches!(dropping.after(Some(UNREACHABLE), quickly, &policy), Next::Retry(_)));
        // A healthy session resets the backoff.
        assert_eq!(
            dropping.after(Some(BROKEN), Duration::from_secs(61), &policy),
            Next::Retry(Duration::from_millis(10))
        );

        // Another window serves this peer: wait the longer standby interval.
        assert_eq!(fresh.after(Some(STANDBY), quickly, &policy), Next::Retry(policy.standby));

        // Refused, or killed by a signal: stop.
        for status in [Some(1), Some(2), None] {
            assert_eq!(Backoff::new(&policy).after(status, quickly, &policy), Next::GiveUp);
        }
    }

    #[cfg(unix)]
    fn scripted(codes: &'static [i32]) -> impl FnMut(u32) -> std::io::Result<Child> + Send {
        move |attempt| {
            let code = codes.get(attempt as usize - 1).copied().unwrap_or(1);
            Command::new("sh").arg("-c").arg(format!("exit {code}")).stdin(Stdio::piped()).spawn()
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_lane_keeps_reconnecting_until_the_end_is_hopeless() {
        // Standby, a drop, a broken carrier, then unreachable (after having connected), then a
        // refusal: five attempts, and the lane stops by itself.
        let lane = Lane::start(scripted(&[STANDBY, CLOSED, BROKEN, UNREACHABLE, 1]), quick());
        let deadline = Instant::now() + Duration::from_secs(10);
        while lane.worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            assert!(Instant::now() < deadline, "the lane should have given up");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(lane.stop(), 5);

        // A host without vvagent is tried once.
        let lane = Lane::start(scripted(&[UNREACHABLE]), quick());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(lane.stop(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn stopping_closes_the_bridges_stdin_and_only_then_kills() {
        use std::sync::Mutex;

        // A bridge that leaves when its stdin closes, recording that it did.
        let marker = std::env::temp_dir().join(format!(
            "vvssh-mesh-stop-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&marker);
        let path = Arc::new(Mutex::new(marker.clone()));
        let lane = Lane::start(
            move |_| {
                let marker = path.lock().unwrap().clone();
                Command::new("sh")
                    .arg("-c")
                    .arg(format!("cat >/dev/null; touch '{}'", marker.display()))
                    .stdin(Stdio::piped())
                    .spawn()
            },
            quick(),
        );
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        assert_eq!(lane.stop(), 1);
        assert!(marker.exists(), "the bridge saw end of file and left on its own");
        assert!(started.elapsed() < Duration::from_millis(300), "and was not left to the kill");
        let _ = std::fs::remove_file(&marker);

        // One that ignores its stdin is killed once the grace runs out.
        let lane =
            Lane::start(|_| Command::new("sleep").arg("30").stdin(Stdio::piped()).spawn(), quick());
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        lane.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
