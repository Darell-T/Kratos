use kratos_claude_client::CanUseTool;
use kratos_command_safety::{
    DangerousCommandPlatform, dangerous_command, has_bash_syntax_error,
    has_powershell_syntax_error, reads_commands_from_input,
};
use kratos_protocol::{ApprovalRequest, ExtraAccess};
use serde_json::Value;

use super::policy::Action;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shell {
    Bash,
    PowerShell,
}

impl Shell {
    pub(super) fn of(tool_name: &str) -> Option<Self> {
        match tool_name {
            "Bash" => Some(Self::Bash),
            "PowerShell" => Some(Self::PowerShell),
            _ => None,
        }
    }

    fn words(self, text: &str) -> (Vec<String>, DangerousCommandPlatform) {
        match self {
            // Git Bash on Windows starts Windows programs too, so the host's rules apply.
            Self::Bash => (
                ["bash", "-lc", text].map(str::to_owned).to_vec(),
                DangerousCommandPlatform::host(),
            ),
            // The check lowers a PowerShell body only under the Windows rules.
            Self::PowerShell => (
                ["powershell.exe", "-NoProfile", "-Command", text]
                    .map(str::to_owned)
                    .to_vec(),
                DangerousCommandPlatform::Windows,
            ),
        }
    }

    fn has_syntax_error(self, text: &str) -> bool {
        match self {
            Self::Bash => has_bash_syntax_error(text),
            Self::PowerShell => has_powershell_syntax_error(text),
        }
    }

    fn flags(self, text: &str) -> bool {
        let (words, platform) = self.words(text);
        dangerous_command(&words, platform).is_some()
            || reads_commands_from_input(&words)
            || self.has_syntax_error(text)
    }
}

pub(super) fn command_text(input: &Value) -> Option<&str> {
    input["command"]
        .as_str()
        .filter(|text| !text.trim().is_empty() && !text.contains('\0'))
}

pub(super) fn action(shell: Shell, input: &Value) -> Action {
    match command_text(input) {
        Some(text) => Action::Command {
            flagged: shell.flags(text),
        },
        None => Action::Invalid,
    }
}

pub(super) fn approval(tool: &CanUseTool, folder: &str) -> Option<ApprovalRequest> {
    Shell::of(&tool.tool_name)?;
    Some(ApprovalRequest::Command {
        command: command_text(&tool.input)?.to_owned(),
        cwd: Some(folder.to_owned()),
        reason: None,
        access: ExtraAccess::default(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn flagged(shell: Shell, text: &str) -> Action {
        action(shell, &json!({ "command": text }))
    }

    #[test]
    fn the_check_flags_destructive_commands_in_both_shells_and_passes_ordinary_ones() {
        for (shell, text, expected) in [
            (Shell::Bash, "rm -rf build", true),
            (Shell::Bash, "ls -la && git status", false),
            (Shell::Bash, "bash", true),
            (Shell::Bash, "echo 'unterminated", true),
            (Shell::PowerShell, "Remove-Item build -Recurse -Force", true),
            (Shell::PowerShell, "Get-ChildItem", false),
            (Shell::PowerShell, "powershell", true),
        ] {
            assert_eq!(
                flagged(shell, text),
                Action::Command { flagged: expected },
                "{shell:?} {text}"
            );
        }
    }

    #[test]
    fn valid_powershell_and_bash_syntax_is_not_flagged_by_posix_shlex() {
        // Catches POSIX `shlex::split` misparsing valid PowerShell paths, backtick escapes, block
        // comments, and Bash comments with apostrophes as syntax errors while still flagging real
        // unterminated quotes.
        for (shell, text, expected) in [
            (Shell::PowerShell, r"Get-ChildItem C:\", false),
            (Shell::PowerShell, r#"Write-Output "a`"b""#, false),
            (Shell::PowerShell, "<# don't #> Get-ChildItem", false),
            (Shell::PowerShell, "git log --format=oneline", false),
            (
                Shell::PowerShell,
                "$b = 'main'; git log --format=oneline $b",
                false,
            ),
            (Shell::Bash, "echo ok # don't", false),
            (Shell::Bash, "echo 'unterminated", true),
            (Shell::PowerShell, "Write-Output 'unterminated", true),
            (
                Shell::PowerShell,
                r"Write-Output `'; Remove-Item -Recurse -Force .\src",
                true,
            ),
            (
                Shell::PowerShell,
                r"<#'#> $x = 1; Remove-Item -Recurse -Force .\src",
                true,
            ),
            (Shell::PowerShell, "$x = 1; powershell", true),
            (
                Shell::PowerShell,
                "Write-Output \u{2019}; Remove-Item -Recurse -Force .\\src",
                true,
            ),
        ] {
            assert_eq!(
                flagged(shell, text),
                Action::Command { flagged: expected },
                "{shell:?} {text}"
            );
        }
    }

    #[test]
    fn a_request_without_readable_command_text_is_invalid() {
        for input in [
            json!({}),
            json!({ "command": "  " }),
            json!({ "command": 3 }),
            json!({ "command": "ls\0" }),
        ] {
            assert_eq!(action(Shell::Bash, &input), Action::Invalid, "{input}");
        }
    }
}
