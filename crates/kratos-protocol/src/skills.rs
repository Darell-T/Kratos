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

/// The body of `GET /api/threads/{id}/skills`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSkills {
    pub skills: Vec<Skill>,
    /// Why no slash commands are listed, when the CLI's protocol does not report them. Kratos does
    /// not read the CLI's private files to fill the gap.
    pub slash_commands_gap: Option<String>,
}
