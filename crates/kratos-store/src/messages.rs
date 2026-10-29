use kratos_protocol::{
    Message, MessageId, MessageInput, MessageOrigin, MessageStatus, ThreadEventBody, ThreadId,
    Timestamp, TurnId,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::json::{required_column, to_column};
use crate::{Store, StoreError, ThreadWrite};

/// Where a new message joins a thread's queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuePlace {
    Back,
    /// Ahead of every waiting message, for a continuation that carries on the turn before them.
    Front,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Enqueued {
    Added(Message),
    /// A message with this id is already in the thread, so the client sent it again. Nothing was
    /// written.
    Existing(Message),
    /// The queue already holds `pending` undelivered messages, the most it may. Nothing was
    /// written.
    Full {
        pending: usize,
    },
}

/// The status changes a message may make. Anything else is a defect in the caller.
///
/// A message whose delivery is uncertain only leaves `unknown` for `accepted`, when the provider's
/// history shows it, or `cancelled`, when the user discards it (D11).
const TRANSITIONS: [(&str, &[&str]); 6] = [
    ("queued", &["submitting", "cancelled"]),
    // Back to `queued` when the provider refused a steer before acting on it.
    ("submitting", &["accepted", "failed", "unknown", "queued"]),
    ("failed", &["queued", "cancelled"]),
    ("unknown", &["accepted", "cancelled"]),
    ("accepted", &[]),
    ("cancelled", &[]),
];

/// Statuses of a message the provider has not taken yet. They count against the queue limit.
const PENDING: &str = "'queued', 'submitting', 'failed', 'unknown'";

impl ThreadWrite<'_> {
    /// Adds a message to the thread's queue, unless the id is already in the thread or the queue
    /// already holds `max_pending` undelivered messages (D85). Both checks run inside this write,
    /// so two sends of one id store one message. `Err(MessageIdTaken)` when the thread holds the id
    /// with another message.
    pub fn enqueue_message(
        &mut self,
        id: &MessageId,
        input: Vec<MessageInput>,
        origin: MessageOrigin,
        place: QueuePlace,
        max_pending: usize,
    ) -> Result<Enqueued, StoreError> {
        if let Some(existing) = message(&self.transaction, self.thread_id(), id)? {
            if existing.input != input || existing.origin != origin {
                return Err(StoreError::MessageIdTaken(id.clone()));
            }
            return Ok(Enqueued::Existing(existing));
        }
        let (pending, position) = self.queue_slot(place)?;
        if pending >= max_pending {
            return Ok(Enqueued::Full { pending });
        }
        let message = Message {
            id: id.clone(),
            input,
            origin,
            status: MessageStatus::Queued,
            queued_at: self.at().clone(),
        };
        self.transaction.execute(
            "INSERT INTO thread_messages (thread_id, id, position, input, origin, status, queued_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
            params![
                self.thread_id().as_str(),
                id.as_str(),
                position,
                to_column(&message.input),
                to_column(&message.origin),
                message.queued_at.as_str(),
            ],
        )?;
        self.append(ThreadEventBody::MessageQueued {
            message_id: message.id.clone(),
            input: message.input.clone(),
            origin: message.origin.clone(),
        })?;
        Ok(Enqueued::Added(message))
    }

    /// How many undelivered messages the queue holds, and the position a new message at `place`
    /// takes. Positions only order the queue. A message at the front takes a position below every
    /// message's, delivered ones included, so it can never reuse one.
    fn queue_slot(&self, place: QueuePlace) -> Result<(usize, i64), StoreError> {
        let (pending, first, last): (usize, Option<i64>, Option<i64>) =
            self.transaction.query_row(
                &format!(
                    "SELECT
                       (SELECT COUNT(*) FROM thread_messages WHERE thread_id = ?1 AND status IN ({PENDING})),
                       (SELECT MIN(position) FROM thread_messages WHERE thread_id = ?1),
                       (SELECT MAX(position) FROM thread_messages WHERE thread_id = ?1)"
                ),
                [self.thread_id().as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let position = match (place, first, last) {
            (QueuePlace::Front, Some(first), _) => first - 1,
            (_, _, Some(last)) => last + 1,
            (_, _, None) => 1,
        };
        Ok((pending, position))
    }

    /// Moves a message to `status` and records the change. `Err(MessageTransition)` for a change
    /// [`TRANSITIONS`] does not allow, which writes nothing.
    pub fn set_message_status(
        &mut self,
        id: &MessageId,
        status: MessageStatus,
    ) -> Result<Message, StoreError> {
        let current = message(&self.transaction, self.thread_id(), id)?.ok_or_else(|| {
            StoreError::UnknownMessage {
                thread_id: self.thread_id().clone(),
                message_id: id.clone(),
            }
        })?;
        let (from, to) = (status_name(&current.status), status_name(&status));
        let allowed = TRANSITIONS
            .iter()
            .find(|(name, _)| *name == from)
            .is_some_and(|(_, next)| next.contains(&to));
        if !allowed {
            return Err(StoreError::MessageTransition {
                message_id: id.clone(),
                from,
                to,
            });
        }
        let (turn_id, error) = status_details(&status);
        self.transaction.execute(
            "UPDATE thread_messages SET status = ?1, turn_id = ?2, error = ?3
             WHERE thread_id = ?4 AND id = ?5",
            params![to, turn_id, error, self.thread_id().as_str(), id.as_str()],
        )?;
        self.append(ThreadEventBody::MessageStatusChanged {
            message_id: id.clone(),
            status: status.clone(),
        })?;
        Ok(Message { status, ..current })
    }

    /// The thread's undelivered messages, oldest first, as this write sees them.
    pub fn pending_messages(&self) -> Result<Vec<Message>, StoreError> {
        pending_messages(&self.transaction, self.thread_id())
    }
}

impl Store {
    pub fn message(
        &self,
        thread_id: &ThreadId,
        id: &MessageId,
    ) -> Result<Option<Message>, StoreError> {
        message(&self.connection, thread_id, id)
    }

    /// The messages the provider has not taken yet, in queue order: queued ones, the one being
    /// sent, and ones that failed or whose delivery is uncertain.
    pub fn pending_messages(&self, thread_id: &ThreadId) -> Result<Vec<Message>, StoreError> {
        pending_messages(&self.connection, thread_id)
    }

    /// Every message a provider was being sent when the engine stopped, by thread. After a restart
    /// their delivery is uncertain.
    pub fn submitting_messages(&self) -> Result<Vec<(ThreadId, MessageId)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT thread_id, id FROM thread_messages WHERE status = 'submitting'
             ORDER BY thread_id, position",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((ThreadId::new(row.get(0)?), MessageId::new(row.get(1)?)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

const COLUMNS: &str = "id, input, origin, status, turn_id, error, queued_at";

fn message(
    connection: &Connection,
    thread_id: &ThreadId,
    id: &MessageId,
) -> Result<Option<Message>, StoreError> {
    let message = connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM thread_messages WHERE thread_id = ?1 AND id = ?2"),
            params![thread_id.as_str(), id.as_str()],
            message_from_row,
        )
        .optional()?;
    Ok(message)
}

fn pending_messages(
    connection: &Connection,
    thread_id: &ThreadId,
) -> Result<Vec<Message>, StoreError> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM thread_messages WHERE thread_id = ?1 AND status IN ({PENDING})
         ORDER BY position"
    ))?;
    let messages = statement.query_map([thread_id.as_str()], message_from_row)?;
    Ok(messages.collect::<Result<_, _>>()?)
}

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<Message> {
    let status: String = row.get(3)?;
    let turn_id: Option<String> = row.get(4)?;
    let error: Option<String> = row.get(5)?;
    Ok(Message {
        id: MessageId::new(row.get(0)?),
        input: required_column(row, 1)?,
        origin: required_column(row, 2)?,
        status: status_from_columns(&status, turn_id, error).ok_or_else(|| {
            let reason = format!("message status {status} without its details");
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, reason.into())
        })?,
        queued_at: Timestamp::from_stored(row.get(6)?),
    })
}

fn status_name(status: &MessageStatus) -> &'static str {
    match status {
        MessageStatus::Queued => "queued",
        MessageStatus::Submitting => "submitting",
        MessageStatus::Accepted { .. } => "accepted",
        MessageStatus::Failed { .. } => "failed",
        MessageStatus::Unknown { .. } => "unknown",
        MessageStatus::Cancelled => "cancelled",
    }
}

/// The `turn_id` and `error` columns of a status.
fn status_details(status: &MessageStatus) -> (Option<&str>, Option<&str>) {
    match status {
        MessageStatus::Accepted { turn_id } => (Some(turn_id.as_str()), None),
        MessageStatus::Failed { error } | MessageStatus::Unknown { error } => {
            (None, Some(error.as_str()))
        }
        MessageStatus::Queued | MessageStatus::Submitting | MessageStatus::Cancelled => {
            (None, None)
        }
    }
}

fn status_from_columns(
    status: &str,
    turn_id: Option<String>,
    error: Option<String>,
) -> Option<MessageStatus> {
    Some(match status {
        "queued" => MessageStatus::Queued,
        "submitting" => MessageStatus::Submitting,
        "accepted" => MessageStatus::Accepted {
            turn_id: TurnId::new(turn_id?),
        },
        "failed" => MessageStatus::Failed { error: error? },
        "unknown" => MessageStatus::Unknown { error: error? },
        "cancelled" => MessageStatus::Cancelled,
        _ => return None,
    })
}
