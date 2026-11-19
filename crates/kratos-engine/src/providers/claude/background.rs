use kratos_claude_client::Background;
use kratos_protocol::ThreadId;

use super::actor::{Actor, Done};
use super::threads::ReportId;

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
        if let Some(report_id) = open.note(background) {
            let thread_id = thread_id.clone();
            self.spawn(async move {
                tokio::time::sleep(wait).await;
                Done::ReportOverdue {
                    thread_id,
                    epoch,
                    report_id,
                }
            });
        }
        self.dispatch(thread_id).await;
    }

    pub(super) async fn on_report_overdue(
        &mut self,
        thread_id: &ThreadId,
        epoch: u64,
        report_id: ReportId,
    ) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        if thread.epoch != epoch {
            return;
        }
        if let Some(open) = thread.open_mut()
            && open.awaited_report == Some(report_id)
        {
            open.awaited_report = None;
            open.finished_tasks.clear();
        }
        self.dispatch(thread_id).await;
    }
}
