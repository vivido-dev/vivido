//! TTY related functionality.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::sync::Arc;
use std::{env, io};

use base64::Engine;
use log::warn;
use polling::{Event, PollMode, Poller};
use tempfile::TempDir;

#[cfg(not(windows))]
pub mod shell_integration;
#[cfg(not(windows))]
mod unix;
#[cfg(not(windows))]
pub use self::unix::*;

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use self::windows::*;

/// Configuration for the `Pty` interface.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Options {
    /// Shell options.
    ///
    /// [`None`] will use the default shell.
    pub shell: Option<Shell>,

    /// Shell startup directory.
    pub working_directory: Option<PathBuf>,

    /// Drain the child process output before exiting the terminal.
    pub drain_on_exit: bool,

    /// Extra environment variables.
    pub env: HashMap<String, String>,

    /// Start a supported shell with Vivido's integration loaded, so it reports its prompts,
    /// commands, and working directory.
    pub shell_integration: bool,

    /// Specifies whether the Windows shell arguments should be escaped.
    ///
    /// - When `true`: Arguments will be escaped according to the standard C runtime rules.
    /// - When `false`: Arguments will be passed raw without additional escaping.
    #[cfg(target_os = "windows")]
    pub escape_args: bool,
}

/// Shell options.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Shell {
    /// Path to a shell program to run on startup.
    pub(crate) program: String,
    /// Arguments passed to shell.
    pub(crate) args: Vec<String>,
}

impl Shell {
    /// Configure a child shell executable and its arguments.
    pub fn new(program: String, args: Vec<String>) -> Self {
        Self { program, args }
    }
}

/// A shell that Vivido started with its integration loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegratedShell {
    /// Bash startup integration.
    Bash,
    /// Zsh startup integration.
    Zsh,
    /// Fish startup integration.
    Fish,
    /// PowerShell startup integration.
    PowerShell,
}

impl IntegratedShell {
    /// The name automation reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::PowerShell => "powershell",
        }
    }
}

/// Stream read and/or write behavior.
///
/// This defines an abstraction over polling's interface in order to allow either
/// one read/write object or a separate read and write object.
pub trait EventedReadWrite {
    /// Concrete reader type supplied by this implementation.
    type Reader: io::Read;
    /// Concrete writer type supplied by this implementation.
    type Writer: io::Write;

    /// # Safety
    ///
    /// The underlying sources must outlive their registration in the `Poller`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the resource cannot be registered with the operating system poller.
    unsafe fn register(&mut self, _: &Arc<Poller>, _: Event, _: PollMode) -> io::Result<()>;
    /// Update interest in an already registered PTY transport.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the poller cannot update this resource’s interest.
    fn reregister(&mut self, _: &Arc<Poller>, _: Event, _: PollMode) -> io::Result<()>;
    /// Remove this PTY transport from its polling loop.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the poller cannot remove this resource.
    fn deregister(&mut self, _: &Arc<Poller>) -> io::Result<()>;

    /// Borrow the readable side of the PTY transport.
    fn reader(&mut self) -> &mut Self::Reader;
    /// Borrow the writable side of the PTY transport.
    fn writer(&mut self) -> &mut Self::Writer;
}

/// Events concerning TTY child processes.
#[derive(Debug, PartialEq, Eq)]
pub enum ChildEvent {
    /// Indicates the child has exited.
    Exited(Option<ExitStatus>),
}

/// A pseudoterminal (or PTY).
///
/// This is a refinement of EventedReadWrite that also provides a channel through which we can be
/// notified if the PTY child process does something we care about (other than writing to the TTY).
/// In particular, this allows for race-free child exit notification on UNIX (cf. `SIGCHLD`).
pub trait EventedPty: EventedReadWrite {
    /// Tries to retrieve an event.
    ///
    /// Returns `Some(event)` on success, or `None` if there are no events to retrieve.
    fn next_child_event(&mut self) -> Option<ChildEvent>;
}

const TERMINFO_NAME: &str = "vivido";
// Regenerate from `extra/vivido.info` with `term_dir=$(mktemp -d)`, `tic -x -e vivido -o
// "$term_dir" extra/vivido.info`, and `base64` on the generated `vivido` entry. Depending on the
// host ncurses build, `tic` may put the entry below `v/` or hexadecimal `76/`.
const BUNDLED_TERMINFO: &str = include_str!("../../../extra/vivido.terminfo.b64");

/// Owns child-environment defaults and their temporary terminfo and shell resources.
///
/// Keep this guard alive until every child using its environment has exited. Provisioning
/// never mutates the process environment and is safe after other threads have started.
#[must_use]
pub struct TerminfoGuard {
    _directory: Option<TempDir>,
    #[cfg(not(windows))]
    _shell_integration: Option<TempDir>,
    variables: HashMap<&'static str, Option<OsString>>,
}

impl TerminfoGuard {
    /// Apply terminal defaults to one child before applying user overrides.
    pub fn apply(&self, command: &mut Command) {
        for (name, value) in &self.variables {
            match value {
                Some(value) => {
                    command.env(name, value);
                },
                None => {
                    command.env_remove(name);
                },
            }
        }
    }

    #[cfg(not(windows))]
    fn integration_directory(&self) -> Option<&std::path::Path> {
        self._shell_integration.as_ref().map(TempDir::path)
    }

    #[cfg(windows)]
    fn apply_options(&self, environment: &mut HashMap<String, String>) {
        for (name, value) in &self.variables {
            if let Some(value) = value
                && !environment.keys().any(|key| key.eq_ignore_ascii_case(name))
            {
                environment.insert((*name).to_owned(), value.to_string_lossy().into_owned());
            }
        }
    }
}

/// Provision terminal environment defaults without mutating the process environment.
///
/// PTY constructors apply these defaults automatically. Other child launchers can call
/// [`TerminfoGuard::apply`] before adding their own environment overrides.
pub fn setup_env() -> TerminfoGuard {
    let mut variables = HashMap::new();
    #[cfg(target_os = "macos")]
    if crate::macos::locale::needs_child_locale() {
        let (key, value) = crate::macos::locale::child_locale();
        variables.insert(key, Some(OsString::from(value)));
    }
    // Prefer an entry installed by the user or package manager. Source builds and `cargo install`
    // do not install data files, so materialize the bundled entry when the database has no Vivido
    // definition. User-configured environment variables are applied after this function and can
    // still override TERM, TERMINFO, or COLORTERM.
    let directory = if terminfo_exists(TERMINFO_NAME) {
        None
    } else {
        match provision_bundled_terminfo() {
            Ok(directory) => {
                variables.insert("TERMINFO", Some(directory.path().as_os_str().to_owned()));
                Some(directory)
            },
            Err(error) => {
                warn!("Could not provision bundled Vivido terminfo: {error}");
                None
            },
        }
    };

    let terminfo = if directory.is_some() || terminfo_exists(TERMINFO_NAME) {
        TERMINFO_NAME
    } else {
        "xterm-256color"
    };
    variables.insert("TERM", Some(OsString::from(terminfo)));

    // Advertise 24-bit color support.
    variables.insert("COLORTERM", Some(OsString::from("truecolor")));

    // Shells read the scripts from here, and anyone loading them by hand finds them through the
    // variable. A value inherited from an enclosing Vivido names that instance's copy, so it is
    // replaced, or removed when this instance has none.
    #[cfg(not(windows))]
    let shell_integration = match shell_integration::provision() {
        Ok(directory) => {
            variables.insert(
                shell_integration::DIRECTORY_ENV,
                Some(directory.path().as_os_str().to_owned()),
            );
            Some(directory)
        },
        Err(error) => {
            warn!("Could not provision shell integration scripts: {error}");
            variables.insert(shell_integration::DIRECTORY_ENV, None);
            None
        },
    };

    TerminfoGuard {
        _directory: directory,
        #[cfg(not(windows))]
        _shell_integration: shell_integration,
        variables,
    }
}

/// Materialize the bundled compiled entry in both terminfo directory layouts.
fn provision_bundled_terminfo() -> io::Result<TempDir> {
    let encoded: String = BUNDLED_TERMINFO.split_whitespace().collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let directory = tempfile::Builder::new().prefix("vivido-terminfo-").tempdir()?;
    for entry_directory in [&TERMINFO_NAME[..1], "76"] {
        let entry_directory = directory.path().join(entry_directory);
        fs::create_dir(&entry_directory)?;
        fs::write(entry_directory.join(TERMINFO_NAME), &bytes)?;
    }
    Ok(directory)
}

/// Check if a terminfo entry exists on the system.
fn terminfo_exists(terminfo: &str) -> bool {
    // Get first terminfo character for the parent directory.
    let first = terminfo.get(..1).unwrap_or_default();
    let first_hex = format!("{:x}", first.chars().next().unwrap_or_default() as usize);

    // Return true if the terminfo file exists at the specified location.
    macro_rules! check_path {
        ($path:expr) => {
            if $path.join(first).join(terminfo).exists()
                || $path.join(&first_hex).join(terminfo).exists()
            {
                return true;
            }
        };
    }

    if let Some(dir) = env::var_os("TERMINFO") {
        check_path!(PathBuf::from(&dir));
    }

    if let Some(home) = home::home_dir() {
        check_path!(home.join(".terminfo"));
    }

    if let Ok(dirs) = env::var("TERMINFO_DIRS") {
        for dir in dirs.split(':') {
            check_path!(PathBuf::from(dir));
        }
    }

    if let Ok(prefix) = env::var("PREFIX") {
        let path = PathBuf::from(prefix);
        check_path!(path.join("etc/terminfo"));
        check_path!(path.join("lib/terminfo"));
        check_path!(path.join("share/terminfo"));
    }

    check_path!(PathBuf::from("/etc/terminfo"));
    check_path!(PathBuf::from("/lib/terminfo"));
    check_path!(PathBuf::from("/usr/share/terminfo"));
    check_path!(PathBuf::from("/boot/system/data/terminfo"));

    // No valid terminfo path has been found.
    false
}

// Debug omits user content and native resources, and never acquires application locks.
impl std::fmt::Debug for TerminfoGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminfoGuard")
            .field("provisioned_terminfo", &self._directory.is_some())
            .field("variables", &self.variables.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{BUNDLED_TERMINFO, TERMINFO_NAME, provision_bundled_terminfo};

    use std::fs;

    use base64::Engine;

    #[test]
    fn bundled_terminfo_is_valid_and_lifecycle_scoped() {
        let directory = provision_bundled_terminfo().unwrap();
        let root = directory.path().to_owned();
        let encoded: String = BUNDLED_TERMINFO.split_whitespace().collect();
        let expected = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();

        assert_eq!(fs::read(root.join("v").join(TERMINFO_NAME)).unwrap(), expected);
        assert_eq!(fs::read(root.join("76").join(TERMINFO_NAME)).unwrap(), expected);
        assert!(expected.windows(b"Smulx".len()).any(|window| window == b"Smulx"));
        assert!(expected.windows(b"\x1b[4:%p1%dm".len()).any(|window| window == b"\x1b[4:%p1%dm"));

        drop(directory);
        assert!(!root.exists());
    }
}
