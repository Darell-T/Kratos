//! Commands Claude runs in the background, which end with the CLI that runs them. A settings change
//! that needs the CLI started again waits for them, and for the turn the CLI then starts to report
//! them, before the thread's next message closes it.

use std::num::NonZeroU64;

use kratos_claude_client::Background;
use kratos_protocol::{LiveUpdate, ThreadId};

use super::actor::{Actor, Done};

/// What a thread whose message waits for a background command is told.
pub(super) const WAITING: &str = "Claude is running a background command, and this message needs \
     Claude started again with the thread's new settings, which would end the command. The \
     message starts when the command ends.";

impl Actor {
    /// Keeps what the CLI reports of its background tasks. Once the last command ended and the
    /// CLI owes the turn that reports it, the wait for that turn starts.
    pub(super) async fn on_background(&mut self, thread_id: &ThreadId, background: Background) {
        let wait = self.settings.report_turn_wait;
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        let epoch = thread.epoch;
        let Some(open) = thread.open_mut() else {
            return;
        };
        open.note(background);
        if let (0, Some(ended)) = (open.background_tasks, open.report_owed) {
            let thread_id = thread_id.clone();
            self.spawn(async move {
                tokio::time::sleep(wait).await;
                Done::ReportOverdue {
                    thread_id,
                    epoch,
                    ended,
                }
            });
        }
        self.dispatch(thread_id).await;
    }

    /// The CLI started no turn to report the commands that ended, so a message waiting on them
    /// goes ahead.
    pub(super) async fn on_report_overdue(
        &mut self,
        thread_id: &ThreadId,
        epoch: u64,
        ended: NonZeroU64,
    ) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        if thread.epoch != epoch {
            return;
        }
        if let Some(open) = thread.open_mut()
            && open.report_owed == Some(ended)
        {
            open.report_owed = None;
        }
        self.dispatch(thread_id).await;
    }

    pub(super) fn tell_waiting_for_background(&self, thread_id: &ThreadId) {
        self.hub.publish(
            thread_id,
            LiveUpdate::Waiting {
                reason: WAITING.to_owned(),
            },
        );
    }
}
