//! SSH wrapper for forwarding the current Vivido window's Vivid endpoint.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const HELP: &str = r#"Forward the current Vivido window's Vivid endpoint over SSH.

Usage: vvssh [SSH_OPTIONS] DESTINATION

All arguments are passed to ssh and DESTINATION must be the final argument. Connection options can
also be placed in ~/.ssh/config. vvssh opens an interactive remote login shell; remote commands and
options that suppress the remote session (such as -N, -T, and -W) are not supported.

Examples:
  vvssh user@host
  vvssh -p 2222 user@host
  vvssh -J bastion user@host
"#;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("vvssh: {error}");
            ExitCode::FAILURE
        },
    }
}

fn run() -> Result<u8, String> {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.len() == 1 && matches_argument(&arguments[0], &["-h", "--help"]) {
        print!("{HELP}");
        return Ok(0);
    }
    if arguments.len() == 1 && matches_argument(&arguments[0], &["-V", "--version"]) {
        println!("vvssh {}", env!("VERSION"));
        return Ok(0);
    }
    validate_arguments(&arguments)?;

    let endpoint = env::var("VIVID_ENDPOINT")
        .map_err(|_| "VIVID_ENDPOINT is not set; run vvssh inside Vivido".to_owned())?;
    let token = env::var("VIVID_TOKEN")
        .map_err(|_| "VIVID_TOKEN is not set; run vvssh inside Vivido".to_owned())?;
    if token.is_empty() {
        return Err("VIVID_TOKEN is empty; start a fresh Vivido window".into());
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let (setup_arguments, ssh_arguments, token_file) =
        build_ssh_arguments(arguments, &endpoint, std::process::id(), nonce)?;

    let ssh = env::var_os("VVSSH_SSH").unwrap_or_else(|| OsString::from("ssh"));
    let mut setup = Command::new(&ssh)
        .args(&setup_arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not provision remote Vivid token: {error}"))?;
    let transfer_result = setup
        .stdin
        .take()
        .ok_or_else(|| "could not open protected token channel".to_owned())?
        .write_all(token.as_bytes());
    let setup_status = setup.wait().map_err(|error| format!("token setup failed: {error}"))?;
    if let Err(error) = transfer_result {
        let _ = cleanup_remote_token(&ssh, &setup_arguments, &token_file);
        return Err(format!("could not transfer Vivid token: {error}"));
    }
    if !setup_status.success() {
        let _ = cleanup_remote_token(&ssh, &setup_arguments, &token_file);
        return Err("remote host rejected the protected Vivid token setup channel".into());
    }
    let status = Command::new(&ssh).args(&ssh_arguments).status().map_err(|error| {
        let _ = cleanup_remote_token(&ssh, &setup_arguments, &token_file);
        format!("could not run {}: {error}", Path::new(&ssh).display())
    })?;

    Ok(status.code().and_then(|code| u8::try_from(code).ok()).unwrap_or(1))
}

fn validate_arguments(arguments: &[OsString]) -> Result<(), String> {
    if arguments.is_empty() {
        return Err("missing SSH destination; run `vvssh --help` for usage".into());
    }
    if arguments.last().is_some_and(|argument| argument.to_string_lossy().starts_with('-')) {
        return Err("missing final SSH destination; run `vvssh --help` for usage".into());
    }
    Ok(())
}

fn matches_argument(argument: &OsStr, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| argument == OsStr::new(candidate))
}

fn build_ssh_arguments(
    passthrough: Vec<OsString>,
    endpoint: &str,
    process_id: u32,
    nonce: u128,
) -> Result<(Vec<OsString>, Vec<OsString>, String), String> {
    let local_socket = endpoint
        .strip_prefix("unix:")
        .ok_or_else(|| format!("expected a unix: VIVID_ENDPOINT, got {endpoint}"))?;
    if !Path::new(local_socket).is_absolute() {
        return Err(format!("VIVID_ENDPOINT socket path is not absolute: {local_socket}"));
    }
    if local_socket.contains(':') {
        return Err(
            "VIVID_ENDPOINT socket path contains ':' and cannot be forwarded by OpenSSH".into()
        );
    }
    let remote_socket = format!("/tmp/vivido-vivid-{process_id}-{nonce}.sock");
    let token_file = format!("/tmp/vivido-vivid-{process_id}-{nonce}.token");
    let remote_endpoint = format!("unix:{remote_socket}");
    let remote_command = format!(
        "VIVID_TOKEN=$(cat {}) && rm -f {} && export VIVID_TOKEN && env VIVID_ENDPOINT={} \"$SHELL\" -l",
        shell_quote(&token_file),
        shell_quote(&token_file),
        shell_quote(&remote_endpoint),
    );
    let remote_forward = format!("{remote_socket}:{local_socket}");

    let mut setup = passthrough.clone();
    setup.push(OsString::from(format!("umask 077 && cat > {}", shell_quote(&token_file))));
    let mut arguments = vec![
        OsString::from("-tt"),
        OsString::from("-o"),
        OsString::from("ExitOnForwardFailure=yes"),
        OsString::from("-o"),
        OsString::from("StreamLocalBindMask=0177"),
        OsString::from("-o"),
        OsString::from("StreamLocalBindUnlink=yes"),
        OsString::from("-R"),
        OsString::from(remote_forward),
    ];
    arguments.extend(passthrough);
    arguments.push(OsString::from(remote_command));
    Ok((setup, arguments, token_file))
}

fn cleanup_remote_token(
    ssh: &OsStr,
    setup_arguments: &[OsString],
    token_file: &str,
) -> Result<(), String> {
    let mut arguments = setup_arguments[..setup_arguments.len().saturating_sub(1)].to_vec();
    arguments.push(OsString::from(format!("rm -f {}", shell_quote(token_file))));
    Command::new(ssh)
        .args(arguments)
        .status()
        .map(|_| ())
        .map_err(|error| format!("could not clean remote token: {error}"))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_private_stream_local_forward() {
        let (_, arguments, token_file) = build_ssh_arguments(
            vec![OsString::from("-p"), OsString::from("2222"), OsString::from("user@host")],
            "unix:/private/tmp/vivido/endpoint.sock",
            42,
            99,
        )
        .unwrap();
        let arguments =
            arguments.iter().map(|argument| argument.to_string_lossy()).collect::<Vec<_>>();

        assert_eq!(arguments[0], "-tt");
        assert!(arguments.contains(&"ExitOnForwardFailure=yes".into()));
        assert!(arguments.contains(&"StreamLocalBindMask=0177".into()));
        assert!(arguments.contains(&"StreamLocalBindUnlink=yes".into()));
        assert!(
            arguments
                .contains(&"/tmp/vivido-vivid-42-99.sock:/private/tmp/vivido/endpoint.sock".into())
        );
        assert_eq!(&arguments[9..12], &["-p", "2222", "user@host"]);
        assert!(arguments[12].contains(&token_file));
        assert!(!arguments.iter().any(|argument| argument.contains("0123abcd")));
    }

    #[test]
    fn rejects_non_unix_endpoints() {
        let error = build_ssh_arguments(vec![OsString::from("host")], "tcp:localhost:1234", 1, 2)
            .unwrap_err();

        assert!(error.contains("expected a unix:"));
    }

    #[test]
    fn quotes_remote_environment_values() {
        assert_eq!(shell_quote("abc'def"), "'abc'\\''def'");
    }

    #[test]
    fn recognizes_help_arguments() {
        assert!(matches_argument(OsStr::new("-h"), &["-h", "--help"]));
        assert!(matches_argument(OsStr::new("--help"), &["-h", "--help"]));
        assert!(!matches_argument(OsStr::new("host"), &["-h", "--help"]));
    }

    #[test]
    fn requires_final_destination() {
        assert!(validate_arguments(&[]).is_err());
        assert!(validate_arguments(&[OsString::from("-v")]).is_err());
        assert!(validate_arguments(&[OsString::from("-v"), OsString::from("host")]).is_ok());
    }
}
