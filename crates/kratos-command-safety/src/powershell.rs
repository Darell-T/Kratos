// Copied from openai/codex rust-v0.159.1, codex-rs/shell-command/src/powershell.rs. Kratos keeps
// the functions that find a PowerShell script body and lower it into commands, and their tests;
// the UTF-8 prefix and the executable search are left out.

use std::path::PathBuf;

use crate::command_safety::try_parse_powershell_commands;
use crate::shell_detect::ShellType;
use crate::shell_detect::detect_shell_type;

const POWERSHELL_FLAGS: &[&str] = &["-nologo", "-noprofile", "-command", "-c"];

/// Extract the PowerShell script body from an invocation such as:
///
/// - ["pwsh", "-NoProfile", "-Command", "Get-ChildItem -Recurse | Select-String foo"]
/// - ["powershell.exe", "-Command", "Write-Host hi"]
/// - ["powershell", "-NoLogo", "-NoProfile", "-Command", "...script..."]
///
/// Returns (`shell`, `script`) when the first arg is a PowerShell executable and a
/// `-Command` (or `-c`) flag is present followed by a script string.
pub fn extract_powershell_command(command: &[String]) -> Option<(&str, &str)> {
    if command.len() < 3 {
        return None;
    }

    let shell = &command[0];
    if !matches!(
        detect_shell_type(PathBuf::from(shell)),
        Some(ShellType::PowerShell)
    ) {
        return None;
    }

    // Find the first occurrence of -Command (accept common short alias -c as well)
    let mut i = 1usize;
    while i + 1 < command.len() {
        let flag = &command[i];
        // Reject unknown flags
        if !POWERSHELL_FLAGS.contains(&flag.to_ascii_lowercase().as_str()) {
            return None;
        }
        if flag.eq_ignore_ascii_case("-Command") || flag.eq_ignore_ascii_case("-c") {
            let script = &command[i + 1];
            return Some((shell, script));
        }
        i += 1;
    }
    None
}

/// Parse the script body from a top-level PowerShell wrapper into argv-like commands.
///
/// This only unwraps the `-Command`/`-c` body from a PowerShell invocation we
/// already recognize, then lowers the script in-process.
pub fn parse_powershell_command_into_plain_commands(
    command: &[String],
) -> Option<Vec<Vec<String>>> {
    let (_, script) = extract_powershell_command(command)?;
    try_parse_powershell_commands(script)
}

/// Parse literal PowerShell commands without starting a PowerShell executable.
///
/// Unknown or dynamic syntax stays opaque so unfamiliar executables can be
/// evaluated without running them before approval.
pub fn parse_powershell_script_into_plain_commands(script: &str) -> Option<Vec<Vec<String>>> {
    try_parse_powershell_commands(script)
}

#[cfg(test)]
mod tests {
    use super::extract_powershell_command;
    #[cfg(windows)]
    use super::parse_powershell_command_into_plain_commands;
    use super::parse_powershell_script_into_plain_commands;

    #[test]
    fn extracts_basic_powershell_command() {
        let cmd = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Write-Host hi".to_string(),
        ];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Write-Host hi");
    }

    #[test]
    fn extracts_lowercase_flags() {
        let cmd = vec![
            "powershell".to_string(),
            "-nologo".to_string(),
            "-command".to_string(),
            "Write-Host hi".to_string(),
        ];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Write-Host hi");
    }

    #[test]
    fn extracts_full_path_powershell_command() {
        let command = if cfg!(windows) {
            "C:\\windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe".to_string()
        } else {
            "/usr/local/bin/powershell.exe".to_string()
        };
        let cmd = vec![command, "-Command".to_string(), "Write-Host hi".to_string()];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Write-Host hi");
    }

    #[test]
    fn extracts_with_noprofile_and_alias() {
        let cmd = vec![
            "pwsh".to_string(),
            "-NoProfile".to_string(),
            "-c".to_string(),
            "Get-ChildItem | Select-String foo".to_string(),
        ];
        let (_shell, script) = extract_powershell_command(&cmd).expect("extract");
        assert_eq!(script, "Get-ChildItem | Select-String foo");
    }

    #[test]
    fn parses_powershell_command_chains_and_preserves_windows_paths() {
        assert_eq!(
            parse_powershell_script_into_plain_commands(r"echo safe && Remove-Item C:\important"),
            Some(vec![
                vec!["echo".to_string(), "safe".to_string()],
                vec!["Remove-Item".to_string(), r"C:\important".to_string()],
            ]),
        );
    }

    #[cfg(windows)]
    #[test]
    fn parses_plain_powershell_commands() {
        let commands = parse_powershell_command_into_plain_commands(&[
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "echo hi".to_string(),
        ])
        .expect("parse");

        assert_eq!(commands, vec![vec!["echo".to_string(), "hi".to_string()]]);
    }

    #[cfg(windows)]
    #[test]
    fn parses_multiple_plain_powershell_commands() {
        let commands = parse_powershell_command_into_plain_commands(&[
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Write-Output foo | Measure-Object".to_string(),
        ])
        .expect("parse");

        assert_eq!(
            commands,
            vec![
                vec!["Write-Output".to_string(), "foo".to_string()],
                vec!["Measure-Object".to_string()],
            ]
        );
    }
}
