use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};

const DEFAULT_CONFIG: &str = include_str!("../../default-vivido.toml");

fn main() {
    let command = env::args_os().nth(1).unwrap_or_default();
    let result = match command.to_string_lossy().as_ref() {
        "config-init" => initialize_config(),
        "vvmux-check" => ensure_no_vvmux_sessions(),
        "wsl-detect" => detect_wsl_distribution().and_then(|installed| {
            installed.then_some(()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "WSL has no installed distribution")
            })
        }),
        "wsl-install" => install_wsl(),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected config-init, vvmux-check, wsl-detect, or wsl-install",
        )),
    };

    match result {
        Ok(()) => {},
        Err(error) if error.raw_os_error() == Some(3010) => process::exit(3010),
        Err(error) => {
            eprintln!("vivido-windows-setup: {error}");
            process::exit(1);
        },
    }
}

fn initialize_config() -> io::Result<()> {
    let user_profile = required_env_path("USERPROFILE")?;
    let app_data = required_env_path("APPDATA")?;
    initialize_config_at(&user_profile, &app_data)
}

fn initialize_config_at(user_profile: &Path, app_data: &Path) -> io::Result<()> {
    let config_dir = user_profile.join("vivido");
    let config_path = config_dir.join("vivido.toml");
    let legacy_dir = app_data.join("vivido");
    let legacy_path = legacy_dir.join("vivido.toml");

    reject_reparse_point(user_profile)?;
    reject_reparse_point(app_data)?;
    if config_dir.exists() {
        reject_reparse_point(&config_dir)?;
    } else {
        fs::create_dir(&config_dir)?;
    }
    if config_path.exists() {
        reject_reparse_point(&config_path)?;
        return Ok(());
    }

    if legacy_dir.exists() {
        reject_reparse_point(&legacy_dir)?;
    }
    let contents = match fs::read(&legacy_path) {
        Ok(contents) => {
            reject_reparse_point(&legacy_path)?;
            contents
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => DEFAULT_CONFIG.as_bytes().to_vec(),
        Err(error) => return Err(error),
    };

    let mut destination = OpenOptions::new().write(true).create_new(true).open(config_path)?;
    destination.write_all(&contents)?;
    destination.sync_all()
}

fn ensure_no_vvmux_sessions() -> io::Result<()> {
    let executable = env::current_exe()?;
    let vvmux = executable
        .parent()
        .and_then(Path::parent)
        .map(|directory| directory.join("vvmux.exe"))
        .ok_or_else(|| io::Error::other("setup helper has no installation parent"))?;
    if !vvmux.is_file() {
        return Ok(());
    }

    let output = Command::new(vvmux).arg("list").output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            "unable to verify vvmux sessions; uninstall or upgrade was refused",
        ));
    }
    if !output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Err(io::Error::other(
            "live vvmux sessions exist; run `vvmux list` and `vvmux kill-session -t NAME` first",
        ));
    }

    Ok(())
}

fn install_wsl() -> io::Result<()> {
    if detect_wsl_distribution()? {
        return Ok(());
    }

    let output = Command::new(system_executable("wsl.exe")?)
        .args(["--install", "--distribution", "Ubuntu", "--no-launch"])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        let detail = bounded_console_output(&output.stderr, &output.stdout);
        return Err(io::Error::other(format!(
            "wsl --install failed with {}: {detail}",
            output.status
        )));
    }
    if detect_wsl_distribution().unwrap_or(false) {
        return Ok(());
    }

    // Enabling the WSL and virtualization optional features can require a reboot. On bundle
    // resume this helper detects Ubuntu and returns 0.
    Err(io::Error::from_raw_os_error(3010))
}

fn detect_wsl_distribution() -> io::Result<bool> {
    let output = match Command::new(system_executable("wsl.exe")?)
        .args(["--list", "--quiet"])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !output.status.success() {
        return Ok(false);
    }

    Ok(!decode_console_output(&output.stdout).trim_matches(['\0', '\r', '\n', ' ']).is_empty())
}

fn decode_console_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.len().is_multiple_of(2) {
        let nul_count = bytes.iter().skip(1).step_by(2).filter(|byte| **byte == 0).count();
        if nul_count > bytes.len() / 8 {
            let utf16 = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            return String::from_utf16_lossy(&utf16);
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn bounded_console_output(primary: &[u8], fallback: &[u8]) -> String {
    const MAX_ERROR_BYTES: usize = 4096;
    let bytes = if primary.is_empty() { fallback } else { primary };
    let truncated = &bytes[..bytes.len().min(MAX_ERROR_BYTES)];
    let message = decode_console_output(truncated).trim_matches(['\0', '\r', '\n', ' ']).to_owned();
    if message.is_empty() { "no diagnostic output".to_owned() } else { message }
}

fn system_executable(name: &str) -> io::Result<PathBuf> {
    let system_root = required_env_path("SystemRoot")?;
    let executable = system_root.join("System32").join(name);
    if !executable.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("system executable not found: {}", executable.display()),
        ));
    }
    Ok(executable)
}

fn required_env_path(name: &str) -> io::Result<PathBuf> {
    let value: OsString = env::var_os(name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{name} is not defined")))?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is not an absolute path"),
        ));
    }
    Ok(path)
}

fn reject_reparse_point(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || has_windows_reparse_attribute(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing reparse point: {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn has_windows_reparse_attribute(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn has_windows_reparse_attribute(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn decodes_utf8_console_output() {
        assert_eq!(decode_console_output(b"Ubuntu\r\n"), "Ubuntu\r\n");
    }

    #[test]
    fn decodes_utf16_console_output() {
        let bytes = "Ubuntu\r\n".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        assert_eq!(decode_console_output(&bytes), "Ubuntu\r\n");
    }

    #[test]
    fn diagnostic_output_is_bounded_and_prefers_stderr() {
        assert_eq!(bounded_console_output(b"policy blocked\r\n", b"fallback"), "policy blocked");
        assert_eq!(bounded_console_output(b"", b"offline\r\n"), "offline");
        assert_eq!(bounded_console_output(b"", b""), "no diagnostic output");
        assert_eq!(bounded_console_output(&vec![b'x'; 5000], b"").len(), 4096);
    }

    #[test]
    fn config_init_preserves_existing_user_file() {
        let root = temporary_root("existing");
        let profile = root.join("profile");
        let roaming = root.join("roaming");
        fs::create_dir_all(profile.join("vivido")).unwrap();
        fs::create_dir_all(roaming.join("vivido")).unwrap();
        fs::write(profile.join("vivido/vivido.toml"), "existing = true\n").unwrap();
        fs::write(roaming.join("vivido/vivido.toml"), "legacy = true\n").unwrap();

        initialize_config_at(&profile, &roaming).unwrap();
        assert_eq!(
            fs::read_to_string(profile.join("vivido/vivido.toml")).unwrap(),
            "existing = true\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_init_migrates_legacy_file_with_unicode_path() {
        let root = temporary_root("José Example");
        let profile = root.join("profile");
        let roaming = root.join("roaming");
        fs::create_dir_all(&profile).unwrap();
        fs::create_dir_all(roaming.join("vivido")).unwrap();
        fs::write(roaming.join("vivido/vivido.toml"), "legacy = true\n").unwrap();

        initialize_config_at(&profile, &roaming).unwrap();
        assert_eq!(
            fs::read_to_string(profile.join("vivido/vivido.toml")).unwrap(),
            "legacy = true\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_init_seeds_default_when_no_config_exists() {
        let root = temporary_root("default");
        let profile = root.join("profile");
        let roaming = root.join("roaming");
        fs::create_dir_all(&profile).unwrap();
        fs::create_dir_all(&roaming).unwrap();

        initialize_config_at(&profile, &roaming).unwrap();
        assert_eq!(fs::read_to_string(profile.join("vivido/vivido.toml")).unwrap(), DEFAULT_CONFIG);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn config_init_refuses_profile_symlink() {
        use std::os::unix::fs::symlink;

        let root = temporary_root("symlink");
        let real_profile = root.join("real-profile");
        let profile_link = root.join("profile-link");
        let roaming = root.join("roaming");
        fs::create_dir_all(&real_profile).unwrap();
        fs::create_dir_all(&roaming).unwrap();
        symlink(&real_profile, &profile_link).unwrap();

        let error = initialize_config_at(&profile_link, &roaming).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(root).unwrap();
    }

    fn temporary_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        env::temp_dir().join(format!("vivido-windows-setup-{label}-{}-{nonce}", process::id()))
    }
}
