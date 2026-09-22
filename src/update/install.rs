//! Platform installer verification and launch helpers.

use std::io;
use std::path::Path;

// Only the platform installers and their test doubles return update errors.
#[cfg(any(test, windows, target_os = "macos"))]
use super::UpdateError;

#[cfg(target_os = "macos")]
const SIGNATURE_OUTPUT_LIMIT: usize = 64 * 1024;

/// Verify that a downloaded Windows installer is trusted and has the expected publisher.
#[cfg(windows)]
pub fn verify_installer(path: &Path, publisher: &str) -> Result<(), UpdateError> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::Security::Cryptography::{
        CERT_NAME_SIMPLE_DISPLAY_TYPE, CertGetNameStringW,
    };
    use windows::Win32::Security::WinTrust::{
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
        WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY,
        WTD_UI_NONE, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData,
        WinVerifyTrustEx,
    };
    use windows::core::PCWSTR;

    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: u32::try_from(std::mem::size_of::<WINTRUST_FILE_INFO>())
            .map_err(|_| UpdateError::verification("WinTrust file structure is too large"))?,
        pcwszFilePath: PCWSTR(wide_path.as_ptr()),
        hFile: HANDLE::default(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: u32::try_from(std::mem::size_of::<WINTRUST_DATA>())
            .map_err(|_| UpdateError::verification("WinTrust data structure is too large"))?,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: std::ptr::from_mut(&mut file) },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;

    // SAFETY: `file`, `data`, and the NUL-terminated path remain alive and unmoved for the call.
    // WinVerifyTrust initializes `hWVTStateData`, which is closed below on every return path.
    let status = unsafe { WinVerifyTrustEx(HWND::default(), &mut action, &mut data) };
    let verification = if status == 0 {
        (|| {
            // SAFETY: a successful stateful WinVerifyTrust call owns valid provider data until the
            // matching WTD_STATEACTION_CLOSE call below.
            let provider = unsafe { WTHelperProvDataFromStateData(data.hWVTStateData) };
            if provider.is_null() {
                return Err(UpdateError::verification(
                    "WinTrust did not return signature provider data",
                ));
            }
            // SAFETY: `provider` is valid for the open WinTrust state and signer index zero is the
            // primary Authenticode signer.
            let signer = unsafe { WTHelperGetProvSignerFromChain(provider, 0, false, 0) };
            if signer.is_null() {
                return Err(UpdateError::verification(
                    "WinTrust did not return an Authenticode signer",
                ));
            }
            // SAFETY: `signer` was returned for the live provider state.
            let signer = unsafe { &*signer };
            if signer.csCertChain == 0 || signer.pasCertChain.is_null() {
                return Err(UpdateError::verification(
                    "the Authenticode signer has no certificate chain",
                ));
            }
            // SAFETY: the non-empty certificate chain belongs to the live signer state.
            let certificate = unsafe { (*signer.pasCertChain).pCert };
            if certificate.is_null() {
                return Err(UpdateError::verification(
                    "the Authenticode signer certificate is missing",
                ));
            }
            // SAFETY: the certificate context remains valid until WinTrust state is closed.
            let required = unsafe {
                CertGetNameStringW(certificate, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, None)
            };
            if required <= 1 || required > 1024 {
                return Err(UpdateError::verification(
                    "the Authenticode publisher name has an invalid length",
                ));
            }
            let capacity = usize::try_from(required)
                .map_err(|_| UpdateError::verification("publisher name length overflow"))?;
            let mut name = vec![0_u16; capacity];
            // SAFETY: `name` has the exact capacity advertised by CertGetNameStringW and the
            // certificate context is still owned by the open WinTrust state.
            let written = unsafe {
                CertGetNameStringW(
                    certificate,
                    CERT_NAME_SIMPLE_DISPLAY_TYPE,
                    0,
                    None,
                    Some(&mut name),
                )
            };
            if written != required {
                return Err(UpdateError::verification(
                    "could not read the Authenticode publisher name",
                ));
            }
            let publisher_name = String::from_utf16(&name[..capacity - 1])
                .map_err(|_| UpdateError::verification("publisher name is not valid UTF-16"))?;
            if !publisher_matches(&publisher_name, publisher) {
                return Err(UpdateError::verification(format!(
                    "installer publisher mismatch: expected {publisher}"
                )));
            }
            Ok(())
        })()
    } else {
        Err(UpdateError::verification(format!("WinVerifyTrust returned {status:#x}")))
    };

    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: this closes the state created by the VERIFY call above; all borrowed provider data
    // has gone out of scope before this call.
    let _ = unsafe { WinVerifyTrustEx(HWND::default(), &mut action, &mut data) };
    verification
}

/// Verify that a downloaded macOS package has the expected Developer ID Installer signature.
#[cfg(target_os = "macos")]
pub fn verify_installer(path: &Path, team_id: &str) -> Result<(), UpdateError> {
    let output = std::process::Command::new("pkgutil")
        .arg("--check-signature")
        .arg(path)
        .output()
        .map_err(UpdateError::Io)?;
    let output_size = output
        .stdout
        .len()
        .checked_add(output.stderr.len())
        .ok_or_else(|| UpdateError::verification("pkgutil output length overflow"))?;
    if output_size > SIGNATURE_OUTPUT_LIMIT {
        return Err(UpdateError::verification("pkgutil output exceeds 64 KiB"));
    }
    if !output.status.success() {
        return Err(UpdateError::verification("pkgutil rejected the installer signature"));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| UpdateError::verification("pkgutil output is not valid UTF-8"))?;
    parse_pkgutil_signature(stdout, team_id)
}

/// Launch the platform installer without waiting for it to exit.
#[cfg(windows)]
pub fn launch_installer(path: &Path) -> io::Result<()> {
    let arguments = [std::ffi::OsStr::new("/i"), path.as_os_str()];
    crate::daemon::spawn_daemon("msiexec.exe", arguments)
}

/// Launch the platform installer without waiting for it to exit.
#[cfg(target_os = "macos")]
pub fn launch_installer(path: &Path) -> io::Result<()> {
    crate::daemon::spawn_daemon("open", [path.as_os_str()], None)
}

/// Return an unsupported error on platforms without a suite installer.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn launch_installer(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "in-app installation is only supported on Windows and macOS",
    ))
}

#[cfg(any(test, windows))]
pub(super) fn publisher_matches(actual: &str, expected: &str) -> bool {
    normalize_publisher(actual) == normalize_publisher(expected)
}

#[cfg(any(test, windows))]
fn normalize_publisher(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(any(test, target_os = "macos"))]
pub(super) fn parse_pkgutil_signature(
    output: &str,
    expected_team_id: &str,
) -> Result<(), UpdateError> {
    let actual_team_id = output.lines().find_map(|line| {
        let marker = "Developer ID Installer:";
        let line = line.trim();
        let certificate = line.strip_prefix(marker).or_else(|| {
            let (ordinal, certificate) = line.split_once(". ")?;
            ordinal
                .chars()
                .all(|character| character.is_ascii_digit())
                .then(|| certificate.strip_prefix(marker))?
        })?;
        let certificate = certificate.trim();
        let start = certificate.rfind('(')?;
        let team_id = certificate.get(start + 1..)?.strip_suffix(')')?;
        (!team_id.is_empty()).then_some(team_id)
    });
    match actual_team_id {
        Some(actual) if actual == expected_team_id => Ok(()),
        Some(_) => Err(UpdateError::verification(
            "installer Developer ID Team ID does not match the update manifest",
        )),
        None => Err(UpdateError::verification(
            "pkgutil did not report a Developer ID Installer certificate",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_comparison_is_case_insensitive_and_normalizes_whitespace() {
        assert!(publisher_matches("  Wensheng   Wang ", "wensheng wang"));
        assert!(!publisher_matches("Wensheng Wang LLC", "Wensheng Wang"));
    }

    #[test]
    fn pkgutil_parser_accepts_matching_developer_id_installer() {
        let output = r#"Package "Vivido.pkg":
   Status: signed by a developer certificate issued by Apple for distribution
   Certificate Chain:
    1. Developer ID Installer: Vivido Developer (TEAMID1234)
"#;
        assert!(parse_pkgutil_signature(output, "TEAMID1234").is_ok());
    }

    #[test]
    fn pkgutil_parser_rejects_wrong_team_id() {
        let output = "1. Developer ID Installer: Vivido Developer (WRONGID123)";
        assert!(parse_pkgutil_signature(output, "TEAMID1234").is_err());
    }

    #[test]
    fn pkgutil_parser_rejects_unsigned_and_malformed_output() {
        assert!(parse_pkgutil_signature("Status: no signature", "TEAMID1234").is_err());
        assert!(
            parse_pkgutil_signature("Developer ID Installer: malformed", "TEAMID1234").is_err()
        );
        assert!(
            parse_pkgutil_signature(
                "Package \"Developer ID Installer: Fake (TEAMID1234)\":",
                "TEAMID1234"
            )
            .is_err()
        );
    }
}
