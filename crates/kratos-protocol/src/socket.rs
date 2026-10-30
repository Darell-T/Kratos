//! The event socket: what a client sends to follow threads, and what the engine sends back.

use serde::{Deserialize, Serialize};

use crate::{AgentItem, ContextUsage, Seq, ThreadEvent, ThreadId, TurnId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    /// Sends the thread's events after `after`, in order, then each new one as it is appended.
    /// Subscribing again to the same thread on the same socket replaces the earlier subscription.
    Subscribe {
        thread_id: ThreadId,
        after: Seq,
    },
    Unsubscribe {
        thread_id: ThreadId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    Event {
        event: ThreadEvent,
    },
    Error {
        message: String,
    },
    /// Progress the log does not keep, such as a reply as it is written. A client that misses some
    /// gets a `snapshot` instead, and the finished item arrives as an `item.completed` event.
    Live {
        thread_id: ThreadId,
        update: LiveUpdate,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LiveUpdate {
    /// More text of a reply the agent is writing.
    ReplyDelta {
        turn_id: TurnId,
        item_id: String,
        delta: String,
    },
    /// Every reply of the running turn so far, sent when a client subscribes or falls behind.
    Snapshot {
        turn_id: Option<TurnId>,
        replies: Vec<ReplySoFar>,
    },
    /// An item the agent started, such as a command now running.
    ItemStarted { turn_id: TurnId, item: AgentItem },
    Context {
        turn_id: TurnId,
        usage: ContextUsage,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplySoFar {
    pub item_id: String,
    pub text: String,
}
