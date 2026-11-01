use kratos_protocol::{Seq, ThreadEvent, ThreadEventBody, ThreadId, Timestamp};
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};

use crate::{Store, StoreError};

impl Store {
    /// Appends `body` as the thread's next event and sets the thread's `updated_at` to the event's
    /// `at`, in one `BEGIN IMMEDIATE` transaction.
    ///
    /// The commit is synced to disk before this returns, so the returned `seq` is never handed to
    /// another event. `Err(UnknownThread)` writes nothing.
    pub fn append_event(
        &mut self,
        thread_id: &ThreadId,
        body: &ThreadEventBody,
    ) -> Result<ThreadEvent, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Read with the write lock held, so `at` follows `seq`.
        let at = Timestamp::now();
        let touched = transaction.execute(
            "UPDATE threads SET updated_at = ?1 WHERE id = ?2",
            params![at.as_str(), thread_id.as_str()],
        )?;
        if touched == 0 {
            return Err(StoreError::UnknownThread(thread_id.clone()));
        }
        let seq: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM thread_events WHERE thread_id = ?1",
            [thread_id.as_str()],
            |row| row.get(0),
        )?;
        let stored = serde_json::to_string(body).expect("a thread event body serializes");
        transaction.execute(
            "INSERT INTO thread_events (thread_id, seq, at, event) VALUES (?1, ?2, ?3, ?4)",
            params![thread_id.as_str(), seq, at.as_str(), stored],
        )?;
        transaction.commit()?;
        Ok(ThreadEvent {
            thread_id: thread_id.clone(),
            seq: Seq::new(seq),
            at,
            event: body.clone(),
        })
    }

    /// The seq of the thread's latest event, or [`Seq::BEFORE_FIRST`] when it has none.
    /// `Err(UnknownThread)` when no thread has this id.
    pub fn last_seq(&self, thread_id: &ThreadId) -> Result<Seq, StoreError> {
        let last: Option<u64> = self
            .connection
            .query_row(
                "SELECT COALESCE((SELECT MAX(seq) FROM thread_events WHERE thread_id = threads.id), 0)
                 FROM threads WHERE id = ?1",
                [thread_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        last.map(Seq::new)
            .ok_or_else(|| StoreError::UnknownThread(thread_id.clone()))
    }

    /// One page of the thread's events after `after`, in order.
    ///
    /// Events join the page until the next one would take the page's stored size past `max_bytes`.
    /// The first event always joins, however large, so a caller that loops on pages always moves
    /// forward. `max_bytes = usize::MAX` reads the rest of the log. An unknown thread has no
    /// events.
    ///
    /// An event that does not parse is `Err(UnreadableEvent)`, never skipped: a follower must not
    /// miss one.
    pub fn events_after(
        &self,
        thread_id: &ThreadId,
        after: Seq,
        max_bytes: usize,
    ) -> Result<Vec<ThreadEvent>, StoreError> {
        // A seq above SQLite's INTEGER range has no events, and clamping it says so.
        let after = i64::try_from(after.get()).unwrap_or(i64::MAX);
        let mut statement = self.connection.prepare_cached(
            "SELECT seq, at, event FROM thread_events WHERE thread_id = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let mut rows = statement.query(params![thread_id.as_str(), after])?;
        let mut events = Vec::new();
        let mut page_bytes = 0usize;
        while let Some(row) = rows.next()? {
            let stored = StoredEvent::from_row(row)?;
            if !events.is_empty() && page_bytes.saturating_add(stored.json.len()) > max_bytes {
                break;
            }
            page_bytes += stored.json.len();
            events.push(stored.parse(thread_id)?);
        }
        Ok(events)
    }
}

struct StoredEvent {
    seq: u64,
    at: String,
    json: String,
}

impl StoredEvent {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<StoredEvent> {
        Ok(StoredEvent {
            seq: row.get(0)?,
            at: row.get(1)?,
            json: row.get(2)?,
        })
    }

    fn parse(self, thread_id: &ThreadId) -> Result<ThreadEvent, StoreError> {
        let seq = Seq::new(self.seq);
        let event =
            serde_json::from_str(&self.json).map_err(|source| StoreError::UnreadableEvent {
                thread_id: thread_id.clone(),
                seq,
                source,
            })?;
        Ok(ThreadEvent {
            thread_id: thread_id.clone(),
            seq,
            at: Timestamp::from_stored(self.at),
            event,
        })
    }
}
