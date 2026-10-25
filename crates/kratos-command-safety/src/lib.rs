//! Codex's dangerous-command check, so Kratos flags the commands Codex flags, on every command an
//! agent asks to run and under every permission (D18).
//!
//! Every file under `src/` except this one and `interactive.rs` is copied from openai/codex
//! rust-v0.159.1 under Apache-2.0 (`LICENSE-APACHE`, `NOTICE-CODEX`). Each says where it came from
//! and what changed.
//! [`dangerous_command`] follows Codex's exec policy, `core/src/exec_policy.rs`, which decides
//! which words the check sees.

// The copied files stay as Codex wrote them, so these allow what Kratos's lints would change: a
// form Codex's lint settings accept, and helpers that only the copied tests call.
#[allow(clippy::question_mark, reason = "copied from Codex unchanged")]
mod bash;
#[allow(dead_code, reason = "only the copied tests call some helpers")]
mod command_safety;
mod interactive;
#[allow(dead_code, reason = "only the copied tests call some helpers")]
mod powershell;
mod shell_detect;

use command_safety::is_dangerous_command::{
    dangerous_command_match_for_platform, dangerous_powershell_words_match,
};

pub use command_safety::is_dangerous_command::{DangerousCommandMatch, DangerousCommandPlatform};

/// The rule `command` breaks, judged with `platform`'s command semantics, or `None` when it breaks
/// none. `None` does not prove a command is safe; it means the check found nothing.
///
/// As in Codex: a `bash -lc` script of plain commands is checked one command at a time, a Windows
/// PowerShell `-Command` body is lowered into commands and checked with the PowerShell rules, and
/// anything else is checked as one command, which still finds literal commands inside a complex
/// shell script.
pub fn dangerous_command(
    command: &[String],
    platform: DangerousCommandPlatform,
) -> Option<DangerousCommandMatch> {
    if let Some(commands) = bash::parse_shell_lc_plain_commands(command)
        && !commands.is_empty()
    {
        return commands
            .iter()
            .find_map(|command| dangerous_command_match_for_platform(command, platform));
    }
    if platform == DangerousCommandPlatform::Windows
        && let Some(commands) = powershell::parse_powershell_command_into_plain_commands(command)
        && !commands.is_empty()
    {
        return commands
            .iter()
            .find_map(|command| dangerous_powershell_words_match(command, platform));
    }
    dangerous_command_match_for_platform(command, platform)
}

/// The words of the command text Codex puts in an approval request, which it joins with shell
/// quoting. `None` when the text is not valid shell quoting: the caller cannot check that command
/// and must treat it as unchecked, never as safe.
pub fn split_command_text(text: &str) -> Option<Vec<String>> {
    shlex::split(text)
}

/// Whether `command` starts a shell or an interpreter that reads commands from its input, such as
/// a bare `bash` or `python`. Text an agent later types into that process never reaches Kratos, so
/// Kratos cannot check those commands. A `bash -lc` script or a PowerShell `-Command` body is
/// checked command by command. `false` does not prove a command reads no input.
pub fn reads_commands_from_input(command: &[String]) -> bool {
    let commands = bash::parse_shell_lc_literal_commands(command)
        .or_else(|| powershell::parse_powershell_command_into_plain_commands(command))
        .unwrap_or_else(|| vec![command.to_vec()]);
    commands.iter().any(|words| interactive::reads_input(words))
}
