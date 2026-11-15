//! Claude Code's own shell tools. The CLI runs each command itself, as the user, with no sandbox,
//! so Kratos's decision and the dangerous-command check are the only guards (D18, D84).

use kratos_claude_client::CanUseTool;
use kratos_command_safety::{
    DangerousCommandPlatform, dangerous_command, reads_commands_from_input, split_command_text,
};
use kratos_protocol::{ApprovalRequest, ExtraAccess};
use serde_json::Value;

use super::policy::Action;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shell {
    /// `Bash`: a POSIX shell script, run by Git Bash on Windows.
    Bash,
    /// `PowerShell`: a PowerShell script, on every system that has one.
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

    /// The command line the check reads: the text wrapped the way the shell runs it, judged by the
    /// rules of the language the text is written in.
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

    /// Whether the dangerous-command check flags `text`. Text the check cannot read, or that starts
    /// a shell reading commands Kratos never sees, counts as flagged.
    fn flags(self, text: &str) -> bool {
        let (words, platform) = self.words(text);
        dangerous_command(&words, platform).is_some()
            || reads_commands_from_input(&words)
            || split_command_text(text).is_none()
    }
}

/// The command text of a shell tool request, when it is one Kratos can judge.
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
        // Catches a shell tool's text reaching the check in a form it cannot read, so a forced
        // delete runs unasked under Approve for me and Full access.
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
    fn a_request_without_readable_command_text_is_invalid() {
        // Catches an empty, missing, or NUL-carrying command being judged as an ordinary one.
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
