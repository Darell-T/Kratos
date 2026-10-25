// Copied from openai/codex rust-v0.159.1, codex-rs/shell-command/src/command_safety/mod.rs, without
// the test-only module that checks the tree-sitter lowering against a real PowerShell process.

mod powershell_tree_sitter;

pub mod is_dangerous_command;
pub(crate) use powershell_tree_sitter::try_parse_powershell_commands;
