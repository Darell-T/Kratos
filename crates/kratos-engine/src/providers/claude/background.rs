use std::num::NonZeroU64;

use kratos_claude_client::Background;
use kratos_protocol::{LiveUpdate, ThreadId};

use super::actor::{Actor, Done};

pub(super) const WAITING: &str = "Claude is running a background command, and this message needs \
     Claude started again with the thread's new settings, which would end the command. The \
     message starts when the command ends.";

impl Actor {
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
