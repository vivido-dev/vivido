//! SSH wrapper for forwarding the current Vivido window's Vivid endpoint.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::{Child, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;

#[path = "vvssh/askpass.rs"]
mod askpass;

use askpass::CredentialBroker;

const REALTIME_READY: &[u8; 16] = b"VIVID-REAL-READY";
const BULK_READY: &[u8; 16] = b"VIVID-BULK-READY";

const HELP: &str = r#"Forward the current Vivido window's Vivid endpoint over SSH.

Usage: vvssh [SSH_OPTIONS] DESTINATION [REMOTE_SHELL [ARGUMENTS...]]

vvssh option:
  --shared-media-transport    Carry media on the interactive SSH TCP connection (legacy mode).
  --separate-media-transport  Use independent realtime and bulk SSH connections (default).
  --no-receive-drops          Do not start the optional remote vvreceive helper.

SSH connection options are passed through and can also be placed in ~/.ssh/config. vvssh opens an
interactive remote login shell. A shell command may follow DESTINATION, for example `pwsh.exe` on a
Windows SSH server. Options that suppress the remote session (such as -N, -T, and -W) are not
supported.

Examples:
  vvssh user@host
  vvssh -p 2222 user@host
  vvssh -J bastion user@host
  vvssh user@windows-host pwsh.exe
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemotePlatform {
    Posix,
    Windows,
}

struct SshInvocation {
    connection: Vec<OsString>,
    remote_shell: Vec<OsString>,
}

#[derive(Debug)]
struct MediaForward {
    lane: &'static str,
    arguments: Vec<OsString>,
    ready_marker: &'static [u8; 16],
    cleanup_path: Option<String>,
}

#[derive(Debug)]
struct BuiltSshArguments {
    setup: Vec<OsString>,
    interactive: Vec<OsString>,
    secret_path: String,
    media: Vec<MediaForward>,
}

fn main() -> ExitCode {
    if askpass::is_helper() {
        return match askpass::run_helper() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("vvssh askpass: {error}");
                ExitCode::FAILURE
            },
        };
    }
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("vvssh: {error}");
            ExitCode::FAILURE
        },
    }
}

fn run() -> Result<u8, String> {
    let mut arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.len() == 1 && matches_argument(&arguments[0], &["-h", "--help"]) {
        print!("{HELP}");
        return Ok(0);
    }
    if arguments.len() == 1 && matches_argument(&arguments[0], &["-V", "--version"]) {
        println!("vvssh {}", env!("VERSION"));
        return Ok(0);
    }
    let separate_media = take_media_transport_flags(&mut arguments)?;
    let receive_drops = take_receive_drop_flag(&mut arguments);
    let invocation = parse_ssh_invocation(arguments)?;

    let endpoint = env::var("VIVID_ENDPOINT_CONTROL")
        .map_err(|_| "VIVID_ENDPOINT_CONTROL is not set; run vvssh inside Vivido".to_owned())?;
    let root_secret = env::var("VIVID_ROOT_SECRET")
        .map_err(|_| "VIVID_ROOT_SECRET is not set; run vvssh inside Vivido".to_owned())?;
    if root_secret.is_empty() {
        return Err("VIVID_ROOT_SECRET is empty; start a fresh Vivido window".into());
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let ssh = env::var_os("VVSSH_SSH").unwrap_or_else(|| OsString::from("ssh"));
    let credential_broker = CredentialBroker::new(std::process::id(), nonce)
        .map_err(|error| format!("could not initialize SSH credential broker: {error}"))?;
    let remote_platform = detect_remote_platform(&credential_broker, &ssh, &invocation.connection)?;
    let built = build_ssh_arguments(
        invocation,
        &endpoint,
        std::process::id(),
        nonce,
        separate_media,
        receive_drops,
        remote_platform,
    )?;
    let media_paths =
        built.media.iter().filter_map(|forward| forward.cleanup_path.clone()).collect::<Vec<_>>();
    let mut setup_command = credential_broker.command(&ssh, "setup");
    let mut setup = setup_command
        .args(&built.setup)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not provision remote Vivid root secret: {error}"))?;
    let transfer_result = setup
        .stdin
        .take()
        .ok_or_else(|| "could not open protected root-secret channel".to_owned())?
        .write_all(root_secret.as_bytes());
    let setup_status =
        setup.wait().map_err(|error| format!("root-secret setup failed: {error}"))?;
    if let Err(error) = transfer_result {
        let _ = cleanup_remote_secret(
            &credential_broker,
            &ssh,
            &built.setup,
            &built.secret_path,
            remote_platform,
        );
        return Err(format!("could not transfer Vivid root secret: {error}"));
    }
    if !setup_status.success() {
        let _ = cleanup_remote_secret(
            &credential_broker,
            &ssh,
            &built.setup,
            &built.secret_path,
            remote_platform,
        );
        return Err("remote host rejected the protected Vivid root-secret setup channel".into());
    }
    let mut media = Vec::with_capacity(built.media.len());
    for forward in built.media {
        match start_media_forward(&credential_broker, &ssh, forward) {
            Ok(child) => media.push(child),
            Err(error) => {
                stop_media_forwards(&mut media);
                let _ = cleanup_remote_paths(
                    &credential_broker,
                    &ssh,
                    &built.setup,
                    &built.secret_path,
                    &media_paths,
                    remote_platform,
                );
                return Err(error);
            },
        }
    }
    let status =
        match credential_broker.command(&ssh, "interactive").args(&built.interactive).status() {
            Ok(status) => status,
            Err(error) => {
                stop_media_forwards(&mut media);
                let _ = cleanup_remote_paths(
                    &credential_broker,
                    &ssh,
                    &built.setup,
                    &built.secret_path,
                    &media_paths,
                    remote_platform,
                );
                return Err(format!("could not run {}: {error}", Path::new(&ssh).display()));
            },
        };

    stop_media_forwards(&mut media);
    let _ = cleanup_remote_paths(
        &credential_broker,
        &ssh,
        &built.setup,
        &built.secret_path,
        &media_paths,
        remote_platform,
    );

    Ok(status.code().and_then(|code| u8::try_from(code).ok()).unwrap_or(1))
}

fn start_media_forward(
    credential_broker: &CredentialBroker,
    ssh: &OsStr,
    forward: MediaForward,
) -> Result<Child, String> {
    let mut command = credential_broker.command(ssh, forward.lane);
    let mut child = command
        .args(forward.arguments)
        // The POSIX helper waits on this pipe. Keeping it owned by the child handle binds the
        // remote listener to this vvssh invocation without placing media on stdin.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!("could not start separate {} transport: {error}", forward.lane)
        })?;
    let mut ready = [0_u8; 16];
    let readiness = child
        .stdout
        .as_mut()
        .ok_or_else(|| format!("separate {} transport has no readiness channel", forward.lane))
        .and_then(|stdout| {
            stdout.read_exact(&mut ready).map_err(|error| {
                format!("separate {} transport did not become ready: {error}", forward.lane)
            })
        });
    if let Err(error) = readiness {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    if &ready != forward.ready_marker {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "separate {} transport returned an invalid readiness marker",
            forward.lane
        ));
    }
    Ok(child)
}

fn stop_media_forwards(children: &mut [Child]) {
    for child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn take_media_transport_flags(arguments: &mut Vec<OsString>) -> Result<bool, String> {
    let mut separate = false;
    let mut shared = false;
    arguments.retain(|argument| {
        if argument == OsStr::new("--separate-media-transport") {
            separate = true;
            false
        } else if argument == OsStr::new("--shared-media-transport") {
            shared = true;
            false
        } else {
            true
        }
    });
    if separate && shared {
        return Err("--separate-media-transport conflicts with --shared-media-transport".into());
    }
    Ok(!shared)
}

fn take_receive_drop_flag(arguments: &mut Vec<OsString>) -> bool {
    let mut receive = true;
    arguments.retain(|argument| {
        if argument == OsStr::new("--no-receive-drops") {
            receive = false;
            false
        } else {
            true
        }
    });
    receive
}

fn parse_ssh_invocation(arguments: Vec<OsString>) -> Result<SshInvocation, String> {
    if arguments.is_empty() {
        return Err("missing SSH destination; run `vvssh --help` for usage".into());
    }

    let mut index = 0;
    let mut options_done = false;
    while index < arguments.len() {
        let argument = arguments[index].to_string_lossy();
        if options_done || !argument.starts_with('-') || argument == "-" {
            let connection = arguments[..=index].to_vec();
            let remote_shell = arguments[index + 1..].to_vec();
            return Ok(SshInvocation { connection, remote_shell });
        }
        if argument == "--" {
            options_done = true;
            index += 1;
            continue;
        }
        if ssh_option_takes_value(&argument) && argument.len() == 2 {
            index += 1;
            if index == arguments.len() {
                return Err(format!("SSH option {argument} requires an argument"));
            }
        }
        index += 1;
    }

    Err("missing SSH destination; run `vvssh --help` for usage".into())
}

fn ssh_option_takes_value(argument: &str) -> bool {
    matches!(
        argument.as_bytes().get(1),
        Some(
            b'B' | b'b'
                | b'c'
                | b'D'
                | b'E'
                | b'e'
                | b'F'
                | b'I'
                | b'i'
                | b'J'
                | b'L'
                | b'l'
                | b'm'
                | b'O'
                | b'o'
                | b'p'
                | b'Q'
                | b'R'
                | b'S'
                | b'W'
                | b'w'
        )
    )
}

fn matches_argument(argument: &OsStr, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| argument == OsStr::new(candidate))
}

fn detect_remote_platform(
    credential_broker: &CredentialBroker,
    ssh: &OsStr,
    connection: &[OsString],
) -> Result<RemotePlatform, String> {
    let windows_status = remote_probe_status(
        credential_broker,
        ssh,
        connection,
        "probe-windows",
        "cmd.exe /d /c exit 23",
    )?;
    if windows_status == Some(23) {
        return Ok(RemotePlatform::Windows);
    }

    let posix_status =
        remote_probe_status(credential_broker, ssh, connection, "probe-posix", "sh -c 'exit 24'")?;
    if posix_status == Some(24) {
        return Ok(RemotePlatform::Posix);
    }

    Err("could not identify the SSH server as POSIX or Windows".into())
}

fn remote_probe_status(
    credential_broker: &CredentialBroker,
    ssh: &OsStr,
    connection: &[OsString],
    context: &str,
    remote_command: &str,
) -> Result<Option<i32>, String> {
    let mut arguments = connection.to_vec();
    arguments.push(OsString::from(remote_command));
    credential_broker
        .command(ssh, context)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.code())
        .map_err(|error| format!("could not probe remote SSH platform: {error}"))
}

fn build_ssh_arguments(
    invocation: SshInvocation,
    endpoint: &str,
    process_id: u32,
    nonce: u128,
    separate_media: bool,
    receive_drops: bool,
    remote_platform: RemotePlatform,
) -> Result<BuiltSshArguments, String> {
    let local_target = local_forward_target(endpoint)?;
    match remote_platform {
        RemotePlatform::Posix => build_posix_ssh_arguments(
            invocation,
            local_target,
            process_id,
            nonce,
            separate_media,
            receive_drops,
        ),
        RemotePlatform::Windows => {
            build_windows_ssh_arguments(invocation, local_target, process_id, nonce, separate_media)
        },
    }
}

fn build_posix_ssh_arguments(
    invocation: SshInvocation,
    local_target: String,
    process_id: u32,
    nonce: u128,
    separate_media: bool,
    receive_drops: bool,
) -> Result<BuiltSshArguments, String> {
    let remote_socket = format!("/tmp/vivido-vivid-{process_id}-{nonce}.sock");
    let secret_file = format!("/tmp/vivido-vivid-{process_id}-{nonce}.secret");
    let remote_endpoint = format!("unix:{remote_socket}");
    let realtime_socket =
        separate_media.then(|| format!("/tmp/vivido-vivid-{process_id}-{nonce}-realtime.sock"));
    let bulk_socket =
        separate_media.then(|| format!("/tmp/vivido-vivid-{process_id}-{nonce}-bulk.sock"));
    #[cfg(windows)]
    let anchor_transport = " VIVID_ANCHOR_TRANSPORT=conpty";
    #[cfg(not(windows))]
    let anchor_transport = "";
    let media_environment = realtime_socket
        .as_ref()
        .zip(bulk_socket.as_ref())
        .map(|(realtime, bulk)| {
            let realtime = shell_quote(&format!("unix:{realtime}"));
            let bulk = shell_quote(&format!("unix:{bulk}"));
            format!(" VIVID_ENDPOINT_REALTIME={realtime} VIVID_ENDPOINT_BULK={bulk}")
        })
        .unwrap_or_default();
    let receiver = if receive_drops {
        "if command -v vvreceive >/dev/null 2>&1; then _vvreceive_ready=0; trap '_vvreceive_ready=1' USR1; vvreceive --shell-pid $$ --signal-ready </dev/null >/dev/null 2>&1 & _vvreceive_pid=$!; while [ \"$_vvreceive_ready\" -eq 0 ] && kill -0 \"$_vvreceive_pid\" 2>/dev/null; do sleep 0.01; done; trap - USR1; unset _vvreceive_ready _vvreceive_pid; fi; "
    } else {
        ""
    };
    // OpenSSH forwards the terminal name with the PTY request, but not Vivido's locally
    // materialized terminfo database. Keep the richer entry when it is installed remotely and
    // otherwise select the compatible system entry before the login shell starts.
    let remote_term = "case \"${TERM-}\" in vivido|vivido-direct) if ! command -v infocmp >/dev/null 2>&1 || ! infocmp \"$TERM\" >/dev/null 2>&1; then TERM=xterm-256color; export TERM; fi;; esac; ";
    let login_shell = posix_login_shell(&invocation.remote_shell)?;
    let connection = invocation.connection;
    let remote_command = format!(
        "VIVID_ROOT_SECRET=$(cat {}) && rm -f {} && export VIVID_ROOT_SECRET && export VIVID_REMOTE=1{anchor_transport} VIVID_ENDPOINT_CONTROL={}{media_environment}; {remote_term}{receiver}{login_shell}",
        shell_quote(&secret_file),
        shell_quote(&secret_file),
        shell_quote(&remote_endpoint),
    );
    let remote_forward = format!("{remote_socket}:{local_target}");

    let mut setup = connection.clone();
    setup.push(OsString::from(format!("umask 077 && cat > {}", shell_quote(&secret_file))));
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
    arguments.extend(connection.iter().cloned());
    arguments.push(OsString::from(remote_command));
    let mut media = Vec::new();
    if let Some(socket) = realtime_socket {
        media.push(posix_media_forward(
            &connection,
            &local_target,
            "realtime",
            socket,
            "VIVID-REAL-READY",
            REALTIME_READY,
        ));
    }
    if let Some(socket) = bulk_socket {
        media.push(posix_media_forward(
            &connection,
            &local_target,
            "bulk",
            socket,
            "VIVID-BULK-READY",
            BULK_READY,
        ));
    }
    Ok(BuiltSshArguments { setup, interactive: arguments, secret_path: secret_file, media })
}

fn posix_media_forward(
    connection: &[OsString],
    local_target: &str,
    lane: &'static str,
    remote_socket: String,
    ready_text: &str,
    ready_marker: &'static [u8; 16],
) -> MediaForward {
    let remote_forward = format!("{remote_socket}:{local_target}");
    let mut arguments = vec![
        OsString::from("-T"),
        OsString::from("-o"),
        OsString::from("ControlMaster=no"),
        OsString::from("-o"),
        OsString::from("ControlPath=none"),
        OsString::from("-o"),
        OsString::from("ExitOnForwardFailure=yes"),
        OsString::from("-o"),
        OsString::from("StreamLocalBindMask=0177"),
        OsString::from("-o"),
        OsString::from("StreamLocalBindUnlink=yes"),
        OsString::from("-R"),
        OsString::from(remote_forward),
    ];
    arguments.extend(connection.iter().cloned());
    arguments.push(OsString::from(format!("printf {ready_text}; cat >/dev/null")));
    MediaForward { lane, arguments, ready_marker, cleanup_path: Some(remote_socket) }
}

fn build_windows_ssh_arguments(
    invocation: SshInvocation,
    local_target: String,
    process_id: u32,
    nonce: u128,
    separate_media: bool,
) -> Result<BuiltSshArguments, String> {
    let secret_directory = format!("vivido-vivid-{process_id}-{nonce}");
    let (control_port, realtime_port, bulk_port) = windows_remote_ports(process_id, nonce);
    let remote_endpoint = format!("tcp:127.0.0.1:{control_port}");
    let realtime_endpoint = separate_media.then(|| format!("tcp:127.0.0.1:{realtime_port}"));
    let bulk_endpoint = separate_media.then(|| format!("tcp:127.0.0.1:{bulk_port}"));
    let setup_script = windows_setup_script(&secret_directory);
    let remote_script = windows_login_script(
        &secret_directory,
        &remote_endpoint,
        realtime_endpoint.as_deref(),
        bulk_endpoint.as_deref(),
        &invocation.remote_shell,
    )?;
    let connection = invocation.connection;

    let mut setup = connection.clone();
    setup.push(powershell_encoded_command(&setup_script));

    let remote_forward = format!("127.0.0.1:{control_port}:{local_target}");
    let mut arguments = vec![
        OsString::from("-tt"),
        OsString::from("-o"),
        OsString::from("ExitOnForwardFailure=yes"),
        OsString::from("-R"),
        OsString::from(remote_forward),
    ];
    arguments.extend(connection.iter().cloned());
    arguments.push(powershell_encoded_command(&remote_script));

    let mut media = Vec::new();
    if realtime_endpoint.is_some() {
        media.push(windows_media_forward(
            &connection,
            &local_target,
            "realtime",
            realtime_port,
            "VIVID-REAL-READY",
            REALTIME_READY,
        ));
    }
    if bulk_endpoint.is_some() {
        media.push(windows_media_forward(
            &connection,
            &local_target,
            "bulk",
            bulk_port,
            "VIVID-BULK-READY",
            BULK_READY,
        ));
    }

    Ok(BuiltSshArguments { setup, interactive: arguments, secret_path: secret_directory, media })
}

fn windows_media_forward(
    connection: &[OsString],
    local_target: &str,
    lane: &'static str,
    remote_port: u16,
    ready_text: &str,
    ready_marker: &'static [u8; 16],
) -> MediaForward {
    let remote_forward = format!("127.0.0.1:{remote_port}:{local_target}");
    let mut arguments = vec![
        OsString::from("-T"),
        OsString::from("-o"),
        OsString::from("ControlMaster=no"),
        OsString::from("-o"),
        OsString::from("ControlPath=none"),
        OsString::from("-o"),
        OsString::from("ExitOnForwardFailure=yes"),
        OsString::from("-R"),
        OsString::from(remote_forward),
    ];
    arguments.extend(connection.iter().cloned());
    // The SSH session owns this helper's lifetime. Waiting on stdin made the Windows forward
    // disappear when that incidental stream reached EOF even though the interactive session was
    // still live; the next track generation then saw WSAECONNREFUSED.
    arguments.push(powershell_encoded_command(&format!(
        "$output=[Console]::OpenStandardOutput(); $marker=[Text.Encoding]::ASCII.GetBytes('{ready_text}'); $output.Write($marker,0,$marker.Length); $output.Flush(); [Threading.Thread]::Sleep([Threading.Timeout]::Infinite)"
    )));
    MediaForward { lane, arguments, ready_marker, cleanup_path: None }
}

fn posix_login_shell(remote_shell: &[OsString]) -> Result<String, String> {
    if remote_shell.is_empty() {
        return Ok("exec \"$SHELL\" -l".into());
    }
    let arguments = remote_shell
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .map(shell_quote)
                .ok_or_else(|| "remote shell arguments must be valid UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!("exec {}", arguments.join(" ")))
}

fn windows_remote_ports(process_id: u32, nonce: u128) -> (u16, u16, u16) {
    const DYNAMIC_PORT_FIRST: u16 = 49_152;
    const DYNAMIC_PORT_COUNT: u16 = 16_384;
    let mixed = nonce ^ u128::from(process_id).rotate_left(31);
    let mut offsets = [
        u16::try_from(mixed % u128::from(DYNAMIC_PORT_COUNT))
            .expect("dynamic port offset is bounded to u16"),
        u16::try_from((mixed >> 14) % u128::from(DYNAMIC_PORT_COUNT))
            .expect("dynamic port offset is bounded to u16"),
        u16::try_from((mixed >> 28) % u128::from(DYNAMIC_PORT_COUNT))
            .expect("dynamic port offset is bounded to u16"),
    ];
    for index in 1..offsets.len() {
        while offsets[..index].contains(&offsets[index]) {
            offsets[index] = (offsets[index] + 1) % DYNAMIC_PORT_COUNT;
        }
    }
    (
        DYNAMIC_PORT_FIRST + offsets[0],
        DYNAMIC_PORT_FIRST + offsets[1],
        DYNAMIC_PORT_FIRST + offsets[2],
    )
}

fn windows_setup_script(secret_directory: &str) -> String {
    let directory = powershell_quote(secret_directory);
    format!(
        "$ErrorActionPreference='Stop'; \
         $directory=Join-Path ([IO.Path]::GetTempPath()) {directory}; \
         [IO.Directory]::CreateDirectory($directory) | Out-Null; \
         $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; \
         & (Join-Path $env:SystemRoot 'System32\\icacls.exe') $directory '/inheritance:r' '/grant:r' ('*'+$sid+':(OI)(CI)(F)') | Out-Null; \
         if ($LASTEXITCODE -ne 0) {{ throw 'could not protect Vivid secret directory' }}; \
         $path=Join-Path $directory 'root.secret'; \
         $file=[IO.File]::Open($path,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None); \
         try {{ [Console]::OpenStandardInput().CopyTo($file) }} finally {{ $file.Dispose() }}"
    )
}

fn windows_login_script(
    secret_directory: &str,
    control_endpoint: &str,
    realtime_endpoint: Option<&str>,
    bulk_endpoint: Option<&str>,
    remote_shell: &[OsString],
) -> Result<String, String> {
    let directory = powershell_quote(secret_directory);
    let control_endpoint = powershell_quote(control_endpoint);
    let media_environment = realtime_endpoint
        .zip(bulk_endpoint)
        .map(|(realtime, bulk)| {
            let realtime = powershell_quote(realtime);
            let bulk = powershell_quote(bulk);
            format!("$env:VIVID_ENDPOINT_REALTIME={realtime}; $env:VIVID_ENDPOINT_BULK={bulk}; ")
        })
        .unwrap_or_default();
    let launch = if remote_shell.is_empty() {
        "$shell=(Get-ItemProperty -LiteralPath 'HKLM:\\SOFTWARE\\OpenSSH' -Name DefaultShell -ErrorAction SilentlyContinue).DefaultShell; if ([string]::IsNullOrWhiteSpace($shell)) { $shell=$env:COMSPEC }; & $shell"
            .to_owned()
    } else {
        let arguments = remote_shell
            .iter()
            .map(|argument| {
                argument
                    .to_str()
                    .map(powershell_quote)
                    .ok_or_else(|| "remote shell arguments must be valid UTF-8".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        format!("& {}", arguments.join(" "))
    };
    Ok(format!(
        "$ErrorActionPreference='Stop'; \
         $directory=Join-Path ([IO.Path]::GetTempPath()) {directory}; \
         $path=Join-Path $directory 'root.secret'; \
         try {{ $secret=[IO.File]::ReadAllText($path) }} finally {{ Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue }}; \
         if ([string]::IsNullOrEmpty($secret)) {{ throw 'empty Vivid root secret' }}; \
         $env:VIVID_ROOT_SECRET=$secret; $secret=$null; \
         $env:VIVID_REMOTE='1'; $env:VIVID_ANCHOR_TRANSPORT='conpty'; \
         $env:VIVID_ENDPOINT_CONTROL={control_endpoint}; \
         {media_environment}{launch}; exit $LASTEXITCODE"
    ))
}

fn powershell_encoded_command(script: &str) -> OsString {
    let bytes = script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    OsString::from(format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {encoded}"
    ))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn local_forward_target(endpoint: &str) -> Result<String, String> {
    if let Some(local_socket) = endpoint.strip_prefix("unix:") {
        if !Path::new(local_socket).is_absolute() {
            return Err(format!(
                "VIVID_ENDPOINT_CONTROL socket path is not absolute: {local_socket}"
            ));
        }
        if local_socket.contains(':') {
            return Err(
                "VIVID_ENDPOINT_CONTROL socket path contains ':' and cannot be forwarded by OpenSSH"
                    .into(),
            );
        }
        return Ok(local_socket.to_owned());
    }
    if let Some(address) = endpoint.strip_prefix("tcp:") {
        let address: SocketAddr = address.parse().map_err(|_| {
            format!("VIVID_ENDPOINT_CONTROL contains an invalid TCP address: {address}")
        })?;
        if address.ip() != IpAddr::V4(std::net::Ipv4Addr::LOCALHOST) {
            return Err("VIVID_ENDPOINT_CONTROL TCP address is not IPv4 loopback".into());
        }
        return Ok(format!("127.0.0.1:{}", address.port()));
    }
    Err(format!("expected a unix: or loopback tcp: VIVID_ENDPOINT_CONTROL, got {endpoint}"))
}

fn cleanup_remote_secret(
    credential_broker: &CredentialBroker,
    ssh: &OsStr,
    setup_arguments: &[OsString],
    secret_file: &str,
    remote_platform: RemotePlatform,
) -> Result<(), String> {
    let mut arguments = setup_arguments[..setup_arguments.len().saturating_sub(1)].to_vec();
    arguments.push(cleanup_remote_command(secret_file, &[], remote_platform));
    credential_broker
        .command(ssh, "cleanup")
        .args(arguments)
        .status()
        .map(|_| ())
        .map_err(|error| format!("could not clean remote root secret: {error}"))
}

fn cleanup_remote_paths(
    credential_broker: &CredentialBroker,
    ssh: &OsStr,
    setup_arguments: &[OsString],
    secret_file: &str,
    media_paths: &[String],
    remote_platform: RemotePlatform,
) -> Result<(), String> {
    let mut arguments = setup_arguments[..setup_arguments.len().saturating_sub(1)].to_vec();
    arguments.push(cleanup_remote_command(secret_file, media_paths, remote_platform));
    credential_broker
        .command(ssh, "cleanup")
        .args(arguments)
        .status()
        .map(|_| ())
        .map_err(|error| format!("could not clean remote Vivid paths: {error}"))
}

fn cleanup_remote_command(
    secret_file: &str,
    media_paths: &[String],
    remote_platform: RemotePlatform,
) -> OsString {
    match remote_platform {
        RemotePlatform::Posix => {
            let mut paths = Vec::with_capacity(media_paths.len() + 1);
            paths.push(shell_quote(secret_file));
            paths.extend(media_paths.iter().map(|path| shell_quote(path)));
            OsString::from(format!("rm -f {}", paths.join(" ")))
        },
        RemotePlatform::Windows => {
            let directory = powershell_quote(secret_file);
            powershell_encoded_command(&format!(
                "$directory=Join-Path ([IO.Path]::GetTempPath()) {directory}; \
                 Remove-Item -LiteralPath $directory -Recurse -Force -ErrorAction SilentlyContinue"
            ))
        },
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(connection: &[&str], remote_shell: &[&str]) -> SshInvocation {
        SshInvocation {
            connection: connection.iter().map(OsString::from).collect(),
            remote_shell: remote_shell.iter().map(OsString::from).collect(),
        }
    }

    #[cfg(unix)]
    fn decoded_powershell_script(command: &OsStr) -> String {
        let command = command.to_string_lossy();
        let encoded = command.split_whitespace().last().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
        let utf16 = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&utf16).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn builds_private_stream_local_forward() {
        let built = build_ssh_arguments(
            invocation(&["-p", "2222", "user@host"], &[]),
            "unix:/private/tmp/vivido/endpoint.sock",
            42,
            99,
            false,
            true,
            RemotePlatform::Posix,
        )
        .unwrap();
        let arguments =
            built.interactive.iter().map(|argument| argument.to_string_lossy()).collect::<Vec<_>>();

        assert_eq!(arguments[0], "-tt");
        assert!(arguments.contains(&"ExitOnForwardFailure=yes".into()));
        assert!(arguments.contains(&"StreamLocalBindMask=0177".into()));
        assert!(arguments.contains(&"StreamLocalBindUnlink=yes".into()));
        assert!(
            arguments
                .contains(&"/tmp/vivido-vivid-42-99.sock:/private/tmp/vivido/endpoint.sock".into())
        );
        assert_eq!(&arguments[9..12], &["-p", "2222", "user@host"]);
        assert!(arguments[12].contains(&built.secret_path));
        assert!(arguments[12].contains("VIVID_ROOT_SECRET"));
        assert!(arguments[12].contains("VIVID_ENDPOINT_CONTROL"));
        assert!(!arguments[12].contains("VIVID_ANCHOR_TRANSPORT"));
        assert!(!arguments.iter().any(|argument| argument.contains("0123abcd")));
    }

    #[test]
    fn builds_posix_forward_to_local_windows_loopback_destination() {
        let built = build_ssh_arguments(
            invocation(&["host"], &[]),
            "tcp:127.0.0.1:1234",
            1,
            2,
            false,
            true,
            RemotePlatform::Posix,
        )
        .unwrap();
        let arguments =
            built.interactive.iter().map(|argument| argument.to_string_lossy()).collect::<Vec<_>>();
        assert!(arguments.contains(&"/tmp/vivido-vivid-1-2.sock:127.0.0.1:1234".into()));
        assert!(arguments.last().unwrap().contains("VIVID_REMOTE=1"));
        #[cfg(windows)]
        assert!(arguments.last().unwrap().contains("VIVID_ANCHOR_TRANSPORT=conpty"));
    }

    #[test]
    fn rejects_non_loopback_tcp_endpoints() {
        let error = build_ssh_arguments(
            invocation(&["host"], &[]),
            "tcp:192.0.2.1:1234",
            1,
            2,
            false,
            true,
            RemotePlatform::Posix,
        )
        .unwrap_err();
        assert!(error.contains("not IPv4 loopback"));
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
    fn parses_destination_and_optional_remote_shell_like_ssh() {
        assert!(parse_ssh_invocation(Vec::new()).is_err());
        assert!(parse_ssh_invocation(vec![OsString::from("-v")]).is_err());

        let parsed = parse_ssh_invocation(
            ["-p", "2222", "wensh@host", "pwsh.exe", "-Login"]
                .into_iter()
                .map(OsString::from)
                .collect(),
        )
        .unwrap();
        assert_eq!(
            parsed.connection,
            [OsString::from("-p"), OsString::from("2222"), OsString::from("wensh@host")]
        );
        assert_eq!(parsed.remote_shell, [OsString::from("pwsh.exe"), OsString::from("-Login")]);
    }

    #[test]
    fn posix_realtime_and_bulk_use_distinct_ssh_transports() {
        let built = build_ssh_arguments(
            invocation(&["user@host"], &[]),
            "tcp:127.0.0.1:4321",
            7,
            9,
            true,
            true,
            RemotePlatform::Posix,
        )
        .unwrap();
        let command = built.interactive.last().unwrap().to_string_lossy();
        assert!(command.contains("VIVID_ENDPOINT_BULK='unix:"));
        assert!(command.contains("VIVID_ENDPOINT_REALTIME='unix:"));
        assert_eq!(built.media.len(), 2);
        let realtime = &built.media[0];
        let bulk = &built.media[1];
        assert_eq!(realtime.lane, "realtime");
        assert_eq!(bulk.lane, "bulk");
        assert_ne!(realtime.cleanup_path, bulk.cleanup_path);
        for forward in &built.media {
            assert!(forward.arguments.iter().any(|argument| argument == "ControlMaster=no"));
            assert!(forward.arguments.iter().any(|argument| argument == "ControlPath=none"));
            assert!(forward.arguments.iter().any(|argument| {
                argument.to_string_lossy().contains(forward.cleanup_path.as_deref().unwrap())
            }));
        }
        assert!(realtime.arguments.last().unwrap().to_string_lossy().contains("VIVID-REAL-READY"));
        assert!(bulk.arguments.last().unwrap().to_string_lossy().contains("VIVID-BULK-READY"));
    }

    #[test]
    fn separate_media_is_default_and_shared_transport_is_an_explicit_opt_out() {
        let mut default = vec![OsString::from("host")];
        assert!(take_media_transport_flags(&mut default).unwrap());
        assert_eq!(default, [OsString::from("host")]);

        let mut shared = vec![OsString::from("--shared-media-transport"), OsString::from("host")];
        assert!(!take_media_transport_flags(&mut shared).unwrap());
        assert_eq!(shared, [OsString::from("host")]);

        let mut conflicting = vec![
            OsString::from("--shared-media-transport"),
            OsString::from("--separate-media-transport"),
            OsString::from("host"),
        ];
        assert!(take_media_transport_flags(&mut conflicting).is_err());
    }

    #[test]
    fn receiver_is_backgrounded_before_exec_and_can_be_disabled() {
        let enabled = build_ssh_arguments(
            invocation(&["host"], &[]),
            "tcp:127.0.0.1:4321",
            7,
            9,
            false,
            true,
            RemotePlatform::Posix,
        )
        .unwrap();
        let command = enabled.interactive.last().unwrap().to_string_lossy();
        assert!(
            command
                .contains("vvreceive --shell-pid $$ --signal-ready </dev/null >/dev/null 2>&1 &")
        );
        assert!(command.contains("trap '_vvreceive_ready=1' USR1"));
        assert!(command.ends_with("exec \"$SHELL\" -l"));

        let disabled = build_ssh_arguments(
            invocation(&["host"], &[]),
            "tcp:127.0.0.1:4321",
            7,
            9,
            false,
            false,
            RemotePlatform::Posix,
        )
        .unwrap();
        assert!(!disabled.interactive.last().unwrap().to_string_lossy().contains("vvreceive"));

        let mut arguments = vec![OsString::from("--no-receive-drops"), OsString::from("host")];
        assert!(!take_receive_drop_flag(&mut arguments));
        assert_eq!(arguments, [OsString::from("host")]);
    }

    #[test]
    fn remote_shell_falls_back_when_vivido_terminfo_is_unavailable() {
        let built = build_ssh_arguments(
            invocation(&["host"], &[]),
            "tcp:127.0.0.1:4321",
            7,
            9,
            false,
            false,
            RemotePlatform::Posix,
        )
        .unwrap();
        let command = built.interactive.last().unwrap().to_string_lossy();
        assert!(command.contains("case \"${TERM-}\" in vivido|vivido-direct)"));
        assert!(command.contains("infocmp \"$TERM\""));
        assert!(command.contains("TERM=xterm-256color; export TERM"));
    }

    #[cfg(unix)]
    #[test]
    fn windows_server_uses_loopback_tcp_forward_and_powershell_bootstrap() {
        let built = build_ssh_arguments(
            invocation(&["wensh@192.168.2.246"], &["pwsh.exe"]),
            "unix:/private/tmp/vivido/endpoint.sock",
            42,
            99,
            true,
            true,
            RemotePlatform::Windows,
        )
        .unwrap();
        let (control_port, realtime_port, bulk_port) = windows_remote_ports(42, 99);
        assert_ne!(control_port, realtime_port);
        assert_ne!(control_port, bulk_port);
        assert_ne!(realtime_port, bulk_port);

        assert!(built.interactive.iter().any(|argument| {
            argument
                .to_string_lossy()
                .contains(&format!("127.0.0.1:{control_port}:/private/tmp/vivido/endpoint.sock"))
        }));
        assert!(
            !built
                .interactive
                .iter()
                .any(|argument| { argument.to_string_lossy().contains("StreamLocalBind") })
        );
        let setup_script = decoded_powershell_script(built.setup.last().unwrap());
        assert!(setup_script.contains("icacls.exe"));
        assert!(setup_script.contains("/inheritance:r"));
        assert!(setup_script.contains("OpenStandardInput"));
        assert!(setup_script.contains(&built.secret_path));

        let login_script = decoded_powershell_script(built.interactive.last().unwrap());
        assert!(login_script.contains(&format!("tcp:127.0.0.1:{control_port}")));
        assert!(login_script.contains(&format!("tcp:127.0.0.1:{realtime_port}")));
        assert!(login_script.contains(&format!("tcp:127.0.0.1:{bulk_port}")));
        assert!(login_script.contains("$env:VIVID_ROOT_SECRET=$secret"));
        assert!(login_script.contains("$env:VIVID_ANCHOR_TRANSPORT='conpty'"));
        assert!(login_script.contains("& 'pwsh.exe'"));
        assert!(!login_script.contains("0123abcd"));

        assert_eq!(built.media.len(), 2);
        let realtime = &built.media[0];
        let bulk = &built.media[1];
        assert!(realtime.arguments.iter().any(|argument| {
            argument
                .to_string_lossy()
                .contains(&format!("127.0.0.1:{realtime_port}:/private/tmp/vivido/endpoint.sock"))
        }));
        assert!(bulk.arguments.iter().any(|argument| {
            argument
                .to_string_lossy()
                .contains(&format!("127.0.0.1:{bulk_port}:/private/tmp/vivido/endpoint.sock"))
        }));
        let realtime_script = decoded_powershell_script(realtime.arguments.last().unwrap());
        assert!(realtime_script.contains("VIVID-REAL-READY"));
        assert!(realtime_script.contains("Sleep([Threading.Timeout]::Infinite)"));
        assert!(!realtime_script.contains("OpenStandardInput"));
        let bulk_script = decoded_powershell_script(bulk.arguments.last().unwrap());
        assert!(bulk_script.contains("VIVID-BULK-READY"));
        assert!(bulk_script.contains("Sleep([Threading.Timeout]::Infinite)"));
        assert!(!bulk_script.contains("OpenStandardInput"));
    }

    #[test]
    fn remote_shell_arguments_are_quoted_for_each_platform() {
        assert_eq!(
            posix_login_shell(&[OsString::from("fish"), OsString::from("a'b")]).unwrap(),
            "exec 'fish' 'a'\\''b'"
        );
        assert_eq!(powershell_quote("a'b"), "'a''b'");
    }
}
