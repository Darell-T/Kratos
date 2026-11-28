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
    if platform == DangerousCommandPlatform::Windows
        && let Some(commands) = parse_powershell_command_literal_commands(command)
        && let Some(found) = commands
            .iter()
            .find_map(|command| dangerous_powershell_words_match(command, platform))
    {
        return Some(found);
    }
    dangerous_command_match_for_platform(command, platform)
}

/// The words of the command text Codex puts in an approval request, which it joins with shell
/// quoting. `None` when the text is not valid shell quoting: the caller cannot check that command
/// and must treat it as unchecked, never as safe.
pub fn split_command_text(text: &str) -> Option<Vec<String>> {
    shlex::split(text)
}

/// Whether `script` fails to parse cleanly as Bash syntax.
pub fn has_bash_syntax_error(script: &str) -> bool {
    bash::try_parse_shell(script).is_none_or(|tree| tree.root_node().has_error())
}

/// Whether `script` fails to parse cleanly as PowerShell syntax.
pub fn has_powershell_syntax_error(script: &str) -> bool {
    parse_powershell_tree(script).is_none()
}

/// Whether `command` starts a shell or an interpreter that reads commands from its input, such as
/// a bare `bash` or `python`. Text an agent later types into that process never reaches Kratos, so
/// Kratos cannot check those commands. A `bash -lc` script or a PowerShell `-Command` body is
/// checked command by command. `false` does not prove a command reads no input.
pub fn reads_commands_from_input(command: &[String]) -> bool {
    let commands = bash::parse_shell_lc_literal_commands(command)
        .or_else(|| powershell::parse_powershell_command_into_plain_commands(command))
        .or_else(|| parse_powershell_command_literal_commands(command))
        .unwrap_or_else(|| vec![command.to_vec()]);
    commands.iter().any(|words| interactive::reads_input(words))
}

fn parse_powershell_tree(script: &str) -> Option<tree_sitter::Tree> {
    if script
        .chars()
        .any(|ch| matches!(ch, '‘' | '’' | '“' | '”' | '–' | '—' | '―'))
    {
        return None;
    }
    let masked = mask_double_dash_equals(script);
    let mut parser = tree_sitter::Parser::new();
    let language = tree_sitter_powershell::LANGUAGE.into();
    parser
        .set_language(&language)
        .expect("load powershell grammar");
    let tree = parser.parse(&masked, None)?;
    (!tree.root_node().has_error()).then_some(tree)
}

/// tree-sitter-powershell rejects bare `--flag=value`; replacing `=` with a space preserves byte
/// offsets so literal extraction still reads the original text from `script`.
fn mask_double_dash_equals(script: &str) -> String {
    let mut bytes = script.as_bytes().to_vec();
    let mut start = 0;
    for (index, ch) in script.char_indices() {
        if ch.is_whitespace() || "|&;#><(){}".contains(ch) {
            mask_token_equals(&mut bytes, script, start, index);
            start = index + ch.len_utf8();
        }
    }
    mask_token_equals(&mut bytes, script, start, script.len());
    String::from_utf8(bytes).expect("replacing ASCII `=` with space preserves UTF-8")
}

fn mask_token_equals(bytes: &mut [u8], script: &str, start: usize, end: usize) {
    let token = &script[start..end];
    if token.starts_with("--")
        && !token
            .chars()
            .any(|ch| matches!(ch, '\'' | '"' | '`' | '$' | '#'))
        && let Some(equals) = token.find('=')
        && equals > 2
        && equals + 1 < token.len()
    {
        bytes[start + equals] = b' ';
    }
}

fn parse_powershell_command_literal_commands(command: &[String]) -> Option<Vec<Vec<String>>> {
    let (_, script) = powershell::extract_powershell_command(command)?;
    let tree = parse_powershell_tree(script)?;
    let mut commands = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "command"
            && let Some(words) = literal_powershell_words(node, script)
        {
            commands.push(words);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    Some(commands)
}

fn literal_powershell_words(command: tree_sitter::Node<'_>, script: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cursor = command.walk();
    for child in command.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                let text = child.utf8_text(script.as_bytes()).ok()?;
                words.push(unquote_powershell_token(text));
            }
            "command_elements" => {
                let mut elem_cursor = child.walk();
                for elem in child.named_children(&mut elem_cursor) {
                    if matches!(
                        elem.kind(),
                        "command_argument_sep"
                            | "redirection"
                            | "sub_expression"
                            | "parenthesized_expression"
                            | "script_block_expression"
                    ) {
                        continue;
                    }
                    if let Ok(text) = elem.utf8_text(script.as_bytes()) {
                        let word = unquote_powershell_token(text);
                        if !word.is_empty() {
                            words.push(word);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (!words.is_empty()).then_some(words)
}

fn unquote_powershell_token(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    {
        return inner.replace("''", "'");
    }
    if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return unescape_powershell_backticks(inner);
    }
    unescape_powershell_backticks(trimmed)
}

fn unescape_powershell_backticks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '`'
            && let Some(next) = chars.next()
        {
            out.push(next);
        } else {
            out.push(ch);
        }
    }
    out
}
