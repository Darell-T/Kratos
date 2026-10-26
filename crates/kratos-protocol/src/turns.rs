use std::fmt;

use serde::{Deserialize, Serialize};

/// A turn's id, as the provider issued it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    pub fn new(id: String) -> TurnId {
        TurnId(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// How a turn ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "outcome",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TurnOutcome {
    Completed,
    /// The user stopped it, or Kratos did at its deadline.
    Interrupted,
    Failed {
        error: String,
    },
}

/// Text Kratos keeps only the start of, because stored events have a size budget (L3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CappedText {
    pub text: String,
    /// How many bytes were left out after `text`. 0 when nothing was.
    pub omitted_bytes: u64,
}

impl CappedText {
    /// Keeps at most `max_bytes` of `text`, cut at a character boundary.
    pub fn new(text: &str, max_bytes: usize) -> CappedText {
        let mut end = text.len().min(max_bytes);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        CappedText {
            text: text[..end].to_owned(),
            omitted_bytes: (text.len() - end) as u64,
        }
    }
}

/// Where a command or file change stands. A stored item is finished; `InProgress` appears only in
/// live updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    /// The user or Kratos said no, so it never ran.
    Declined,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FileChangeKind {
    Add,
    Delete,
    Update { move_path: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: String,
    #[serde(flatten)]
    pub kind: FileChangeKind,
    pub diff: CappedText,
}

/// A finished piece of an agent's turn, in Kratos's own terms, so every provider's history reads the
/// same way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentItem {
    /// Text the agent wrote to the user. Saved whole: it is the reply (D88).
    Message { id: String, text: String },
    /// The plan a planning turn ends with.
    Plan { id: String, text: String },
    Command {
        id: String,
        command: String,
        cwd: Option<String>,
        status: ItemStatus,
        exit_code: Option<i32>,
        output: CappedText,
    },
    FileChanges {
        id: String,
        status: ItemStatus,
        changes: Vec<FileChange>,
    },
    /// An item Kratos does not describe yet, kept by its provider's name for it.
    Other { id: String, provider_type: String },
}

/// The context one turn used, as the provider reported it (D76). This is the conversation's
/// context, not the account's quota.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    /// Every token the turn counted.
    pub total_tokens: i64,
    /// The model's context window the provider works with, when it said.
    pub context_window: Option<i64>,
}
