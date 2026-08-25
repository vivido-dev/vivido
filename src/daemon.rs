#[cfg(target_os = "openbsd")]
use std::ffi::CStr;
#[cfg(not(windows))]
use std::ffi::CString;
use std::ffi::OsStr;
#[cfg(not(any(target_os = "macos", target_os = "openbsd", windows)))]
use std::fs;
use std::io;
#[cfg(not(windows))]
use std::os::unix::ffi::OsStringExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
#[cfg(target_os = "openbsd")]
use std::ptr;
use std::sync::OnceLock;

#[rustfmt::skip]
#[cfg(not(windows))]
use {
    std::error::Error,
    std::os::unix::process::CommandExt,
    std::os::unix::io::RawFd,
    std::path::PathBuf,
};

#[cfg(not(windows))]
use libc::pid_t;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

#[cfg(target_os = "macos")]
use crate::macos;

/// Start a new process in the background.
#[cfg(windows)]
pub fn spawn_daemon<I, S>(program: &str, args: I) -> io::Result<()>
where
    I: IntoIterator<Item = S> + Copy,
    S: AsRef<OsStr>,
{
    // Setting all the I/O handles to null and setting the
    // CREATE_NEW_PROCESS_GROUP and CREATE_NO_WINDOW has the effect
    // that console applications will run without opening a new
    // console window.
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

/// Start a new process in the background.
///
/// `working_directory` is the directory the daemon starts in; callers which have a PTY pass the
/// foreground process' directory, and callers which do not — the tab chrome — pass whatever they
/// know, or `None` to inherit this process' directory.
#[cfg(not(windows))]
pub fn spawn_daemon<I, S>(
    program: &str,
    args: I,
    working_directory: Option<PathBuf>,
) -> io::Result<()>
where
    I: IntoIterator<Item = S> + Copy,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let working_directory =
        working_directory.and_then(|path| CString::new(path.into_os_string().into_vec()).ok());

    unsafe {
        command
            .pre_exec(move || {
                // POSIX.1-2017 describes `fork` as async-signal-safe with the following note:
                //
                // > While the fork() function is async-signal-safe, there is no way for
                // > an implementation to determine whether the fork handlers established by
                // > pthread_atfork() are async-signal-safe. [...] It is therefore undefined for the
                // > fork handlers to execute functions that are not async-signal-safe when fork()
                // > is called from a signal handler.
                //
                // POSIX.1-2024 removes this guarantee and introduces an async-signal-safe
                // replacement `_Fork`, which we'd like to use, but macOS doesn't support it yet.
                //
                // Since we aren't registering any fork handlers, and hopefully the OS doesn't
                // either, we're fine on systems compatible with POSIX.1-2017, which should be
                // enough for a long while. If this ever becomes a problem in the future, we should
                // be able to switch to `_Fork`.
                match libc::fork() {
                    -1 => return Err(io::Error::last_os_error()),
                    0 => (),
                    _ => libc::_exit(0),
                }

                // Copy foreground process' working directory, ignoring invalid paths.
                if let Some(working_directory) = working_directory.as_ref() {
                    libc::chdir(working_directory.as_ptr());
                }

                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }

                Ok(())
            })
            .spawn()?
            .wait()
            .map(|_| ())
    }
}

/// Arguments for relaunching this executable as another top-level Vivido window.
///
/// A new window starts a fresh session, so it must not inherit the command the current one was
/// asked to run, nor — off Windows, where [`spawn_daemon`] sets the directory itself — the working
/// directory. `args` is this process' argv with the program name already removed.
pub fn relaunch_arguments<I>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut relaunch = Vec::new();
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        if argument == "-e" || argument == "--command" {
            // `--command` takes every remaining argument, so nothing after it can be kept.
            break;
        }

        #[cfg(not(windows))]
        if argument == "--working-directory" {
            let _ = args.next();
            continue;
        }

        relaunch.push(argument);
    }
    relaunch
}

/// Get working directory of controlling process.
#[cfg(not(any(windows, target_os = "openbsd")))]
pub fn foreground_process_path(
    master_fd: RawFd,
    shell_pid: u32,
) -> Result<PathBuf, Box<dyn Error>> {
    let mut pid = unsafe { libc::tcgetpgrp(master_fd) };
    if pid < 0 {
        pid = shell_pid as pid_t;
    }

    #[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
    let link_path = format!("/proc/{pid}/cwd");
    #[cfg(target_os = "freebsd")]
    let link_path = format!("/compat/linux/proc/{}/cwd", pid);

    #[cfg(not(target_os = "macos"))]
    let cwd = fs::read_link(link_path)?;

    #[cfg(target_os = "macos")]
    let cwd = macos::proc::cwd(pid)?;

    Ok(cwd)
}

#[cfg(target_os = "openbsd")]
pub fn foreground_process_path(
    master_fd: RawFd,
    shell_pid: u32,
) -> Result<PathBuf, Box<dyn Error>> {
    let mut pid = unsafe { libc::tcgetpgrp(master_fd) };
    if pid < 0 {
        pid = shell_pid as pid_t;
    }
    let name = [libc::CTL_KERN, libc::KERN_PROC_CWD, pid];
    let mut buf = [0u8; libc::PATH_MAX as usize];
    let result = unsafe {
        libc::sysctl(
            name.as_ptr(),
            name.len().try_into().unwrap(),
            buf.as_mut_ptr() as *mut _,
            &mut buf.len() as *mut _,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        Err(io::Error::last_os_error().into())
    } else {
        let foreground_path = unsafe { CStr::from_ptr(buf.as_ptr().cast()) }.to_str()?;
        Ok(PathBuf::from(foreground_path))
    }
}

/// Whether an OSC 7 working-directory report's host refers to this machine.
pub(crate) fn is_local_host(host: &str) -> bool {
    host_matches(host, local_hostname())
}

/// Empty hosts and `localhost` always match; any other host must equal the local name.
fn host_matches(host: &str, local: &str) -> bool {
    host.is_empty() || host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case(local)
}

/// This machine's hostname, cached for the process lifetime; empty when unavailable.
fn local_hostname() -> &'static str {
    static HOSTNAME: OnceLock<String> = OnceLock::new();
    HOSTNAME.get_or_init(probe_hostname)
}

#[cfg(not(windows))]
fn probe_hostname() -> String {
    let mut buffer = [0u8; 256];
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return String::new();
    }
    // The buffer is not guaranteed to be NUL-terminated when the name is truncated.
    let end = buffer.iter().position(|&byte| byte == 0).unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

#[cfg(windows)]
fn probe_hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{host_matches, relaunch_arguments};

    fn relaunch(args: &[&str]) -> Vec<String> {
        relaunch_arguments(args.iter().map(|argument| (*argument).to_owned()))
    }

    #[test]
    fn a_relaunch_keeps_the_window_arguments_it_was_started_with() {
        assert_eq!(
            relaunch(&["--title", "Vivido", "-o", "window.blur=true"]),
            ["--title", "Vivido", "-o", "window.blur=true"]
        );
    }

    #[test]
    fn a_relaunch_never_inherits_the_command() {
        assert_eq!(
            relaunch(&["--title", "Vivido", "-e", "htop", "--sort", "cpu"]),
            ["--title", "Vivido"]
        );
        assert_eq!(relaunch(&["--command", "htop"]), Vec::<String>::new());
    }

    #[test]
    #[cfg(not(windows))]
    fn a_unix_relaunch_drops_the_working_directory_the_daemon_sets_itself() {
        assert_eq!(
            relaunch(&["--working-directory", "/tmp", "--title", "Vivido"]),
            ["--title", "Vivido"]
        );
    }

    #[test]
    fn local_host_forms_match() {
        assert!(host_matches("", "machine"));
        assert!(host_matches("localhost", "machine"));
        assert!(host_matches("LOCALHOST", "machine"));
        assert!(host_matches("machine", "machine"));
        assert!(host_matches("MACHINE", "machine"));
    }

    #[test]
    fn foreign_hosts_do_not_match() {
        assert!(!host_matches("remote", "machine"));
        assert!(!host_matches("machine-2", "machine"));
        // With no local hostname only the implicit-local forms match.
        assert!(!host_matches("remote", ""));
        assert!(host_matches("", ""));
    }
}
