//! Automatic shell integration for bash, zsh, and fish.
//!
//! The scripts under `extra/shell-integration` report the prompt and command lifecycle with
//! OSC 133 and the working directory with OSC 7. Vivido loads them without touching any of the
//! user's startup files: bash starts in POSIX mode with `ENV` naming Vivido's script, which then
//! reads the usual startup files itself; zsh reads Vivido's `.zshenv` through `ZDOTDIR`, which
//! puts `ZDOTDIR` back; fish finds a `vendor_conf.d` file through `XDG_DATA_DIRS`, which restores
//! the variable. Each script undoes its variable before the user's configuration runs, so the
//! programs a shell starts see the environment they would have seen anyway.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::terminal::tty::IntegratedShell;

/// Names the directory holding the scripts, for shells that load them by hand.
pub const DIRECTORY_ENV: &str = "VIVIDO_SHELL_INTEGRATION_DIR";

const BASH_SCRIPT: &str = "bash/vivido.bash";
const ZSH_DIRECTORY: &str = "zsh";
const FISH_SCRIPT: &str = "fish/vendor_conf.d/vivido-shell-integration.fish";

const FILES: [(&str, &str); 4] = [
    (BASH_SCRIPT, include_str!("../../../extra/shell-integration/bash/vivido.bash")),
    ("zsh/.zshenv", include_str!("../../../extra/shell-integration/zsh/.zshenv")),
    (
        "zsh/vivido-integration.zsh",
        include_str!("../../../extra/shell-integration/zsh/vivido-integration.zsh"),
    ),
    (
        FISH_SCRIPT,
        include_str!(
            "../../../extra/shell-integration/fish/vendor_conf.d/vivido-shell-integration.fish"
        ),
    ),
];

/// Variables that only carry an injection from Vivido to a script. An inherited one would make a
/// shell Vivido did not inject replay its startup files or restore a stale value.
const PRIVATE_ENV: [&str; 7] = [
    "VIVIDO_BASH_INJECT",
    "VIVIDO_BASH_ENV",
    "VIVIDO_BASH_RCFILE",
    "VIVIDO_BASH_HISTFILE",
    "VIVIDO_ZSH_ZDOTDIR",
    "VIVIDO_FISH_INJECT",
    "VIVIDO_FISH_XDG_DATA_DIRS",
];

/// How to start one shell with Vivido's integration loaded.
#[derive(Debug, PartialEq, Eq)]
pub struct Injection {
    pub shell: IntegratedShell,
    /// Arguments that replace the shell's own.
    pub args: Vec<String>,
    /// Variables set in the shell's environment, over everything else.
    pub env: Vec<(&'static str, String)>,
}

/// Write the scripts to a new private directory, which lives as long as the returned value.
pub fn provision() -> io::Result<TempDir> {
    // Owner-only regardless of the umask, which tempfile would otherwise apply; the scripts
    // inside are then out of everyone else's reach whatever their own modes.
    let directory = tempfile::Builder::new()
        .prefix("vivido-shell-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()?;
    for (path, contents) in FILES {
        let path = directory.path().join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    Ok(directory)
}

/// The directory [`provision`] published for this process, if any.
pub fn directory() -> Option<PathBuf> {
    std::env::var_os(DIRECTORY_ENV).map(PathBuf::from)
}

/// Variables to clear from every shell's environment before an [`Injection`] sets its own.
pub fn private_env() -> impl Iterator<Item = &'static str> {
    PRIVATE_ENV.into_iter()
}

/// Plan how to start `program` with `args` so that it loads the integration in `directory`.
///
/// `lookup` reads the environment the shell would otherwise start with. Returns `None` for
/// programs other than bash, zsh, and fish, for arguments that make the shell a script runner or
/// skip its startup files, and when `directory` is missing or writable by anyone but this user,
/// since a shell would run whatever it found there.
pub fn inject(
    program: &str,
    args: &[String],
    directory: &Path,
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<Injection> {
    if !is_private(directory) {
        return None;
    }
    match Path::new(program).file_name()?.to_str()? {
        "bash" => bash(program, args, directory, &lookup),
        "zsh" => zsh(args, directory, &lookup),
        "fish" => fish(args, directory, &lookup),
        _ => None,
    }
}

/// Start bash in POSIX mode, where an interactive shell reads `ENV` and no other startup file.
fn bash(
    program: &str,
    args: &[String],
    directory: &Path,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Option<Injection> {
    // Apple's bash 3.2 does not read ENV in POSIX mode, and nothing else is installed as
    // /bin/bash on macOS.
    if cfg!(target_os = "macos") && program == "/bin/bash" {
        return None;
    }
    // bash expands ENV before reading it.
    let script = script(directory, BASH_SCRIPT)?;
    if script.contains(['$', '`', '\\']) {
        return None;
    }

    let mut kept = vec![String::from("--posix")];
    let mut flags = String::from("1");
    let mut rcfile = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Already POSIX, which the script would undo.
            "--posix" => return None,
            // POSIX mode reads no startup files anyway; the script honors these.
            "--norc" | "--noprofile" => {
                flags.push(' ');
                flags.push_str(arg);
            },
            "--rcfile" | "--init-file" => rcfile = Some(args.next()?.clone()),
            "-o" | "+o" | "-O" | "+O" => {
                let option = args.next()?;
                if option == "posix" {
                    return None;
                }
                kept.extend([arg.clone(), option.clone()]);
            },
            // Everything after these is a script and its arguments.
            "-" | "--" => {
                if !args.as_slice().is_empty() {
                    return None;
                }
                kept.push(arg.clone());
            },
            long if long.starts_with("--") => kept.push(arg.clone()),
            // A command string never makes an interactive shell.
            short if short.len() > 1 && short.starts_with(['-', '+']) => {
                if short[1..].contains('c') {
                    return None;
                }
                kept.push(arg.clone());
            },
            // A script to run.
            _ => return None,
        }
    }

    let mut env = vec![("ENV", script), ("VIVIDO_BASH_INJECT", flags)];
    if let Some(previous) = lookup("ENV") {
        env.push(("VIVIDO_BASH_ENV", previous));
    }
    if let Some(rcfile) = rcfile {
        env.push(("VIVIDO_BASH_RCFILE", rcfile));
    }
    if lookup("HISTFILE").is_none() {
        env.push(("VIVIDO_BASH_HISTFILE", String::from("1")));
    }
    Some(Injection { shell: IntegratedShell::Bash, args: kept, env })
}

/// Point `ZDOTDIR` at Vivido's `.zshenv`, which zsh reads first and which restores `ZDOTDIR`.
fn zsh(
    args: &[String],
    directory: &Path,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Option<Injection> {
    // Each of these skips .zshenv, which would leave ZDOTDIR pointing here.
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let skips_zshenv = match arg.as_str() {
            "--emulate" => true,
            "-o" => args.next().is_some_and(|option| is_zsh_option(option, "norcs")),
            long if long.starts_with("--") => is_zsh_option(&long[2..], "norcs"),
            short if short.len() > 1 && short.starts_with('-') => short[1..].contains('f'),
            _ => false,
        };
        if skips_zshenv {
            return None;
        }
    }

    let zdotdir = script(directory, ZSH_DIRECTORY)?;
    let mut env = vec![("ZDOTDIR", zdotdir)];
    if let Some(previous) = lookup("ZDOTDIR") {
        env.push(("VIVIDO_ZSH_ZDOTDIR", previous));
    }
    Some(Injection { shell: IntegratedShell::Zsh, args: Vec::new(), env })
}

/// Whether a zsh option name, which ignores case and underscores, is the lowercase `name`.
fn is_zsh_option(option: &str, name: &str) -> bool {
    option
        .chars()
        .filter(|&char| char != '_' && char != '-')
        .map(|char| char.to_ascii_lowercase())
        .eq(name.chars())
}

/// Put Vivido's directory first in `XDG_DATA_DIRS`, whose `fish/vendor_conf.d` fish sources.
fn fish(
    args: &[String],
    directory: &Path,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Option<Injection> {
    // Without configuration fish never runs the file that restores XDG_DATA_DIRS.
    let no_config = args.iter().any(|arg| match arg.as_str() {
        "--no-config" => true,
        short if short.len() > 1 && !short.starts_with("--") && short.starts_with('-') => {
            short[1..].contains('N')
        },
        _ => false,
    });
    if no_config {
        return None;
    }

    script(directory, FISH_SCRIPT)?;
    let directory = directory.to_str().filter(|directory| !directory.contains(':'))?;
    let previous = lookup("XDG_DATA_DIRS");
    let data_dirs = match previous.as_deref() {
        Some(previous) if !previous.is_empty() => format!("{directory}:{previous}"),
        _ => directory.to_owned(),
    };
    let mut env = vec![("XDG_DATA_DIRS", data_dirs), ("VIVIDO_FISH_INJECT", String::from("1"))];
    if let Some(previous) = previous {
        env.push(("VIVIDO_FISH_XDG_DATA_DIRS", previous));
    }
    Some(Injection { shell: IntegratedShell::Fish, args: args.to_vec(), env })
}

/// The path of an installed script as UTF-8, if it is there.
fn script(directory: &Path, relative: &str) -> Option<String> {
    let path = directory.join(relative);
    path.exists().then(|| path.to_str().map(str::to_owned)).flatten()
}

/// Whether `directory` is a real directory that only this user can write to.
fn is_private(directory: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(directory) else { return false };
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let user = unsafe { libc::geteuid() };
    metadata.is_dir() && metadata.uid() == user && metadata.mode() & 0o022 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn plan(program: &str, args: &[&str], env: &[(&str, &str)]) -> Option<Injection> {
        let directory = provision().unwrap();
        let env: HashMap<_, _> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let injection =
            inject(program, &strings(args), directory.path(), |name| env.get(name).cloned());
        injection.map(|mut injection| {
            // Paths inside the temporary directory differ from run to run.
            let root = directory.path().to_str().unwrap();
            for (_, value) in &mut injection.env {
                *value = value.replace(root, "<dir>");
            }
            injection
        })
    }

    fn env_of(injection: &Injection) -> HashMap<&'static str, &str> {
        injection.env.iter().map(|(name, value)| (*name, value.as_str())).collect()
    }

    #[test]
    fn provisioned_scripts_match_the_sources_in_a_private_directory() {
        let directory = provision().unwrap();
        for (path, contents) in FILES {
            assert_eq!(fs::read_to_string(directory.path().join(path)).unwrap(), contents);
        }
        assert!(is_private(directory.path()));
        let root = directory.path().to_owned();
        drop(directory);
        assert!(!root.exists());
    }

    #[test]
    fn a_directory_others_can_write_to_is_not_used() {
        let directory = provision().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(inject("zsh", &[], directory.path(), |_| None), None);
        assert_eq!(inject("zsh", &[], &directory.path().join("missing"), |_| None), None);
    }

    #[test]
    fn bash_starts_in_posix_mode_with_env_naming_the_script() {
        let injection = plan("/usr/bin/bash", &[], &[]).unwrap();
        assert_eq!(injection.shell, IntegratedShell::Bash);
        assert_eq!(injection.args, ["--posix"]);
        let env = env_of(&injection);
        assert_eq!(env["ENV"], "<dir>/bash/vivido.bash");
        assert_eq!(env["VIVIDO_BASH_INJECT"], "1");
        // POSIX mode would otherwise read and write ~/.sh_history.
        assert_eq!(env["VIVIDO_BASH_HISTFILE"], "1");
        assert!(!env.contains_key("VIVIDO_BASH_ENV"));
    }

    #[test]
    fn bash_saves_env_and_leaves_an_explicit_histfile_alone() {
        let injection =
            plan("bash", &[], &[("ENV", "/home/me/.shinit"), ("HISTFILE", "/h")]).unwrap();
        let env = env_of(&injection);
        assert_eq!(env["VIVIDO_BASH_ENV"], "/home/me/.shinit");
        assert!(!env.contains_key("VIVIDO_BASH_HISTFILE"));
    }

    #[test]
    fn bash_hands_startup_flags_to_the_script_and_keeps_the_rest() {
        let injection = plan(
            "bash",
            &["-l", "--norc", "--noprofile", "--rcfile", "/r", "-o", "vi", "--noediting"],
            &[],
        )
        .unwrap();
        assert_eq!(injection.args, ["--posix", "-l", "-o", "vi", "--noediting"]);
        let env = env_of(&injection);
        assert_eq!(env["VIVIDO_BASH_INJECT"], "1 --norc --noprofile");
        assert_eq!(env["VIVIDO_BASH_RCFILE"], "/r");
    }

    #[test]
    fn bash_scripts_commands_and_posix_shells_are_left_alone() {
        for args in [
            &["-c", "echo"][..],
            &["-lc", "echo"],
            &["script.sh"],
            &["--", "script.sh"],
            &["--posix"],
            &["-o", "posix"],
            &["--rcfile"],
        ] {
            assert_eq!(plan("bash", args, &[]), None, "{args:?}");
        }
        if cfg!(target_os = "macos") {
            assert_eq!(plan("/bin/bash", &[], &[]), None);
        }
    }

    #[test]
    fn zsh_reads_the_integration_zdotdir_and_keeps_the_users() {
        let injection = plan("/bin/zsh", &["-l"], &[("ZDOTDIR", "/home/me/.config/zsh")]).unwrap();
        assert_eq!(injection.shell, IntegratedShell::Zsh);
        assert_eq!(injection.args, Vec::<String>::new());
        let env = env_of(&injection);
        assert_eq!(env["ZDOTDIR"], "<dir>/zsh");
        assert_eq!(env["VIVIDO_ZSH_ZDOTDIR"], "/home/me/.config/zsh");

        let injection = plan("zsh", &[], &[]).unwrap();
        assert!(!env_of(&injection).contains_key("VIVIDO_ZSH_ZDOTDIR"));
    }

    #[test]
    fn zsh_skipping_its_startup_files_is_left_alone() {
        for args in [&["-f"][..], &["-if"], &["--no-rcs"], &["--NO_RCS"], &["-o", "no_rcs"]] {
            assert_eq!(plan("zsh", args, &[]), None, "{args:?}");
        }
        assert_eq!(plan("zsh", &["--emulate", "sh"], &[]), None);
    }

    #[test]
    fn fish_finds_the_script_first_in_xdg_data_dirs() {
        let injection = plan("fish", &["-l"], &[("XDG_DATA_DIRS", "/usr/share")]).unwrap();
        assert_eq!(injection.shell, IntegratedShell::Fish);
        assert_eq!(injection.args, ["-l"]);
        let env = env_of(&injection);
        assert_eq!(env["XDG_DATA_DIRS"], "<dir>:/usr/share");
        assert_eq!(env["VIVIDO_FISH_XDG_DATA_DIRS"], "/usr/share");
        assert_eq!(env["VIVIDO_FISH_INJECT"], "1");

        // An unset XDG_DATA_DIRS stays unset for fish's children.
        let env = plan("fish", &[], &[]).unwrap().env;
        assert!(env.iter().all(|(name, _)| *name != "VIVIDO_FISH_XDG_DATA_DIRS"));
        assert!(env.contains(&("XDG_DATA_DIRS", String::from("<dir>"))));

        for args in [&["-N"][..], &["--no-config"], &["-iN"]] {
            assert_eq!(plan("fish", args, &[]), None, "{args:?}");
        }
    }

    #[test]
    fn other_programs_are_left_alone() {
        for program in ["sh", "dash", "nu", "/usr/bin/vim", "bash-5.2", ""] {
            assert_eq!(plan(program, &[], &[]), None, "{program}");
        }
    }
}
