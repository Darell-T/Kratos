use std::fmt;

use serde::de::{Deserializer, Error};
use serde::{Deserialize, Serialize};

use crate::{ContinuationId, RequiredText, Timestamp, TurnId};

/// A message's id. A client may choose it, so that sending the same message again after a lost
/// reply finds the first one instead of queueing a copy (D85); otherwise the engine issues a UUID.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct MessageId(String);

/// Longer ids are refused, so an id never grows a row past a page.
pub const MAX_MESSAGE_ID_CHARS: usize = 128;

impl MessageId {
    /// For ids the engine issued or read from its records.
    pub fn new(id: String) -> MessageId {
        MessageId(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for MessageId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        if id.is_empty() || id.chars().count() > MAX_MESSAGE_ID_CHARS {
            return Err(D::Error::custom(format!(
                "must be 1 to {MAX_MESSAGE_ID_CHARS} characters"
            )));
        }
        Ok(MessageId(id))
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One part of a message (D86). Paths are absolute paths on the engine's machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MessageInput {
    Text {
        text: RequiredText,
    },
    /// An image file the agent sees.
    Image {
        path: RequiredText,
    },
    /// A file the agent is pointed at, by name and path.
    File {
        path: RequiredText,
    },
}

/// Who wrote a message. A continuation is the message Kratos sends to resume a turn that stopped at a
/// usage limit (D39), so the record shows it was not the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum MessageOrigin {
    User,
    Continuation { continuation_id: ContinuationId },
}

/// Where a message stands. A message is never sent twice (D11): once its delivery is `unknown`,
/// only the user can discard it, and nothing behind it starts until they do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum MessageStatus {
    /// Waiting for the turn before it to end.
    Queued,
    /// Sent; the provider has not yet said whether a turn took it.
    Submitting,
    Accepted {
        turn_id: TurnId,
    },
    /// The provider refused it before acting on it, so sending it again is safe.
    Failed {
        error: String,
    },
    /// Kratos cannot tell whether the provider acted on it.
    Unknown {
        error: String,
    },
    /// The user removed it, or discarded an uncertain delivery.
    Cancelled,
}

/// A message in a thread's queue, as sending it and the thread's details report it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: MessageId,
    pub input: Vec<MessageInput>,
    #[serde(flatten)]
    pub origin: MessageOrigin,
    #[serde(flatten)]
    pub status: MessageStatus,
    pub queued_at: Timestamp,
}

/// The body of `POST /api/threads/{id}/messages` to a thread whose provider Kratos drives: `text`,
/// or `input` parts, with an optional `messageId`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendMessage {
    #[serde(default)]
    pub message_id: Option<MessageId>,
    #[serde(default)]
    pub text: Option<RequiredText>,
    #[serde(default)]
    pub input: Option<Vec<MessageInput>>,
}

impl SendMessage {
    /// The message's parts and the id the client chose. `Err` names what is wrong with the body.
    pub fn into_parts(self) -> Result<(Option<MessageId>, Vec<MessageInput>), &'static str> {
        match (self.text, self.input) {
            (Some(text), None) => Ok((self.message_id, vec![MessageInput::Text { text }])),
            (None, Some(input)) if !input.is_empty() => Ok((self.message_id, input)),
            (None, Some(_)) => Err("input: must not be empty"),
            (Some(_), Some(_)) => Err("send either text or input, not both"),
            (None, None) => Err("missing field `text` or `input`"),
        }
    }
}

/// The body of a 429 when a thread's queue is full (D85): the client may send the same message,
/// with the same `messageId`, again later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueFull {
    pub error: String,
    pub queued: usize,
    pub limit: usize,
}
