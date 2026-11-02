use kratos_protocol::{Seq, ThreadEvent, ThreadEventBody, ThreadId, Timestamp};
use rusqlite::{Transaction, TransactionBehavior, params};

use crate::columns::to_column;
use crate::events::last_seq_of;
use crate::{Store, StoreError};

/// One `BEGIN IMMEDIATE` transaction over one thread's records: its events, queue, approvals,
/// questions, continuations, and the thread's own state. Nothing it writes is visible until
/// [`ThreadWrite::commit`], and dropping it rolls everything back, so a step that fails halfway
/// leaves no event without the change it reports, and no change without its event.
///
/// Every event of one write shares one `at`, read with the write lock held, so `at` never goes
/// backwards along a thread's log.
pub struct ThreadWrite<'a> {
    pub(crate) transaction: Transaction<'a>,
    thread_id: ThreadId,
    at: Timestamp,
    last_seq: u64,
    appended: Vec<ThreadEvent>,
}

impl Store {
    /// Begins a write to `thread_id`'s records. `Err(UnknownThread)` when no thread has this id.
    ///
    /// The engine calls this only through its hub, which wakes the thread's followers once the
    /// write commits.
    pub fn write_thread(&mut self, thread_id: &ThreadId) -> Result<ThreadWrite<'_>, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let at = Timestamp::now();
        let Some(last_seq) = last_seq_of(&transaction, thread_id)? else {
            return Err(StoreError::UnknownThread(thread_id.clone()));
        };
        Ok(ThreadWrite {
            transaction,
            thread_id: thread_id.clone(),
            at,
            last_seq,
            appended: Vec::new(),
        })
    }
}

impl ThreadWrite<'_> {
    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    /// The time every row and event of this write records.
    pub(crate) fn at(&self) -> &Timestamp {
        &self.at
    }

    pub fn append(&mut self, body: ThreadEventBody) -> Result<&ThreadEvent, StoreError> {
        let seq = self.last_seq + 1;
        self.transaction.execute(
            "INSERT INTO thread_events (thread_id, seq, at, event) VALUES (?1, ?2, ?3, ?4)",
            params![
                self.thread_id.as_str(),
                seq,
                self.at.as_str(),
                to_column(&body)
            ],
        )?;
        self.last_seq = seq;
        self.appended.push(ThreadEvent {
            thread_id: self.thread_id.clone(),
            seq: Seq::new(seq),
            at: self.at.clone(),
            event: body,
        });
        Ok(self.appended.last().expect("an event was just appended"))
    }

    /// Commits, synced to disk before this returns, and hands back the events the write appended.
    /// When it appended any, the thread's `updated_at` becomes their `at`.
    pub fn commit(self) -> Result<Vec<ThreadEvent>, StoreError> {
        if !self.appended.is_empty() {
            self.transaction.execute(
                "UPDATE threads SET updated_at = ?1 WHERE id = ?2",
                params![self.at.as_str(), self.thread_id.as_str()],
            )?;
        }
        self.transaction.commit()?;
        Ok(self.appended)
    }
}
