//! Directory reporting for interactive PowerShell sessions, without modifying user profiles.

use base64::Engine;

use crate::terminal::tty::{Options, Shell};

const PROMPT_HOOK: &str = include_str!("powershell_prompt.ps1");

pub(super) fn configure(options: &mut Options) {
    let shell = options.shell.get_or_insert_with(|| Shell::new("powershell".into(), Vec::new()));
    let name = shell.program.rsplit(['/', '\\']).next().unwrap_or_default();
    if !["pwsh", "pwsh.exe", "powershell", "powershell.exe"]
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
        || !interactive_arguments(&shell.args)
    {
        return;
    }

    // Profiles load before -EncodedCommand, so capture the user's finished prompt. Restrict
    // this to interactive launches; scripts, commands, and noninteractive jobs keep their args.
    if !shell.args.iter().any(|arg| arg.eq_ignore_ascii_case("-NoExit")) {
        shell.args.push("-NoExit".into());
    }
    let bytes = PROMPT_HOOK.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    shell.args.extend(["-EncodedCommand".into(), base64::prelude::BASE64_STANDARD.encode(bytes)]);
}

fn interactive_arguments(args: &[String]) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_ascii_lowercase().as_str() {
            "-nologo" | "-noprofile" | "-noexit" | "-sta" | "-mta" => (),
            "-executionpolicy" | "-workingdirectory" => {
                if args.next().is_none() {
                    return false;
                }
            },
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_powershell_gets_directory_reporting() {
        for program in ["pwsh.exe", r"C:\Program Files\PowerShell\7\pwsh.exe", "powershell"] {
            let mut options = Options {
                shell: Some(Shell::new(program.into(), vec!["-NoProfile".into()])),
                ..Options::default()
            };
            configure(&mut options);
            let shell = options.shell.unwrap();
            assert_eq!(shell.program, program);
            assert_eq!(&shell.args[..3], ["-NoProfile", "-NoExit", "-EncodedCommand"]);
        }
    }

    #[test]
    fn commands_and_other_shells_are_not_wrapped() {
        for (program, args) in [
            ("pwsh", vec!["-Command", "Get-Date"]),
            ("powershell.exe", vec!["-File", "task.ps1"]),
            ("pwsh", vec!["-NonInteractive"]),
            ("pwsh", vec!["-c", "exit"]),
            ("wsl.exe", vec![]),
        ] {
            let mut options = Options {
                shell: Some(Shell::new(
                    program.into(),
                    args.iter().map(|arg| (*arg).into()).collect(),
                )),
                ..Options::default()
            };
            let before = options.clone();
            configure(&mut options);
            assert_eq!(options, before);
        }
    }

    #[test]
    fn prompt_reports_cd_and_preserves_a_custom_prompt() {
        // Exercise the actual script in Windows PowerShell, available on supported Windows hosts.
        let script = format!(
            "function global:prompt {{ 'custom prompt> ' }}\n{PROMPT_HOOK}\n\
             Set-Location $env:SystemRoot\nprompt\nSet-Location $env:TEMP\nprompt"
        );
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .expect("run PowerShell prompt integration");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let output = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.matches("custom prompt> ").count(), 2);
        let reports = output
            .split("\x1b]7;")
            .skip(1)
            .map(|part| part.split('\x07').next().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(reports.len(), 2, "{output}");
        assert!(reports.iter().all(|report| report.starts_with("file:///")));
        assert_ne!(reports[0], reports[1]);
    }
}
