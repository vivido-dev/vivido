//! Launch choices offered by the tab strip's launch menu.
//!
//! Presentation lives in [`super::menu`]; this module only answers *what* the current platform can
//! start. Keeping the two apart means a future source of entries — configured profiles, remote
//! targets — is added here alone.

use std::path::PathBuf;

use crate::cli::TerminalOptions;
use crate::config::ui_config::Program;

/// Shell and optional starting directory selected from the tab menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabProgram {
    /// Executable and arguments for the new tab.
    pub program: Program,
    /// Native starting directory, or `None` to inherit the tab's directory.
    pub working_directory: Option<PathBuf>,
}

impl TabProgram {
    /// Apply the menu selection to the new tab's inherited options.
    pub fn apply_to(&self, options: &mut TerminalOptions) {
        options.set_command(&self.program);
        if let Some(directory) = &self.working_directory {
            options.working_directory = Some(directory.clone());
        }
    }
}

/// What choosing a launch menu entry does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchAction {
    /// Open a tab; `None` runs the configured shell, as a plain `+` click does.
    NewTab(Option<TabProgram>),
    /// Open another top-level Vivido window.
    NewWindow,
}

/// One row of the launch menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchEntry {
    /// User-visible label.
    pub label: String,
    /// Action to execute.
    pub action: LaunchAction,
}

impl LaunchEntry {
    fn new(label: impl Into<String>, action: LaunchAction) -> Self {
        Self { label: label.into(), action }
    }
}

/// Entries the launch menu offers on this platform.
///
/// Probing costs a process spawn on Windows, so callers build this once and keep the result rather
/// than rebuilding it per menu open.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn entries() -> Vec<LaunchEntry> {
    vec![
        LaunchEntry::new("New Tab", LaunchAction::NewTab(None)),
        LaunchEntry::new("New Window", LaunchAction::NewWindow),
    ]
}

/// Entries the launch menu offers on this platform.
///
/// PowerShell and every installed WSL distribution, each as its own tab. Probing costs a process
/// spawn, so callers build this once and keep the result rather than rebuilding it per menu open.
#[cfg(windows)]
pub fn entries() -> Vec<LaunchEntry> {
    let mut entries = vec![powershell_entry(executable_on_path("pwsh.exe"), std::env::home_dir())];
    entries.extend(wsl_entries());
    entries
}

/// PowerShell 7 when it is installed, otherwise the Windows PowerShell which ships with the OS.
#[cfg(any(windows, test))]
fn powershell_entry(has_pwsh: bool, home: Option<PathBuf>) -> LaunchEntry {
    let (label, executable) = if has_pwsh {
        ("PowerShell", "pwsh.exe")
    } else {
        ("Windows PowerShell", "powershell.exe")
    };
    LaunchEntry::new(
        label,
        LaunchAction::NewTab(Some(TabProgram {
            program: Program::Just(executable.to_owned()),
            working_directory: home,
        })),
    )
}

#[cfg(windows)]
fn executable_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|directory| directory.join(name).is_file())
}

/// One row per installed WSL distribution, or nothing when WSL is not available.
#[cfg(windows)]
fn wsl_entries() -> Vec<LaunchEntry> {
    let Some(output) = wsl_distribution_output() else { return Vec::new() };
    let distributions = parse_wsl_distributions(&output);
    if distributions.is_empty() {
        // WSL answered but named nothing; the default distribution is still launchable.
        return vec![wsl_entry(None)];
    }
    distributions.into_iter().map(|distribution| wsl_entry(Some(distribution))).collect()
}

#[cfg(any(windows, test))]
fn wsl_entry(distribution: Option<String>) -> LaunchEntry {
    let mut args = vec!["--cd".to_owned(), "~".to_owned()];
    let label = if let Some(distribution) = distribution {
        let label = format!("WSL: {distribution}");
        args.extend(["-d".to_owned(), distribution]);
        label
    } else {
        "WSL".to_owned()
    };
    LaunchEntry::new(
        label,
        LaunchAction::NewTab(Some(TabProgram {
            program: Program::WithArgs { program: "wsl.exe".to_owned(), args },
            working_directory: None,
        })),
    )
}

#[cfg(windows)]
fn wsl_distribution_output() -> Option<Vec<u8>> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let output = Command::new("wsl.exe")
        .args(["-l", "-q"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// Distribution names from `wsl.exe -l -q` output.
///
/// WSL writes its listings as UTF-16LE rather than the console code page
/// (microsoft/WSL#4456), so the bytes are decoded before they are split into lines.
#[cfg(any(windows, test))]
fn parse_wsl_distributions(stdout: &[u8]) -> Vec<String> {
    decode_console_output(stdout)
        .lines()
        .map(|line| line.trim_matches(|character: char| character.is_whitespace()).to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Decode console output which may be UTF-16LE or UTF-8.
#[cfg(any(windows, test))]
fn decode_console_output(stdout: &[u8]) -> String {
    let utf16 = stdout.len().is_multiple_of(2)
        && !stdout.is_empty()
        && stdout.iter().skip(1).step_by(2).any(|byte| *byte == 0);
    if !utf16 {
        return String::from_utf8_lossy(stdout).into_owned();
    }
    let units = stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(|unit| unit.to_le_bytes()).collect()
    }

    #[test]
    fn utf16_distribution_listings_are_decoded_and_trimmed() {
        let listing = utf16le("Ubuntu-24.04\r\nDebian\r\n\r\n");

        assert_eq!(parse_wsl_distributions(&listing), ["Ubuntu-24.04", "Debian"]);
    }

    #[test]
    fn a_single_distribution_and_utf8_output_both_parse() {
        assert_eq!(parse_wsl_distributions(&utf16le("Ubuntu\r\n")), ["Ubuntu"]);
        assert_eq!(parse_wsl_distributions(b"Ubuntu\nDebian\n"), ["Ubuntu", "Debian"]);
    }

    #[test]
    fn empty_or_blank_listings_name_no_distribution() {
        assert!(parse_wsl_distributions(b"").is_empty());
        assert!(parse_wsl_distributions(&utf16le("\r\n \r\n")).is_empty());
    }

    #[test]
    fn powershell_menu_starts_both_editions_in_the_windows_home() {
        let home = PathBuf::from(r"C:\Users\Demo User");
        for (has_pwsh, executable) in [(true, "pwsh.exe"), (false, "powershell.exe")] {
            let entry = powershell_entry(has_pwsh, Some(home.clone()));
            let LaunchAction::NewTab(Some(selection)) = entry.action else {
                panic!("expected a PowerShell tab");
            };
            let mut options = TerminalOptions::default();
            options.working_directory = Some(PathBuf::from(r"C:\Program Files\Vivido"));
            selection.apply_to(&mut options);
            assert_eq!(options.working_directory.as_ref(), Some(&home));
            let program = options.command().unwrap();
            assert_eq!(program.program(), executable);
            assert!(program.args().is_empty());
        }
    }

    #[test]
    fn wsl_menu_opens_the_selected_distributions_home() {
        for (distribution, label, args) in [
            (None, "WSL", vec!["--cd", "~"]),
            (Some("My Ubuntu"), "WSL: My Ubuntu", vec!["--cd", "~", "-d", "My Ubuntu"]),
        ] {
            let entry = wsl_entry(distribution.map(str::to_owned));
            assert_eq!(entry.label, label);
            let LaunchAction::NewTab(Some(selection)) = entry.action else {
                panic!("expected a WSL tab");
            };
            let mut options = TerminalOptions::default();
            selection.apply_to(&mut options);
            let program = options.command().unwrap();
            assert_eq!(program.program(), "wsl.exe");
            assert_eq!(program.args(), args.as_slice());
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix_desktop_offers_a_tab_and_a_window() {
        let entries = entries();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].action, LaunchAction::NewTab(None));
        assert_eq!(entries[1].action, LaunchAction::NewWindow);
    }
}
