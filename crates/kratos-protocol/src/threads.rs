use std::collections::BTreeMap;

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

use crate::{
    Approval, ContextLimit, Message, PauseReason, Question, RequiredText, Seq, Timestamp, TurnId,
    WorkspaceId,
};

string_id! {
    /// A thread's id. The engine issues UUIDs. Any string parses, because a thread that does not
    /// exist answers 404.
    #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
    ThreadId
}

/// One task run by one agent on this machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: ThreadId,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub provider: Provider,
    pub created_at: Timestamp,
    /// The `at` of the thread's latest event, or `created_at` before its first.
    pub updated_at: Timestamp,
}

/// The agent CLI a thread runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Codex,
    Claude,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Codex, Provider::Claude];

    /// The name in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Claude => "claude",
        }
    }
}

/// A new provider breaks this match, as a reminder to add it to [`Provider::ALL`].
const _: () = match Provider::Codex {
    Provider::Codex | Provider::Claude => {}
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateThread {
    #[serde(deserialize_with = "trimmed_workspace_id")]
    pub workspace_id: WorkspaceId,
    pub title: RequiredText,
    pub provider: Provider,
    /// The defaults when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<ThreadSettings>,
}

impl CreateThread {
    pub fn settings_or_default(&self) -> ThreadSettings {
        self.settings.clone().unwrap_or_default()
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

impl Permission {
    /// Every permission, least trusting first.
    pub const ALL: [Permission; 3] = [
        Permission::AskMe,
        Permission::ApproveForMe,
        Permission::FullAccess,
    ];
}

/// A new permission breaks this match, as a reminder to add it to [`Permission::ALL`].
const _: () = match Permission::AskMe {
    Permission::AskMe | Permission::ApproveForMe | Permission::FullAccess => {}
};

/// What a thread's next turn runs with. A change applies from the next turn, never to the turn
/// running now (D87).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadSettings {
    /// `None` runs the provider's default model.
    pub model: Option<RequiredText>,
    pub permission: Permission,
    /// The agent plans and changes nothing until the plan is approved.
    pub plan_mode: bool,
    /// How many tokens of context the agent may fill before the provider compacts it (D76).
    /// `None` keeps the provider's own limit. Never above the window the provider reported.
    pub context_limit_tokens: Option<ContextLimit>,
    /// The shape the agent's final reply must take, such as a Teams reviewer's report. Plan mode
    /// ignores it, since the plan is that turn's reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_format: Option<ReportFormat>,
}

/// A JSON Schema object the agent's final reply must match. A completed turn under one ends with
/// an [`AgentItem::Report`](crate::AgentItem::Report) holding the parsed reply, whichever provider
/// ran it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReportFormat(serde_json::Map<String, serde_json::Value>);

impl ReportFormat {
    pub fn new(schema: serde_json::Map<String, serde_json::Value>) -> ReportFormat {
        ReportFormat(schema)
    }

    pub fn schema(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.0
    }
}

impl Default for ThreadSettings {
    fn default() -> ThreadSettings {
        ThreadSettings {
            model: None,
            permission: Permission::AskMe,
            plan_mode: false,
            context_limit_tokens: None,
            report_format: None,
        }
    }
}

impl ThreadSettings {
    pub fn access(&self) -> AccessLimit {
        AccessLimit {
            permission: self.permission,
            plan_mode: self.plan_mode,
        }
    }

    /// The report format a turn under these settings follows. Plan mode has none, since the plan
    /// is that turn's reply.
    pub fn turn_report_format(&self) -> Option<&ReportFormat> {
        self.report_format.as_ref().filter(|_| !self.plan_mode)
    }

    /// These settings with no more access than `limit`.
    pub fn limited_to(&self, limit: AccessLimit) -> ThreadSettings {
        let access = self.access().intersect(limit);
        ThreadSettings {
            permission: access.permission,
            plan_mode: access.plan_mode,
            ..self.clone()
        }
    }
}

/// How much a thread may do: its permission, and whether it only plans. A thread that takes a
/// message from another thread drops to no more than the sender's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessLimit {
    pub permission: Permission,
    pub plan_mode: bool,
}

impl AccessLimit {
    pub const FULL: AccessLimit = AccessLimit {
        permission: Permission::FullAccess,
        plan_mode: false,
    };

    /// Whether `other` is no more than this: no higher permission, and planning if this plans.
    pub fn covers(self, other: AccessLimit) -> bool {
        self.intersect(other) == other
    }

    /// The most access both allow: the lower permission, and planning if either plans. Narrowing
    /// and the `covers` check both use this one order, so they cannot disagree.
    fn intersect(self, other: AccessLimit) -> AccessLimit {
        AccessLimit {
            permission: self.permission.min(other.permission),
            plan_mode: self.plan_mode || other.plan_mode,
        }
    }
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

fn trimmed_workspace_id<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<WorkspaceId, D::Error> {
    RequiredText::deserialize(deserializer).map(|id| WorkspaceId::new(id.into_string()))
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListThreads {
    pub workspace_id: WorkspaceId,
}

/// The query of `GET /api/threads/{id}/events`. Without `after`, the page starts at the first
/// event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListEvents {
    #[serde(default)]
    pub after: Option<Seq>,
}

/// The body of `GET /api/threads/{id}`: the thread and what its next turn waits on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDetails {
    #[serde(flatten)]
    pub thread: Thread,
    pub settings: ThreadSettings,
    pub paused: Option<PauseReason>,
    pub running_turn: Option<TurnId>,
    /// Messages not yet accepted or cancelled, oldest first.
    pub queue: Vec<Message>,
    pub approvals: Vec<Approval>,
    pub questions: Vec<Question>,
    /// The thread's newest event when these details were read, so a client that reads the events
    /// after it misses nothing that changed them.
    pub last_seq: Seq,
}
