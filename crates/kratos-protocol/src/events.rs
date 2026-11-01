use serde::{Deserialize, Serialize};

use crate::{RequiredText, ThreadId, Timestamp};

/// A position in one thread's event log.
///
/// A thread's events count up from 1 with no gaps, and a number, once committed, never names
/// another event. A client that has seen event `n` resumes with `after: n`. The cursor 0 means
/// "before the first event", so `after: 0` replays the whole log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(u64);

impl Seq {
    pub const BEFORE_FIRST: Seq = Seq(0);

    pub fn new(position: u64) -> Seq {
        Seq(position)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ThreadEventBody {
    #[serde(rename = "user.message")]
    UserMessage { text: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadEvent {
    pub thread_id: ThreadId,
    pub seq: Seq,
    /// When the engine committed the event. Read inside the append's transaction, so `at` never
    /// decreases along `seq` unless the wall clock steps back.
    pub at: Timestamp,
    pub event: ThreadEventBody,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PostMessage {
    pub text: RequiredText,
}

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
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    Event { event: ThreadEvent },
    Error { message: String },
}
