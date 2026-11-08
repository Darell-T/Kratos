use kratos_protocol::{LiveUpdate, ThreadEvent, ThreadId};
use kratos_store::StoreError;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::{StreamExt, StreamMap};

use crate::hub::EventPages;

/// The threads one socket follows. A follow is its stored pages and its live updates (D88) together,
/// so ending one ends the other.
#[derive(Default)]
pub(super) struct Follows {
    pages: StreamMap<ThreadId, EventPages>,
    live: StreamMap<ThreadId, BroadcastStream<LiveUpdate>>,
}

pub(super) enum FollowUpdate {
    Page(ThreadId, Result<Vec<ThreadEvent>, StoreError>),
    Live(ThreadId, Result<LiveUpdate, BroadcastStreamRecvError>),
}

impl Follows {
    /// Follows `thread_id`, replacing any earlier follow of it.
    pub(super) fn insert(
        &mut self,
        thread_id: ThreadId,
        pages: EventPages,
        updates: broadcast::Receiver<LiveUpdate>,
    ) {
        self.pages.insert(thread_id.clone(), pages);
        self.live.insert(thread_id, BroadcastStream::new(updates));
    }

    /// Replaces the live updates of a thread that is still followed.
    pub(super) fn replace_live(
        &mut self,
        thread_id: ThreadId,
        updates: broadcast::Receiver<LiveUpdate>,
    ) {
        if self.pages.contains_key(&thread_id) {
            self.live.insert(thread_id, BroadcastStream::new(updates));
        }
    }

    pub(super) fn remove(&mut self, thread_id: &ThreadId) {
        self.pages.remove(thread_id);
        self.live.remove(thread_id);
    }

    /// The next page or live update of any followed thread. Waits forever when none is followed.
    pub(super) async fn next(&mut self) -> FollowUpdate {
        tokio::select! {
            Some((thread_id, page)) = self.pages.next() => FollowUpdate::Page(thread_id, page),
            Some((thread_id, update)) = self.live.next() => FollowUpdate::Live(thread_id, update),
            else => std::future::pending().await,
        }
    }
}
