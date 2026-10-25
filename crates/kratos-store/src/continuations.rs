//! The usage scheduler's durable plans to carry on turns that stopped at a usage limit (D39).
//!
//! A thread has at most one open continuation, `scheduled` or `awaitingUser`, which a unique index
//! enforces. Sending one queues its message and marks it `sent` in the same write, so a crash
//! between the two cannot send the continuation twice.

use kratos_protocol::{
    Continuation, ContinuationId, ContinuationState, MessageId, MessageInput, MessageOrigin,
    ThreadEventBody, ThreadId, Timestamp, TurnId,
};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::json::{required_column, to_column};
use crate::{Enqueued, QueuePlace, Store, StoreError, ThreadWrite};

impl ThreadWrite<'_> {
    /// Records a new continuation. `Err(Sqlite)` with a constraint error when the thread already
    /// has an open one.
    pub fn add_continuation(&mut self, continuation: Continuation) -> Result<(), StoreError> {
        let (state, detail, message_id) = state_columns(&continuation.state);
        self.transaction.execute(
            "INSERT INTO continuations (id, thread_id, turn_id, waits_on, resets_at, state, detail,
               message_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                continuation.id.as_str(),
                self.thread_id().as_str(),
                continuation.turn_id.as_str(),
                to_column(&continuation.waits_on),
                continuation.resets_at.as_ref().map(Timestamp::as_str),
                state,
                detail,
                message_id,
                self.at().as_str(),
            ],
        )?;
        self.append(ThreadEventBody::ContinuationChanged { continuation })?;
        Ok(())
    }

    /// Replaces an open continuation's reset time and state and records the change. Returns
    /// `None`, writing nothing, when the continuation is no longer open: a stale timer or a late
    /// request then changes nothing.
    pub fn update_continuation(
        &mut self,
        id: &ContinuationId,
        resets_at: Option<Timestamp>,
        state: ContinuationState,
    ) -> Result<Option<Continuation>, StoreError> {
        let (name, detail, message_id) = state_columns(&state);
        let updated = self.transaction.execute(
            "UPDATE continuations SET resets_at = ?1, state = ?2, detail = ?3, message_id = ?4,
               updated_at = ?5
             WHERE id = ?6 AND thread_id = ?7 AND state IN ('scheduled', 'awaitingUser')",
            params![
                resets_at.as_ref().map(Timestamp::as_str),
                name,
                detail,
                message_id,
                self.at().as_str(),
                id.as_str(),
                self.thread_id().as_str(),
            ],
        )?;
        if updated == 0 {
            return Ok(None);
        }
        let continuation = continuation(&self.transaction, id)?
            .map(|(_, continuation)| continuation)
            .expect("the continuation was just updated");
        self.append(ThreadEventBody::ContinuationChanged {
            continuation: continuation.clone(),
        })?;
        Ok(Some(continuation))
    }

    /// Queues the continuation's message ahead of every waiting message and marks it sent, in this
    /// one write. Returns `None`, writing nothing, when the continuation is no longer open.
    pub fn send_continuation(
        &mut self,
        id: &ContinuationId,
        message_id: &MessageId,
        input: Vec<MessageInput>,
        max_pending: usize,
    ) -> Result<Option<Enqueued>, StoreError> {
        let Some((_, open)) = continuation(&self.transaction, id)? else {
            return Err(StoreError::UnknownContinuation(id.clone()));
        };
        if !is_open(&open.state) {
            return Ok(None);
        }
        let origin = MessageOrigin::Continuation {
            continuation_id: id.clone(),
        };
        let enqueued =
            self.enqueue_message(message_id, input, origin, QueuePlace::Front, max_pending)?;
        if let Enqueued::Added(message) = &enqueued {
            let state = ContinuationState::Sent {
                message_id: message.id.clone(),
            };
            self.update_continuation(id, open.resets_at, state)?;
        }
        Ok(Some(enqueued))
    }

    /// The thread's open continuation, as this write sees it.
    pub fn open_continuation(&self) -> Result<Option<Continuation>, StoreError> {
        open_continuation(&self.transaction, self.thread_id())
    }
}

impl Store {
    pub fn continuation(
        &self,
        id: &ContinuationId,
    ) -> Result<Option<(ThreadId, Continuation)>, StoreError> {
        continuation(&self.connection, id)
    }

    pub fn open_continuation(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<Continuation>, StoreError> {
        open_continuation(&self.connection, thread_id)
    }

    /// Every open continuation, for re-arming the scheduler's timers after a restart (L6).
    pub fn open_continuations(&self) -> Result<Vec<(ThreadId, Continuation)>, StoreError> {
        let mut statement = self.connection.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM continuations WHERE state IN ('scheduled', 'awaitingUser')
             ORDER BY created_at, rowid"
        ))?;
        let rows = statement.query_map([], continuation_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

const COLUMNS: &str = "thread_id, id, turn_id, waits_on, resets_at, state, detail, message_id";

fn continuation(
    connection: &Connection,
    id: &ContinuationId,
) -> Result<Option<(ThreadId, Continuation)>, StoreError> {
    let row = connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM continuations WHERE id = ?1"),
            [id.as_str()],
            continuation_from_row,
        )
        .optional()?;
    Ok(row)
}

fn open_continuation(
    connection: &Connection,
    thread_id: &ThreadId,
) -> Result<Option<Continuation>, StoreError> {
    let row = connection
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM continuations
                 WHERE thread_id = ?1 AND state IN ('scheduled', 'awaitingUser')"
            ),
            [thread_id.as_str()],
            continuation_from_row,
        )
        .optional()?;
    Ok(row.map(|(_, continuation)| continuation))
}

fn continuation_from_row(row: &Row<'_>) -> rusqlite::Result<(ThreadId, Continuation)> {
    let state: String = row.get(5)?;
    let detail: Option<String> = row.get(6)?;
    let message_id: Option<String> = row.get(7)?;
    let state = state_from_columns(&state, detail, message_id).ok_or_else(|| {
        let reason = format!("continuation state {state} without its details");
        rusqlite::Error::FromSqlConversionFailure(5, Type::Text, reason.into())
    })?;
    Ok((
        ThreadId::new(row.get(0)?),
        Continuation {
            id: ContinuationId::new(row.get(1)?),
            turn_id: TurnId::new(row.get(2)?),
            waits_on: required_column(row, 3)?,
            resets_at: row.get::<_, Option<String>>(4)?.map(Timestamp::from_stored),
            state,
        },
    ))
}

fn is_open(state: &ContinuationState) -> bool {
    matches!(
        state,
        ContinuationState::Scheduled | ContinuationState::AwaitingUser { .. }
    )
}

/// The `state`, `detail`, and `message_id` columns of a state.
fn state_columns(state: &ContinuationState) -> (&'static str, Option<&str>, Option<&str>) {
    match state {
        ContinuationState::Scheduled => ("scheduled", None, None),
        ContinuationState::AwaitingUser { why } => ("awaitingUser", Some(why), None),
        ContinuationState::Sent { message_id } => ("sent", None, Some(message_id.as_str())),
        ContinuationState::Cancelled { reason } => ("cancelled", Some(reason), None),
    }
}

fn state_from_columns(
    state: &str,
    detail: Option<String>,
    message_id: Option<String>,
) -> Option<ContinuationState> {
    Some(match state {
        "scheduled" => ContinuationState::Scheduled,
        "awaitingUser" => ContinuationState::AwaitingUser { why: detail? },
        "sent" => ContinuationState::Sent {
            message_id: MessageId::new(message_id?),
        },
        "cancelled" => ContinuationState::Cancelled { reason: detail? },
        _ => return None,
    })
}
