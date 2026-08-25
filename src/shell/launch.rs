//! Launch choices offered by the tab strip's `+` menu.
//!
//! Presentation lives in [`super::menu`]; this module only answers *what* the current platform can
//! start. Keeping the two apart means a future source of entries — configured profiles, remote
//! targets — is added here alone.

use crate::config::ui_config::Program;

/// What choosing a `+` menu entry does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchAction {
    /// Open a tab; `None` runs the configured shell, as a plain `+` click does.
    NewTab(Option<Program>),
    /// Open another top-level Vivido window.
    NewWindow,
}

/// One row of the `+` menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchEntry {
    pub label: String,
    pub action: LaunchAction,
}

impl LaunchEntry {
    fn new(label: impl Into<String>, action: LaunchAction) -> Self {
        Self { label: label.into(), action }
    }
}

/// Entries the `+` menu offers on this platform.
///
/// Probing costs a process spawn on Windows, so callers build this once and keep the result rather
/// than rebuilding it per menu open.
#[cfg(target_os = "linux")]
pub fn entries() -> Vec<LaunchEntry> {
    vec![
        LaunchEntry::new("New Tab", LaunchAction::NewTab(None)),
        LaunchEntry::new("New Window", LaunchAction::NewWindow),
    ]
}

/// Entries the `+` menu offers on this platform.
///
/// PowerShell and every installed WSL distribution, each as its own tab. Probing costs a process
/// spawn, so callers build this once and keep the result rather than rebuilding it per menu open.
#[cfg(windows)]
pub fn entries() -> Vec<LaunchEntry> {
    let mut entries = vec![powershell_entry()];
    entries.extend(wsl_entries());
    entries
}

/// PowerShell 7 when it is installed, otherwise the Windows PowerShell which ships with the OS.
///
/// `powershell.exe` is also Vivido's default Windows shell, so the fallback row launches exactly
/// what a plain `+` click would.
#[cfg(windows)]
fn powershell_entry() -> LaunchEntry {
    if executable_on_path("pwsh.exe") {
        LaunchEntry::new(
            "PowerShell",
            LaunchAction::NewTab(Some(Program::Just("pwsh.exe".to_owned()))),
        )
    } else {
        LaunchEntry::new(
            "Windows PowerShell",
            LaunchAction::NewTab(Some(Program::Just("powershell.exe".to_owned()))),
        )
    }
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
        return vec![LaunchEntry::new(
            "WSL",
            LaunchAction::NewTab(Some(Program::Just("wsl.exe".to_owned()))),
        )];
    }
    distributions
        .into_iter()
        .map(|distribution| {
            LaunchEntry::new(
                format!("WSL: {distribution}"),
                LaunchAction::NewTab(Some(Program::WithArgs {
                    program: "wsl.exe".to_owned(),
                    args: vec!["-d".to_owned(), distribution],
                })),
            )
        })
        .collect()
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
        .chunks_exact(2)
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

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_offers_a_tab_and_a_window() {
        let entries = entries();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].action, LaunchAction::NewTab(None));
        assert_eq!(entries[1].action, LaunchAction::NewWindow);
    }
}
