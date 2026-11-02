use std::fmt;

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

use crate::{
    Approval, Message, PauseReason, Permission, Provider, Question, RequiredText, Seq, Timestamp,
    TurnId, WorkspaceId,
};

/// A thread's id. The engine issues UUIDs. Any string parses, because a thread that does not exist
/// answers 404.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(String);

impl ThreadId {
    pub fn new(id: String) -> ThreadId {
        ThreadId(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateThread {
    #[serde(deserialize_with = "trimmed_workspace_id")]
    pub workspace_id: WorkspaceId,
    pub title: RequiredText,
    pub provider: Provider,
    /// The defaults when absent.
    #[serde(default)]
    pub settings: Option<ThreadSettings>,
}

/// What a thread's next turn runs with. A change applies from the next turn, never to the turn
/// running now (D87).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadSettings {
    /// `None` runs the provider's default model.
    pub model: Option<String>,
    pub permission: Permission,
    /// The agent plans and changes nothing until the plan is approved.
    pub plan_mode: bool,
    /// How many tokens of context the agent may fill before the provider compacts it (D76).
    /// `None` keeps the provider's own limit. Never above the window the provider reported.
    pub context_limit_tokens: Option<u64>,
}

impl Default for ThreadSettings {
    fn default() -> ThreadSettings {
        ThreadSettings {
            model: None,
            permission: Permission::AskMe,
            plan_mode: false,
            context_limit_tokens: None,
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

    /// These settings with no more access than `limit`.
    pub fn limited_to(&self, limit: AccessLimit) -> ThreadSettings {
        ThreadSettings {
            permission: self.permission.min(limit.permission),
            plan_mode: self.plan_mode || limit.plan_mode,
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
        other.permission <= self.permission && (other.plan_mode || !self.plan_mode)
    }
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
