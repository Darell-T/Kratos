use serde::{Deserialize, Serialize};

/// The Claude Code release installed on the engine's machine: `GET /api/agents/claude/cli`, and
/// the reply to `POST /api/agents/claude/cli/update`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliRelease {
    /// `None` when the CLI's version could not be read; `update` says why.
    pub installed: Option<String>,
    /// The oldest release Kratos runs. A thread on an older one pauses until the CLI is updated.
    pub oldest_supported: String,
    pub update: CliUpdate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CliUpdate {
    Current,
    /// `POST /api/agents/claude/cli/update` installs `latest`.
    Available {
        latest: String,
    },
    /// Kratos cannot tell whether a newer release is out, and `reason` says why. The update route
    /// still runs the native install's updater, and refuses any other install.
    Unknown {
        reason: String,
    },
}
