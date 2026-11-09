//! The agent CLIs the engine drives: Codex and Claude Code. Each runs as an actor of its own, and
//! what they share is here: the errors a provider returns, what became of a sent message, and the
//! queue rules.

mod changes;
pub mod claude;
pub mod codex;
pub(crate) mod queue;

use kratos_protocol::{ApprovalId, Message, MessageId, QuestionId, ThreadId};
use kratos_store::StoreError;

/// The most undelivered messages one thread holds (D85).
pub(crate) const MAX_PENDING_MESSAGES: usize = 10;

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
