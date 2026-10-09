//! TTY related functionality.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Error, ErrorKind, Read, Result};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, ptr};

use libc::{F_GETFL, F_SETFL, O_NONBLOCK, TIOCSCTTY, c_int, fcntl};
use log::error;
use polling::{Event, PollMode, Poller};
use rustix_openpty::openpty;
use rustix_openpty::rustix::termios::Winsize;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix_openpty::rustix::termios::{self, InputModes, OptionalActions};
use signal_hook::low_level::{pipe as signal_pipe, unregister as unregister_signal};
use signal_hook::{SigId, consts as sigconsts};

use crate::terminal::event::{OnResize, WindowSize};
use crate::terminal::tty::{
    ChildEvent, EventedPty, EventedReadWrite, IntegratedShell, Options, shell_integration,
};

// Interest in PTY read/writes.
pub(crate) const PTY_READ_WRITE_TOKEN: usize = 0;

// Interest in new child events.
pub(crate) const PTY_CHILD_EVENT_TOKEN: usize = 1;

macro_rules! die {
    ($($arg:tt)*) => {{
        error!($($arg)*);
        std::process::exit(1);
    }};
}

/// Really only needed on BSD, but should be fine elsewhere.
fn set_controlling_terminal(fd: c_int) -> Result<()> {
    // SAFETY: fd is the newly opened PTY slave; TIOCSCTTY receives the platform-specific scalar argument.
    let res = unsafe {
        // TIOSCTTY changes based on platform and the `ioctl` call is different
        // based on architecture (32/64). So a generic cast is used to make sure
        // there are no issues. To allow such a generic cast the clippy warning
        // is disabled.
        #[allow(
            clippy::cast_lossless,
            reason = "the native uid type varies across supported Unix targets"
        )]
        libc::ioctl(fd, TIOCSCTTY as _, 0)
    };

    if res == 0 { Ok(()) } else { Err(Error::last_os_error()) }
}

#[derive(Debug)]
struct Passwd<'a> {
    name: &'a str,
    dir: &'a str,
    shell: &'a str,
}

/// Return a Passwd struct with pointers into the provided buf.
///
/// # Unsafety
///
/// If `buf` is changed while `Passwd` is alive, bad thing will almost certainly happen.
fn get_pw_entry(buf: &mut [i8; 1024]) -> Result<Passwd<'_>> {
    // Create zeroed passwd struct.
    let mut entry: MaybeUninit<libc::passwd> = MaybeUninit::uninit();

    let mut res: *mut libc::passwd = ptr::null_mut();

    // Try and read the pw file.
    // SAFETY: getuid has no arguments and returns a scalar identity.
    let uid = unsafe { libc::getuid() };
    // SAFETY: entry, result pointer, and scratch buffer are writable locals with their exact declared sizes.
    let status = unsafe {
        libc::getpwuid_r(uid, entry.as_mut_ptr(), buf.as_mut_ptr() as *mut _, buf.len(), &mut res)
    };

    // `getpwuid_r` reports failure through its return value, not errno, and leaves `entry`
    // uninitialized unless it found the user.
    if status != 0 {
        return Err(Error::from_raw_os_error(status));
    }

    if res.is_null() {
        return Err(Error::other("pw not found"));
    }

    // SAFETY: `getpwuid_r` succeeded and returned a non-null result, so it filled `entry`.
    let entry = unsafe { entry.assume_init() };

    // Sanity check.
    assert_eq!(entry.pw_uid, uid);

    // SAFETY: on success every string field points to a NUL-terminated string inside `buf`,
    // which outlives the returned `Passwd`.
    let field = |pointer| {
        // SAFETY: successful getpwuid_r returned a checked non-null field in the scratch buffer, which outlives this copy.
        unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .map_err(|_| Error::new(ErrorKind::InvalidData, "passwd entry is not UTF-8"))
    };

    // Build a borrowed Passwd struct.
    Ok(Passwd {
        name: field(entry.pw_name)?,
        dir: field(entry.pw_dir)?,
        shell: field(entry.pw_shell)?,
    })
}

/// An owned pseudoterminal, child process, and polling resources.
pub struct Pty {
    child: Child,
    file: File,
    signals: UnixStream,
    sig_id: SigId,
    shell_integration: Option<IntegratedShell>,
    _environment: super::TerminfoGuard,
}

impl Pty {
    /// Borrow the child process owned by this PTY.
    pub fn child(&self) -> &Child {
        &self.child
    }

    /// The shell started with Vivido's integration loaded, if any.
    pub fn shell_integration(&self) -> Option<IntegratedShell> {
        self.shell_integration
    }

    /// Borrow this PTY's master file handle.
    pub fn file(&self) -> &File {
        &self.file
    }
}

/// User information that is required for a new shell session.
struct ShellUser {
    user: String,
    home: String,
    shell: String,
}

impl ShellUser {
    /// look for shell, username, longname, and home dir in the respective environment variables
    /// before falling back on looking into `passwd`.
    fn from_env() -> Result<Self> {
        let user = env::var("USER");
        let home = env::var("HOME");
        let shell = env::var("SHELL");
        match (user, home, shell) {
            (Ok(user), Ok(home), Ok(shell)) => Ok(Self { user, home, shell }),
            (user, home, shell) => {
                let mut buf = [0; 1024];
                let pw = get_pw_entry(&mut buf)?;
                Ok(Self {
                    user: user.unwrap_or_else(|_| pw.name.to_owned()),
                    home: home.unwrap_or_else(|_| pw.dir.to_owned()),
                    shell: shell.unwrap_or_else(|_| pw.shell.to_owned()),
                })
            },
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn default_shell_command(shell: &str, _user: &str, _home: &str, args: &[String]) -> Command {
    let mut command = Command::new(shell);
    command.args(args);
    command
}

#[cfg(target_os = "macos")]
fn default_shell_command(shell: &str, user: &str, home: &str, args: &[String]) -> Command {
    let shell_name = shell.rsplit('/').next().unwrap();

    // On macOS, use the `login` command so the shell will appear as a tty session.
    let mut login_command = Command::new("/usr/bin/login");

    // Exec the shell with argv[0] prepended by '-' so it becomes a login shell.
    // `login` normally does this itself, but `-l` disables this. The only arguments here are
    // shell integration flags such as `--posix`, which need no quoting.
    let mut exec = format!("exec -a -{} {}", shell_name, shell);
    for arg in args {
        exec.push(' ');
        exec.push_str(arg);
    }

    // Since we use -l, `login` will not change directory to the user's home. However,
    // `login` only checks the current working directory for a .hushlogin file, causing
    // it to miss any in the user's home directory. We can fix this by doing the check
    // ourselves and passing `-q`
    let has_home_hushlogin = Path::new(home).join(".hushlogin").exists();

    // `exec -a` needs a shell that supports it; the user's own shell does when it is zsh
    // or bash, otherwise fall back to zsh so the exec line still works.
    let interpreter = login_interpreter(shell);

    // -f: Bypasses authentication for the already-logged-in user.
    // -l: Skips changing directory to $HOME and prepending '-' to argv[0].
    // -p: Preserves the environment.
    // -q: Act as if `.hushlogin` exists.
    let flags = if has_home_hushlogin { "-qflp" } else { "-flp" };
    login_command.args([flags, user, interpreter, "-fc", &exec]);
    login_command
}

/// The interpreter for the macOS `exec -a` login line: the user's own shell when it supports
/// `exec -a`, zsh otherwise.
#[cfg(target_os = "macos")]
fn login_interpreter(shell: &str) -> &str {
    match shell.rsplit('/').next().unwrap() {
        "zsh" | "bash" => shell,
        _ => "/bin/zsh",
    }
}

/// Create a new TTY and return a handle to interact with it.
///
/// # Errors
///
/// Returns an I/O error if PTY allocation, configuration, or child spawning fails.
pub fn new(config: &Options, window_size: WindowSize, window_id: u64) -> Result<Pty> {
    let pty = openpty(None, Some(&window_size.to_winsize()))?;
    let (master, slave) = (pty.controller, pty.user);
    from_fd(config, window_id, master, slave)
}

/// Create a new TTY from a PTY's file descriptors.
///
/// # Errors
///
/// Returns an error if the supplied PTY descriptors cannot be configured or the child cannot start.
pub fn from_fd(config: &Options, window_id: u64, master: OwnedFd, slave: OwnedFd) -> Result<Pty> {
    spawn(config, window_id, master, slave, None)
}

/// Start the shell on `slave`, loading the integration scripts in `integration` when it can.
fn spawn(
    config: &Options,
    window_id: u64,
    master: OwnedFd,
    slave: OwnedFd,
    integration: Option<&Path>,
) -> Result<Pty> {
    let environment = super::setup_env();
    let integration = config
        .shell_integration
        .then(|| integration.or_else(|| environment.integration_directory()))
        .flatten();
    let master_fd = master.as_raw_fd();
    let slave_fd = slave.as_raw_fd();

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Ok(mut termios) = termios::tcgetattr(&master) {
        // Set character encoding to UTF-8.
        termios.input_modes.set(InputModes::IUTF8, true);
        let _ = termios::tcsetattr(&master, OptionalActions::Now, &termios);
    }

    let user = ShellUser::from_env()?;

    let (program, args) = match config.shell.as_ref() {
        Some(shell) => (shell.program.as_str(), shell.args.as_slice()),
        None => (user.shell.as_str(), &[][..]),
    };
    // The integration saves what the shell's environment would otherwise have held.
    let lookup = |name: &str| config.env.get(name).cloned().or_else(|| env::var(name).ok());
    let injection = integration
        .and_then(|directory| shell_integration::inject(program, args, directory, lookup));
    let args = injection.as_ref().map_or(args, |injection| injection.args.as_slice());

    let mut builder = if config.shell.is_some() {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    } else {
        default_shell_command(&user.shell, &user.user, &user.home, args)
    };

    // Setup child stdin/stdout/stderr as slave fd of PTY.
    builder.stdin(slave.try_clone()?);
    builder.stderr(slave.try_clone()?);
    builder.stdout(slave);

    // Setup shell environment.
    environment.apply(&mut builder);
    let window_id = window_id.to_string();
    builder.env("VIVIDO_WINDOW_ID", &window_id);
    builder.env("USER", user.user);
    builder.env("HOME", user.home);
    for (key, value) in &config.env {
        builder.env(key, value);
    }
    for name in shell_integration::private_env() {
        builder.env_remove(name);
    }
    if let Some(injection) = &injection {
        for (name, value) in &injection.env {
            builder.env(name, value);
        }
    }

    // Prevent child processes from inheriting linux-specific startup notification env.
    builder.env_remove("XDG_ACTIVATION_TOKEN");
    builder.env_remove("DESKTOP_STARTUP_ID");

    let working_directory = config
        .working_directory
        .as_ref()
        .and_then(|path| CString::new(path.as_os_str().as_bytes()).ok());

    // SAFETY: pre_exec uses only async-signal-safe libc calls and prebuilt data, without allocation or locks.
    unsafe {
        builder.pre_exec(move || {
            // Create a new process group.
            let err = libc::setsid();
            if err == -1 {
                return Err(Error::last_os_error());
            }

            // Set working directory, ignoring invalid paths.
            if let Some(working_directory) = working_directory.as_ref() {
                libc::chdir(working_directory.as_ptr());
            }

            set_controlling_terminal(slave_fd)?;

            // No longer need slave/master fds.
            libc::close(slave_fd);
            libc::close(master_fd);

            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGALRM, libc::SIG_DFL);

            Ok(())
        });
    }

    // Prepare signal handling before spawning child.
    let (signals, sig_id) = {
        let (sender, recv) = UnixStream::pair()?;

        // Register the recv end of the pipe for SIGCHLD.
        let sig_id = signal_pipe::register(sigconsts::SIGCHLD, sender)?;
        recv.set_nonblocking(true)?;
        (recv, sig_id)
    };

    match builder.spawn() {
        Ok(child) => {
            // SAFETY: the child owns the slave and this Pty owns the live master descriptor passed to fcntl.
            unsafe {
                // Maybe this should be done outside of this function so nonblocking
                // isn't forced upon consumers. Although maybe it should be?
                set_nonblocking(master_fd)?;
            }

            Ok(Pty {
                child,
                file: File::from(master),
                signals,
                sig_id,
                shell_integration: injection.map(|injection| injection.shell),
                _environment: environment,
            })
        },
        Err(err) => Err(Error::new(
            err.kind(),
            format!(
                "Failed to spawn command '{}': {}",
                builder.get_program().to_string_lossy(),
                err
            ),
        )),
    }
}

/// Grace period for the PTY child to exit after SIGHUP before SIGKILL escalation.
const GRACEFUL_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Grace period for the PTY child to be reaped after SIGKILL before giving up.
const FORCEFUL_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Poll interval while waiting for the PTY child to exit.
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Poll `try_wait` until `timeout` elapses; returns true when the child was reaped.
fn reap_child(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) => std::thread::sleep(REAP_POLL_INTERVAL),
        }
    }
    matches!(child.try_wait(), Ok(Some(_)) | Err(_))
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Make sure the PTY is terminated properly.
        // SAFETY: kill accepts the scalar child PID and SIGHUP; no Rust references cross the call.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGHUP);
        }

        // Clear signal-hook handler.
        unregister_signal(self.sig_id);

        // Reap with a bound, then escalate to SIGKILL: on macOS the login-wrapped
        // shell can wedge uninterruptibly in concurrent session teardown, and
        // shutdown must never hang on a child that outlives SIGHUP. A child that
        // even SIGKILL cannot reap is left for init; exit proceeds regardless.
        if reap_child(&mut self.child, GRACEFUL_REAP_TIMEOUT) {
            return;
        }
        let _ = self.child.kill();
        reap_child(&mut self.child, FORCEFUL_REAP_TIMEOUT);
    }
}

impl EventedReadWrite for Pty {
    type Reader = File;
    type Writer = File;

    #[inline]
    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        poll_opts: PollMode,
    ) -> Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        // SAFETY: this Pty owns file until deregistration; its event-loop owner outlives the poll registration.
        unsafe {
            poll.add_with_mode(&self.file, interest, poll_opts)?;
        }

        // SAFETY: this Pty owns signals until deregistration; its event-loop owner outlives the poll registration.
        unsafe {
            poll.add_with_mode(
                &self.signals,
                Event::readable(PTY_CHILD_EVENT_TOKEN),
                PollMode::Level,
            )
        }
    }

    #[inline]
    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        poll_opts: PollMode,
    ) -> Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        poll.modify_with_mode(&self.file, interest, poll_opts)?;

        poll.modify_with_mode(
            &self.signals,
            Event::readable(PTY_CHILD_EVENT_TOKEN),
            PollMode::Level,
        )
    }

    #[inline]
    fn deregister(&mut self, poll: &Arc<Poller>) -> Result<()> {
        poll.delete(&self.file)?;
        poll.delete(&self.signals)
    }

    #[inline]
    fn reader(&mut self) -> &mut File {
        &mut self.file
    }

    #[inline]
    fn writer(&mut self) -> &mut File {
        &mut self.file
    }
}

impl EventedPty for Pty {
    #[inline]
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        // See if there has been a SIGCHLD.
        let mut buf = [0u8; 1];
        if let Err(err) = self.signals.read(&mut buf) {
            if err.kind() != ErrorKind::WouldBlock {
                error!("Error reading from signal pipe: {err}");
            }
            return None;
        }

        // Match on the child process.
        match self.child.try_wait() {
            Err(err) => {
                error!("Error checking child process termination: {err}");
                None
            },
            Ok(None) => None,
            Ok(exit_status) => Some(ChildEvent::Exited(exit_status)),
        }
    }
}

impl OnResize for Pty {
    /// Resize the PTY.
    ///
    /// Tells the kernel that the window size changed with the new pixel
    /// dimensions and line/column counts.
    fn on_resize(&mut self, window_size: WindowSize) {
        let win = window_size.to_winsize();

        // SAFETY: win is a correctly aligned initialized winsize that remains live throughout ioctl.
        let res = unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCSWINSZ, &win as *const _) };

        if res < 0 {
            die!("ioctl TIOCSWINSZ failed: {}", Error::last_os_error());
        }
    }
}

/// Types that can produce a `Winsize`.
pub trait ToWinsize {
    /// Get a `Winsize`.
    fn to_winsize(self) -> Winsize;
}

impl ToWinsize for WindowSize {
    fn to_winsize(self) -> Winsize {
        let ws_row = self.num_lines;
        let ws_col = self.num_cols;

        // Pixel dimensions are advisory 16-bit fields; clamp instead of wrapping wide terminals.
        let ws_xpixel = ws_col.saturating_mul(self.cell_width);
        let ws_ypixel = ws_row.saturating_mul(self.cell_height);
        Winsize { ws_row, ws_col, ws_xpixel, ws_ypixel }
    }
}

unsafe fn set_nonblocking(fd: c_int) -> Result<()> {
    // SAFETY: the caller keeps fd open for these scalar fcntl operations; no borrowed memory is passed.
    let res = unsafe { fcntl(fd, F_SETFL, fcntl(fd, F_GETFL, 0) | O_NONBLOCK) };
    if res == 0 { Ok(()) } else { Err(Error::last_os_error()) }
}

#[test]
fn test_get_pw_entry() {
    let mut buf: [i8; 1024] = [0; 1024];
    let _pw = get_pw_entry(&mut buf).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn login_interpreter_runs_compatible_shells_themselves() {
    assert_eq!(login_interpreter("/bin/zsh"), "/bin/zsh");
    assert_eq!(login_interpreter("/bin/bash"), "/bin/bash");
    assert_eq!(login_interpreter("/usr/local/bin/fish"), "/bin/zsh");
    assert_eq!(login_interpreter("/bin/sh"), "/bin/zsh");
}

#[test]
fn pty_drop_reaps_hup_ignoring_child_without_hanging() {
    use std::io::Read;

    use crate::terminal::tty::Shell;

    // Arrange: a child that survives SIGHUP (`trap ''` persists across exec).
    let options = Options {
        shell: Some(Shell::new(
            String::from("/bin/sh"),
            vec![String::from("-c"), String::from("trap '' HUP; echo READY; exec sleep 30")],
        )),
        ..Default::default()
    };
    let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
    let pty = new(&options, size, 0).expect("spawn HUP-ignoring shell");
    let pid = pty.child().id();

    // Wait for READY so the trap is installed before SIGHUP can race it.
    let mut output = Vec::new();
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    let mut reader = pty.file().try_clone().expect("clone pty master");
    while !output.windows(b"READY".len()).any(|window| window == b"READY") {
        assert!(Instant::now() < ready_deadline, "child never became ready");
        let mut chunk = [0u8; 64];
        match reader.read(&mut chunk) {
            Ok(0) => panic!("child exited before becoming ready"),
            Ok(n) => output.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            },
            Err(error) => panic!("pty read failed: {error}"),
        }
    }

    // Act: dropping the PTY must not block on the surviving child.
    let start = Instant::now();
    drop(pty);
    let elapsed = start.elapsed();

    // Assert: waited out the graceful timeout, then SIGKILL escalation reaped it —
    // well before the 30s sleep. The lower bound proves the test really exercised
    // the escalation path instead of winning a spawn race.
    assert!(
        elapsed >= Duration::from_millis(1500),
        "child died before escalation could be tested: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(15), "pty drop hung: {elapsed:?}");
    assert_process_gone(pid);
}

/// Poll `kill(pid, 0)` until the process is gone; fails after a bounded wait.
#[cfg(test)]
fn assert_process_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // SAFETY: signal 0 performs only existence/permission checks.
        let gone = unsafe { libc::kill(pid as libc::pid_t, 0) } != 0;
        if gone {
            return;
        }
        assert!(Instant::now() < deadline, "child {pid} survived pty drop");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn login_command_passes_integration_flags_to_the_shell() {
    let command = default_shell_command(
        "/opt/homebrew/bin/bash",
        "me",
        "/nonexistent",
        &[String::from("--posix")],
    );
    let exec = command.get_args().last().unwrap();
    assert_eq!(exec, "exec -a -bash /opt/homebrew/bin/bash --posix");
}

/// Real shells started through `spawn` with the integration loaded, read back with the parser
/// the terminal uses. Each test skips when its shell is not installed.
// Debug omits user content and native resources, and never acquires application locks.
impl std::fmt::Debug for Pty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pty").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod shell_integration_tests {
    use std::collections::HashMap;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use rustix_openpty::openpty;

    use super::spawn;
    use crate::osc_notification::{OscMessage, OscNotificationParser, ShellIntegrationMarker};
    use crate::terminal::tty::{IntegratedShell, Options, Shell, shell_integration};

    use ShellIntegrationMarker::{CommandFinished, CommandOutputStart, InputStart, PromptStart};

    const PROMPT: &str = "READY> ";

    #[derive(Debug, PartialEq)]
    enum Report {
        Marker(ShellIntegrationMarker),
        Directory(String),
    }

    fn find(name: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
    }

    /// A home directory holding `files`, plus the directory `a b` to change into.
    fn home(files: &[(&str, &str)]) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("a b")).unwrap();
        for (path, contents) in files {
            let path = home.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        home
    }

    /// Start `program` in `home`, send each line once a prompt has been drawn for it, and return
    /// what the shell reported along with its output.
    fn run(
        program: &Path,
        home: &Path,
        env: &[(&str, &str)],
        lines: &[&str],
    ) -> (Option<IntegratedShell>, String, Vec<Report>) {
        let scripts = shell_integration::provision().unwrap();
        let mut env: HashMap<String, String> =
            env.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect();
        env.insert("HOME".into(), home.to_str().unwrap().into());
        env.insert("TERM".into(), "xterm-256color".into());
        let options = Options {
            shell: Some(Shell::new(program.to_str().unwrap().into(), Vec::new())),
            working_directory: Some(home.to_owned()),
            env,
            shell_integration: true,
            ..Options::default()
        };
        let pty = openpty(None, None).unwrap();
        let pty = spawn(&options, 0, pty.controller, pty.user, Some(scripts.path())).unwrap();
        let shell = pty.shell_integration();
        let mut file = pty.file().try_clone().unwrap();

        let mut output = Vec::new();
        let mut sent = 0;
        let mut answered = 0;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let text = String::from_utf8_lossy(&output).into_owned();
            assert!(Instant::now() < deadline, "shell stalled; output so far: {text:?}");
            // Answer the device-attribute and cursor queries a shell may wait on.
            let queries = ["\x1b[c", "\x1b[0c", "\x1b[6n"]
                .iter()
                .map(|query| text.matches(query).count())
                .sum::<usize>();
            for _ in answered..queries {
                file.write_all(b"\x1b[?62c\x1b[1;1R").unwrap();
            }
            answered = queries;
            if text.matches(PROMPT).count() > sent {
                let Some(line) = lines.get(sent) else { break };
                file.write_all(format!("{line}\r").as_bytes()).unwrap();
                sent += 1;
            }

            let mut chunk = [0u8; 4096];
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => output.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                },
                Err(_) => break,
            }
        }
        drop(pty);

        let reports = OscNotificationParser::default()
            .advance(&output)
            .into_iter()
            .filter_map(|message| match message {
                OscMessage::ShellIntegration(marker) => Some(Report::Marker(marker)),
                OscMessage::WorkingDirectory(directory) => Some(Report::Directory(directory.path)),
                _ => None,
            })
            .collect();
        (shell, String::from_utf8_lossy(&output).into_owned(), reports)
    }

    /// Whether `expected` appears in order within `reports`, other reports allowed between.
    fn in_order(reports: &[Report], expected: &[Report]) -> bool {
        let mut reports = reports.iter();
        expected.iter().all(|wanted| reports.any(|report| report == wanted))
    }

    fn marker(marker: ShellIntegrationMarker) -> Report {
        Report::Marker(marker)
    }

    /// The directory `a b` inside `home` as the shell's logical `$PWD` names it after
    /// `cd "$HOME/a b"`, without resolving symlinks such as macOS's /var.
    fn changed_directory(home: &Path) -> Report {
        Report::Directory(home.join("a b").to_str().unwrap().to_owned())
    }

    #[test]
    fn bash_loads_the_users_startup_files_and_reports_the_lifecycle() {
        // Apple's /bin/bash is too old, and a bash older than 4.4 reports nothing.
        let Some(bash) = find("bash").filter(|bash| {
            !(cfg!(target_os = "macos") && bash == Path::new("/bin/bash"))
                && Command::new(bash)
                    .args(["-c", "((BASH_VERSINFO[0] * 100 + BASH_VERSINFO[1] >= 404))"])
                    .status()
                    .is_ok_and(|status| status.success())
        }) else {
            eprintln!("skipped: no bash 4.4 or newer on PATH");
            return;
        };
        // Strict users and their own prompt hooks must keep working.
        let home = home(&[(
            ".bashrc",
            "set -u\nPS1='READY> '\nVIVIDO_TEST_RC=loaded\nPROMPT_COMMAND='echo \"[hook saw $?]\"'\n",
        )]);
        let (shell, output, reports) = run(
            &bash,
            home.path(),
            &[("ENV", "/test/env")],
            &[
                "cd \"$HOME/a b\" && echo \"rc=$VIVIDO_TEST_RC env=[$ENV] \
                 inject=[${VIVIDO_BASH_INJECT-}] $(shopt -oq posix && echo posix)\"; (exit 3)",
                "",
            ],
        );

        assert_eq!(shell, Some(IntegratedShell::Bash));
        // The script restored ENV and left POSIX mode before reading ~/.bashrc.
        assert!(output.contains("rc=loaded env=[/test/env] inject=[] \r\n"), "{output:?}");
        // The user's hook ran before Vivido's and still saw the command's status.
        assert!(output.contains("[hook saw 3]"), "{output:?}");
        assert!(
            in_order(
                &reports,
                &[
                    marker(PromptStart),
                    marker(InputStart),
                    marker(CommandOutputStart),
                    marker(CommandFinished { exit_code: Some(3) }),
                    changed_directory(home.path()),
                    marker(PromptStart),
                    marker(InputStart),
                ],
            ),
            "{reports:?}"
        );
        // An empty line runs nothing, so it finishes nothing.
        let finishes = reports
            .iter()
            .filter(|report| matches!(report, Report::Marker(CommandFinished { .. })))
            .count();
        assert_eq!(finishes, 1, "{reports:?}");
    }

    #[test]
    fn zsh_loads_the_users_startup_files_and_reports_the_lifecycle() {
        let Some(zsh) = find("zsh") else {
            eprintln!("skipped: no zsh on PATH");
            return;
        };
        let home = home(&[
            ("zdot/.zshenv", "VIVIDO_TEST_ENV=loaded\n"),
            ("zdot/.zshrc", "PROMPT='READY> '\nVIVIDO_TEST_RC=loaded\n"),
        ]);
        let zdotdir = home.path().join("zdot");
        let (shell, output, reports) = run(
            &zsh,
            home.path(),
            &[("ZDOTDIR", zdotdir.to_str().unwrap())],
            &[
                "cd \"$HOME/a b\" && echo \"rc=$VIVIDO_TEST_ENV,$VIVIDO_TEST_RC \
                 zdotdir=[${ZDOTDIR:t}] saved=[${VIVIDO_ZSH_ZDOTDIR-}]\"; (exit 3)",
                "",
            ],
        );

        assert_eq!(shell, Some(IntegratedShell::Zsh));
        // Vivido's .zshenv put ZDOTDIR back before zsh read the user's files from it.
        assert!(output.contains("rc=loaded,loaded zdotdir=[zdot] saved=[]"), "{output:?}");
        assert!(
            in_order(
                &reports,
                &[
                    marker(PromptStart),
                    marker(InputStart),
                    marker(CommandOutputStart),
                    changed_directory(home.path()),
                    marker(CommandFinished { exit_code: Some(3) }),
                    marker(PromptStart),
                    marker(InputStart),
                ],
            ),
            "{reports:?}"
        );
        let finishes = reports
            .iter()
            .filter(|report| matches!(report, Report::Marker(CommandFinished { .. })))
            .count();
        assert_eq!(finishes, 1, "{reports:?}");
    }

    #[test]
    fn fish_reports_the_lifecycle_and_restores_xdg_data_dirs() {
        let Some(fish) = find("fish") else {
            eprintln!("skipped: no fish on PATH");
            return;
        };
        let home = home(&[(
            ".config/fish/config.fish",
            "function fish_prompt; echo -n 'READY> '; end\nset -g VIVIDO_TEST_RC loaded\n",
        )]);
        let config = home.path().join(".config");
        let (shell, output, reports) = run(
            &fish,
            home.path(),
            &[("XDG_CONFIG_HOME", config.to_str().unwrap()), ("XDG_DATA_DIRS", "/test/share")],
            &["cd \"$HOME/a b\"; and echo \"rc=$VIVIDO_TEST_RC xdg=[$XDG_DATA_DIRS] \
                 inject=[$VIVIDO_FISH_INJECT]\"; false"],
        );

        assert_eq!(shell, Some(IntegratedShell::Fish));
        assert!(output.contains("rc=loaded xdg=[/test/share] inject=[]"), "{output:?}");
        // fish 4 reports these itself, and fish 3 through Vivido's script.
        assert!(
            in_order(
                &reports,
                &[
                    marker(PromptStart),
                    marker(CommandOutputStart),
                    changed_directory(home.path()),
                    marker(CommandFinished { exit_code: Some(1) }),
                    marker(PromptStart),
                ],
            ),
            "{reports:?}"
        );
    }
}
