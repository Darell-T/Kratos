//! The agent CLIs the engine drives: Codex and Claude Code. Each runs as an actor of its own, and
//! what they share is here: the errors a provider returns, what became of a sent message, and the
//! queue rules.

mod agent;
mod changes;
pub mod claude;
pub mod codex;
pub(crate) mod queue;
pub(crate) mod writes;

pub(crate) use agent::Agent;
pub use agent::Providers;

use kratos_protocol::{
    AgentItem, ApprovalId, Message, MessageId, Place, QuestionId, ThreadId, TurnId,
};
use kratos_store::StoreError;

use crate::store_thread::StoreThread;
use crate::worktrees::check_still_there;

/// The most undelivered messages one thread holds (D85).
pub(crate) const MAX_PENDING_MESSAGES: usize = 10;

/// Why the thread's next turn may not start: Kratos made the worktree it works in, and the worktree
/// is gone from its path, unlocked in git, or replaced by another checkout, so the agent would work
/// in someone else's checkout. Checked as each turn starts, since the user may replace a worktree
/// at any time.
pub(crate) async fn worktree_gone(store: &StoreThread, place: &Place) -> Option<String> {
    check_still_there(store, place)
        .await
        .err()
        .map(|error| error.to_string())
}

/// The item a completed turn under a report format ends with. No provider gives the report an id,
/// so it takes one from its turn.
pub(crate) fn report_item(turn_id: &TurnId, report: serde_json::Value) -> AgentItem {
    AgentItem::Report {
        id: format!("{turn_id}-report"),
        report,
    }
}

/// What became of a continuation the scheduler sent (D39).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Continued {
    Queued,
    /// The continuation was cancelled or sent before; there is nothing to send.
    NotOpen,
    /// The thread cannot continue on its own, and the text says why: the user decides.
    NeedsUser(String),
}

/// What became of a message sent to a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sent {
    Queued(Message),
    /// The client sent an id the thread already has; this is that message.
    Existing(Message),
    /// The thread's queue is full; nothing was stored.
    Full {
        pending: usize,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Unknown thread {0}")]
    UnknownThread(ThreadId),
    #[error("Unknown message {0}")]
    UnknownMessage(MessageId),
    #[error("Unknown approval {0}")]
    UnknownApproval(ApprovalId),
    #[error("Unknown question {0}")]
    UnknownQuestion(QuestionId),
    /// The request does not fit the thread's state, for example a retry of a message that did not
    /// fail. The text says what would.
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Forbidden(String),
    /// The setting is not one the pinned CLI supports, or is out of its reported bounds (D76).
    #[error("{0}")]
    Unsupported(String),
    /// The agent cannot run: its CLI is missing, failed to start, or keeps exiting.
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("The provider has stopped")]
    Stopped,
}
