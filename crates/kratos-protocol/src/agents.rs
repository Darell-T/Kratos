//! Agent types that the engine and the Teams rules both read. The Teams rules depend on this crate,
//! not on the engine.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The agent CLI a thread runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Codex,
    Claude,
}

impl Provider {
    /// The name in JSON and in the `provider` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Claude => "claude",
        }
    }

    /// The provider that [`Provider::as_str`] names `name`, for text read back from the records.
    pub fn from_name(name: &str) -> Option<Provider> {
        [Provider::Codex, Provider::Claude]
            .into_iter()
            .find(|provider| provider.as_str() == name)
    }
}

/// How much a thread's agent may do without asking. `ApproveForMe` is not `FullAccess`, and a
/// command the dangerous-command check flags asks the user under every permission. The variants
/// run from least to most trusting, so `min` picks the narrower of two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Permission {
    AskMe,
    ApproveForMe,
    FullAccess,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnChanges {
    /// Files the turn edited, as the provider reported their paths.
    pub files: Vec<String>,
    /// The turn edited a file after its last command, so no command has checked that edit.
    pub edited_since_command: bool,
    pub ran_command: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CommandResult {
    /// On Windows a crashed process reports its NTSTATUS, which reads as a negative `i32`.
    Exited {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    TimedOut {
        output: String,
    },
    /// The sandbox refused the command, for example a write outside the thread's roots.
    Refused {
        error: String,
    },
}

/// The narrower sandbox a Team gives a member's thread. A thread without one runs by its permission
/// alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadSandbox {
    /// Writable besides the thread's own folder.
    pub extra_writable_roots: Vec<String>,
    pub outside_writes: OutsideWrites,
    /// Set for every command the thread runs, for example a package cache inside a writable root.
    pub env: BTreeMap<String, String>,
}

/// What happens to a write outside a sandboxed thread's roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutsideWrites {
    /// The user decides.
    Ask,
    /// Refused without asking.
    Deny,
}
