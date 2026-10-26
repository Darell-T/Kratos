//! Thread event logs and the clients that follow them.
//!
//! SQLite is the only buffer. Appending commits a row and then rings the thread's doorbell. A
//! follower keeps a cursor and pulls pages of rows after it; the doorbell only says "this log
//! grew". So:
//!
//! - No event is lost. A follower registers its doorbell before its first read and reads again
//!   after every ring, and a ring always comes after its row's commit.
//! - No event repeats. Every read asks for rows after the cursor, and the cursor only moves
//!   forward.
//! - Nobody waits on a client. Appending never touches a socket, and a doorbell folds any number of
//!   rings into one, so a stalled follower costs one page of memory however far behind it falls.
//!   When its client reads again it catches up from the log, a page at a time, without being
//!   disconnected.

//!
//! There is no per-subscriber queue, so there is nothing to overflow.
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures_util::stream::{self, Stream};
use kratos_protocol::{
    LiveUpdate, ReplySoFar, Seq, ThreadEvent, ThreadEventBody, ThreadId, TurnId,
};
use kratos_store::{StoreError, ThreadWrite};
use tokio::sync::{broadcast, watch};

use crate::store_thread::StoreThread;

/// The most stored bytes one page holds, except that a page always holds at least one event.
pub const PAGE_BYTES: usize = 64 * 1024;
/// Live updates a client may fall behind by before it gets a snapshot in their place.
const LIVE_BACKLOG: usize = 256;

/// One thread's events, as non-empty pages in order with no gaps between them. Never ends; drop it
/// to stop following. An `Err` page means the log could not be read past the cursor.
pub type EventPages = Pin<Box<dyn Stream<Item = Result<Vec<ThreadEvent>, StoreError>> + Send>>;

/// Every appender and every follower goes through the hub, so no event can be committed without
/// waking the clients that follow its thread.
#[derive(Clone)]
pub struct Hub {
    store: StoreThread,
    doorbells: Doorbells,
    live: LiveThreads,
}

/// What a write returned, and the events it appended.
#[derive(Debug)]
pub struct Written<T> {
    pub value: T,
    pub events: Vec<ThreadEvent>,
}

impl Hub {
    pub fn new(store: StoreThread) -> Hub {
        Hub {
            store,
            doorbells: Doorbells::default(),
            live: LiveThreads::default(),
        }
    }

    /// Runs `job` in one transaction over the thread's records and wakes the thread's followers
    /// once it commits. Returns once it is committed; never waits for a follower or a socket. When
    /// `job` fails, nothing it wrote is kept. `Err(UnknownThread)` writes nothing.
    #[allow(
        clippy::disallowed_methods,
        reason = "the engine's one call to Store::write_thread; clippy.toml bans every other"
    )]
    pub async fn write<T, F>(&self, thread_id: ThreadId, job: F) -> Result<Written<T>, StoreError>
    where
        F: FnOnce(&mut ThreadWrite<'_>) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        // The commit and the ring run in one job, so a request dropped after the commit cannot skip
        // the ring and leave followers asleep.
        let doorbells = self.doorbells.clone();
        let live = self.live.clone();
        self.store
            .call(move |store| {
                let mut write = store.write_thread(&thread_id)?;
                let value = job(&mut write)?;
                let events = write.commit()?;
                for event in &events {
                    if let ThreadEventBody::TurnCompleted { turn_id, .. } = &event.event {
                        live.end_turn(&thread_id, turn_id);
                    }
                }
                if !events.is_empty() {
                    doorbells.ring(&thread_id);
                }
                Ok(Written { value, events })
            })
            .await
    }

    /// Appends an event to the thread's log and wakes its followers. See [`Hub::write`].
    pub async fn append(
        &self,
        thread_id: ThreadId,
        body: ThreadEventBody,
    ) -> Result<ThreadEvent, StoreError> {
        let written = self
            .write(thread_id, move |write| {
                write.append(body)?;
                Ok(())
            })
            .await?;
        Ok(written
            .events
            .into_iter()
            .next()
            .expect("the write appended one event"))
    }

    /// Sends `update` to the clients following the thread, without storing it. A reply delta also
    /// extends the turn's reply so far, which a client that subscribes later gets as a snapshot.
    pub fn publish(&self, thread_id: &ThreadId, update: LiveUpdate) {
        self.live.publish(thread_id, update);
    }

    /// The thread's replies so far and a receiver for every later update, taken together so no
    /// update falls between them. `None` in place of the snapshot when no turn is running.
    pub fn watch_live(
        &self,
        thread_id: &ThreadId,
    ) -> (Option<LiveUpdate>, broadcast::Receiver<LiveUpdate>) {
        self.live.watch(thread_id)
    }

    /// Follows the thread's log from the cursor `after`: the events already stored, then each new
    /// one. `Err` when the thread does not exist or `after` is ahead of its log.
    pub async fn follow(&self, thread_id: ThreadId, after: Seq) -> Result<EventPages, FollowError> {
        let lookup = thread_id.clone();
        let last = self
            .store
            .call(move |store| store.last_seq(&lookup))
            .await?;
        if after > last {
            return Err(FollowError::CursorAhead {
                thread_id,
                after,
                last,
            });
        }
        // Registering before the first read is what makes the follow gap-free.
        let doorbell = self.doorbells.listen(&thread_id);
        let follower = Follower {
            hub: self.clone(),
            thread_id,
            cursor: after,
            doorbell,
        };
        Ok(Box::pin(stream::unfold(
            follower,
            |mut follower| async move {
                let page = follower.next_page().await;
                Some((page, follower))
            },
        )))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FollowError {
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A client that kept its cursor across a restore of an older backup would otherwise wait for
    /// events it already has, and miss the ones that reuse their numbers.
    #[error(
        "Cursor {} is ahead of thread {thread_id}, which ends at {}",
        .after.get(), .last.get()
    )]
    CursorAhead {
        thread_id: ThreadId,
        after: Seq,
        last: Seq,
    },
}

/// Per thread: the running turn's replies so far, and the channel that carries live updates to the
/// thread's clients. A slow client loses old updates rather than holding them in memory, and gets
/// a snapshot instead.
#[derive(Clone, Default)]
struct LiveThreads(Arc<Mutex<HashMap<ThreadId, LiveThread>>>);

struct LiveThread {
    sender: broadcast::Sender<LiveUpdate>,
    turn_id: Option<TurnId>,
    replies: Vec<ReplySoFar>,
}

impl LiveThreads {
    fn publish(&self, thread_id: &ThreadId, update: LiveUpdate) {
        let mut threads = self.threads();
        let thread = threads
            .entry(thread_id.clone())
            .or_insert_with(LiveThread::new);
        if let LiveUpdate::ReplyDelta {
            turn_id,
            item_id,
            delta,
        } = &update
        {
            thread.extend_reply(turn_id, item_id, delta);
        }
        // No receivers is not an error: nobody follows the thread right now.
        let _ = thread.sender.send(update);
    }

    fn end_turn(&self, thread_id: &ThreadId, turn_id: &TurnId) {
        let mut threads = self.threads();
        let unwatched = threads.get_mut(thread_id).is_some_and(|thread| {
            if thread.turn_id.as_ref() != Some(turn_id) {
                return false;
            }
            thread.turn_id = None;
            thread.replies.clear();
            thread.sender.receiver_count() == 0
        });
        if unwatched {
            threads.remove(thread_id);
        }
    }

    fn watch(&self, thread_id: &ThreadId) -> (Option<LiveUpdate>, broadcast::Receiver<LiveUpdate>) {
        let mut threads = self.threads();
        let thread = threads
            .entry(thread_id.clone())
            .or_insert_with(LiveThread::new);
        let snapshot = thread.turn_id.clone().map(|turn_id| LiveUpdate::Snapshot {
            turn_id: Some(turn_id),
            replies: thread.replies.clone(),
        });
        (snapshot, thread.sender.subscribe())
    }

    fn threads(&self) -> MutexGuard<'_, HashMap<ThreadId, LiveThread>> {
        // No code panics while holding this lock, so a poisoned lock still guards a consistent map.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl LiveThread {
    fn new() -> LiveThread {
        LiveThread {
            sender: broadcast::channel(LIVE_BACKLOG).0,
            turn_id: None,
            replies: Vec::new(),
        }
    }

    fn extend_reply(&mut self, turn_id: &TurnId, item_id: &str, delta: &str) {
        if self.turn_id.as_ref() != Some(turn_id) {
            self.turn_id = Some(turn_id.clone());
            self.replies.clear();
        }
        match self
            .replies
            .iter_mut()
            .find(|reply| reply.item_id == item_id)
        {
            Some(reply) => reply.text.push_str(delta),
            None => self.replies.push(ReplySoFar {
                item_id: item_id.to_owned(),
                text: delta.to_owned(),
            }),
        }
    }
}

#[derive(Clone, Default)]
struct Doorbells(Arc<Mutex<HashMap<ThreadId, watch::Sender<()>>>>);

impl Doorbells {
    fn ring(&self, thread_id: &ThreadId) {
        if let Some(doorbell) = self.bells().get(thread_id) {
            doorbell.send_replace(());
        }
    }

    fn listen(&self, thread_id: &ThreadId) -> watch::Receiver<()> {
        self.bells()
            .entry(thread_id.clone())
            .or_insert_with(|| watch::channel(()).0)
            .subscribe()
    }

    fn release(&self, thread_id: &ThreadId) {
        let mut bells = self.bells();
        // The count still includes the caller's receiver, which drops after this call.
        if bells
            .get(thread_id)
            .is_some_and(|doorbell| doorbell.receiver_count() == 1)
        {
            bells.remove(thread_id);
        }
    }

    fn bells(&self) -> MutexGuard<'_, HashMap<ThreadId, watch::Sender<()>>> {
        // No code panics while holding this lock and a map insert cannot leave it half done, so a
        // poisoned lock still guards a consistent map.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Follower {
    hub: Hub,
    thread_id: ThreadId,
    cursor: Seq,
    doorbell: watch::Receiver<()>,
}

impl Follower {
    /// Marking the doorbell seen before each read is the ordering argument. An event committed
    /// before the mark is in the read. One committed after it rings after the mark, so `changed`
    /// returns at once and the loop reads again.
    async fn next_page(&mut self) -> Result<Vec<ThreadEvent>, StoreError> {
        loop {
            self.doorbell.mark_unchanged();
            let (thread_id, after) = (self.thread_id.clone(), self.cursor);
            let page = self
                .hub
                .store
                .call(move |store| store.events_after(&thread_id, after, PAGE_BYTES))
                .await?;
            if let Some(last) = page.last() {
                self.cursor = last.seq;
                return Ok(page);
            }
            self.doorbell
                .changed()
                .await
                .expect("the hub keeps a doorbell while it has followers");
        }
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.hub.doorbells.release(&self.thread_id);
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Future, poll_fn};
    use std::pin::Pin;
    use std::task::Poll;
    use std::time::Duration;

    use futures_util::StreamExt;
    use kratos_protocol::{CreateThread, CreateWorkspace};
    use kratos_store::Store;
    use serde_json::json;

    use super::*;
    use crate::store_thread;

    fn hub_with_a_thread(home: &std::path::Path) -> (Hub, ThreadId) {
        let store = Store::open(home).unwrap();
        let workspace: CreateWorkspace =
            serde_json::from_value(json!({ "name": "repo", "path": home })).unwrap();
        let workspace = store.create_workspace(&workspace).unwrap();
        let thread: CreateThread = serde_json::from_value(
            json!({ "workspaceId": workspace.id, "title": "chat", "provider": "codex" }),
        )
        .unwrap();
        let thread = store.create_thread(&thread).unwrap();
        let (store_thread, store_jobs) = store_thread::channel();
        std::thread::spawn(move || store_jobs.run(store));
        (Hub::new(store_thread), thread.id)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_committed_completion_clears_its_live_reply_even_if_the_writer_is_cancelled() {
        let home = tempfile::tempdir().unwrap();
        let (hub, thread_id) = hub_with_a_thread(home.path());
        hub.publish(
            &thread_id,
            LiveUpdate::ReplyDelta {
                turn_id: TurnId::new("finished".into()),
                item_id: "reply".into(),
                delta: "hello".into(),
            },
        );
        let mut append = Box::pin(hub.append(
            thread_id.clone(),
            ThreadEventBody::TurnCompleted {
                turn_id: TurnId::new("finished".into()),
                outcome: kratos_protocol::TurnOutcome::Completed,
                changes: kratos_protocol::TurnChanges {
                    files: vec![],
                    edited_since_command: false,
                    ran_command: false,
                },
                context: None,
            },
        ));
        assert!(poll_once(&mut append).await.is_pending());
        drop(append);

        let live = hub.clone();
        let snapshot = hub.store.call(move |_| live.watch_live(&thread_id).0).await;
        assert!(
            snapshot.is_none(),
            "completion must be visible after live state is cleared"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_append_that_lands_after_a_follower_reads_but_before_it_waits_is_not_lost() {
        // Catches a follower that marks its doorbell seen after its read instead of before. The
        // ring of an append that commits in that gap would be erased, and the follower would sleep
        // on an event it has not read. Real timing almost never opens that gap, so the test polls
        // the follower by hand to open it.
        let home = tempfile::tempdir().unwrap();
        let (hub, thread_id) = hub_with_a_thread(home.path());
        let mut pages = hub
            .follow(thread_id.clone(), Seq::BEFORE_FIRST)
            .await
            .unwrap();
        let mut next = pages.next();
        assert!(poll_once(&mut next).await.is_pending());

        let body = ThreadEventBody::UserMessage { text: "one".into() };
        hub.append(thread_id, body).await.unwrap();

        let page = tokio::time::timeout(Duration::from_secs(10), next)
            .await
            .expect("the follower slept on an event it had not read")
            .expect("a follow never ends")
            .unwrap();
        assert_eq!(
            page.iter().map(|event| event.seq.get()).collect::<Vec<_>>(),
            [1]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_append_whose_request_is_dropped_after_it_queues_still_wakes_the_followers() {
        // Catches a ring made by the request instead of by the job that commits. A request dropped
        // after the commit would skip the ring, and followers would sleep on a committed event.
        let home = tempfile::tempdir().unwrap();
        let (hub, thread_id) = hub_with_a_thread(home.path());
        let mut pages = hub
            .follow(thread_id.clone(), Seq::BEFORE_FIRST)
            .await
            .unwrap();
        let mut next = pages.next();
        assert!(poll_once(&mut next).await.is_pending());

        let body = ThreadEventBody::UserMessage { text: "one".into() };
        let mut append = Box::pin(hub.append(thread_id, body));
        assert!(poll_once(&mut append).await.is_pending());
        drop(append);

        let page = tokio::time::timeout(Duration::from_secs(10), next)
            .await
            .expect("the follower slept on an event that was committed")
            .expect("a follow never ends")
            .unwrap();
        assert_eq!(
            page.iter().map(|event| event.seq.get()).collect::<Vec<_>>(),
            [1]
        );
    }

    async fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
        poll_fn(|cx| Poll::Ready(Pin::new(&mut *future).poll(cx))).await
    }
}
