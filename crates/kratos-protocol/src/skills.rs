//! What a thread's agent CLI offers in the thread's folder (D97).

use serde::{Deserialize, Serialize};

/// A skill the agent CLI found for the thread's folder, the project's own included.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: String,
    pub enabled: bool,
}

/// A slash command the agent CLI reports for the thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    /// What the command takes, such as `<file>`; empty when it takes nothing.
    pub argument_hint: String,
    /// Other names that run the command.
    pub aliases: Vec<String>,
    /// True for the CLI's own commands, false for one a user, plugin, or MCP server defines.
    pub builtin: bool,
}

/// The body of `GET /api/threads/{id}/skills`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSkills {
    pub skills: Vec<Skill>,
    /// The slash commands the CLI's protocol reports. Claude lists its skills among them.
    #[serde(default)]
    pub slash_commands: Vec<SlashCommand>,
    /// Why the listing is incomplete or empty, when it is. Kratos does not read the CLI's private
    /// files to fill the gap.
    pub slash_commands_gap: Option<String>,
}
