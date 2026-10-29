use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{FileChange, TurnId};

/// An approval's id, issued by the engine. A provider's own request id lives only as long as its
/// process, so it never leaves the engine.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApprovalId(String);

impl ApprovalId {
    pub fn new(id: String) -> ApprovalId {
        ApprovalId(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ApprovalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Something an agent wants to do that waits for the user's decision (D83).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub id: ApprovalId,
    pub turn_id: TurnId,
    pub request: ApprovalRequest,
    /// Why Kratos asks instead of deciding by the thread's permission.
    pub why: AskReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ApprovalRequest {
    Command {
        command: String,
        cwd: Option<String>,
        /// The agent's own reason, such as why it needs to run outside the sandbox.
        reason: Option<String>,
        access: ExtraAccess,
    },
    FileChanges {
        changes: Vec<FileChange>,
        reason: Option<String>,
    },
    /// Extra permissions for the rest of the turn.
    Permissions {
        reason: Option<String>,
        access: ExtraAccess,
    },
}

/// Access beyond the thread's sandbox that accepting an approval grants. Empty when it grants
/// none.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraAccess {
    pub paths: Vec<PathAccess>,
    pub network: Option<NetworkAccess>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathAccess {
    /// A path, a glob pattern, or a location the provider names, such as Codex's `:root` for
    /// every file or `:tmpdir` for the temporary folder.
    pub path: String,
    pub access: PathAccessMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PathAccessMode {
    Read,
    Write,
    /// Kept closed, even inside a path the grant opens.
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum NetworkAccess {
    Any,
    Host { host: String, protocol: String },
}

/// Why a request reached the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "reason",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AskReason {
    /// The thread's permission is Ask me, or the thread is planning: every action asks (D84).
    AskEveryAction,
    /// The dangerous-command check flagged the command, which asks under every permission (D18).
    Dangerous,
    /// Kratos cannot check what the request will do: the command does not parse, it opens a shell
    /// or interpreter whose later input never reaches Kratos, or a patch's files are unknown.
    Unchecked,
    /// The agent asks to run outside the sandbox, for more permissions, or for network access.
    Escalation,
    /// The change touches a folder only the user may let an agent write (D20).
    ProtectedFolder { path: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Accept,
    /// Accept this and the same request again for the rest of the provider session.
    AcceptForSession,
    /// Say no; the agent continues its turn.
    Decline,
    /// Say no and stop the turn.
    Cancel,
}

/// The body of `POST /api/approvals/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecideApproval {
    pub decision: ApprovalDecision,
}

/// What became of an approval. A decision on one device closes the request everywhere, and a
/// later decision gets this recorded outcome back (D83).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ApprovalOutcome {
    Waiting,
    Decided {
        decision: ApprovalDecision,
    },
    /// The request ended without a decision, for example because its turn ended.
    Closed {
        reason: String,
    },
}

/// A question's id, issued by the engine.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QuestionId(String);

impl QuestionId {
    pub fn new(id: String) -> QuestionId {
        QuestionId(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for QuestionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Questions an agent asks the user in the middle of a turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: QuestionId,
    pub turn_id: TurnId,
    pub items: Vec<QuestionItem>,
    /// The turn waits for the answer.
    pub blocking: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionItem {
    pub id: String,
    pub header: String,
    pub question: String,
    /// The choices offered, when the agent offered any.
    pub options: Vec<QuestionOption>,
    /// The user may answer in their own words.
    pub allows_other: bool,
    /// The answer is a secret, such as a password.
    pub secret: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// What became of a question. As with approvals, the first answer wins and a later one gets this
/// back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum QuestionOutcome {
    Waiting,
    Answered,
    Closed { reason: String },
}

/// The body of `POST /api/questions/{id}`: answers by question item id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnswerQuestion {
    pub answers: BTreeMap<String, Vec<String>>,
}
