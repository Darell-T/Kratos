//! What Codex sends: notifications become thread events and live updates, and requests become
//! approvals and questions, or answers Kratos gives itself.

use kratos_codex_client::{Incoming, Notification, RequestId, types};
use kratos_protocol::{AgentItem, LiveUpdate, Provider, ThreadEventBody, ThreadId, TurnId};

use super::actor::Actor;
use super::wire;

impl Actor {
    pub(super) async fn on_incoming(&mut self, incoming: Incoming) {
        match incoming {
            Incoming::Notification(notification) => self.on_notification(notification).await,
            Incoming::Request(request) => self.on_request(request).await,
            Incoming::Exited(exit) => self.on_exit(exit).await,
        }
    }

    // #lizard forgives: one arm per notification Codex sends, each a call or two.
    async fn on_notification(&mut self, notification: Notification) {
        match notification {
            Notification::AgentMessageDelta(delta) => {
                if let Some(thread_id) = self.thread_of(&delta.thread_id) {
                    self.hub.publish(
                        &thread_id,
                        LiveUpdate::ReplyDelta {
                            turn_id: TurnId::new(delta.turn_id),
                            item_id: delta.item_id,
                            delta: delta.delta,
                        },
                    );
                }
            }
            Notification::ItemStarted(started) => self.on_item_started(started),
            Notification::ItemCompleted(completed) => self.on_item_completed(completed).await,
            Notification::TurnCompleted(completed) => {
                if let Some(thread_id) = self.thread_of(&completed.thread_id) {
                    self.finish_turn(&thread_id, completed.turn).await;
                }
            }
            Notification::Error(error) => self.on_error(error).await,
            Notification::ServerRequestResolved(resolved) => {
                self.withdraw(&RequestId::from(&resolved.request_id)).await;
            }
            Notification::ThreadClosed(closed) => {
                if let Some(thread_id) = self.thread_of(&closed.thread_id)
                    && let Some(thread) = self.threads.get_mut(&thread_id)
                {
                    thread.link = super::threads::Link::Detached;
                }
            }
            Notification::ThreadTokenUsageUpdated(usage) => self.on_token_usage(usage),
            Notification::AccountUpdated(_) => self.usage.account_changed(Provider::Codex),
            Notification::AccountRateLimitsUpdated(updated) => {
                self.usage
                    .merge_bucket(Provider::Codex, wire::usage_bucket(&updated.rate_limits));
            }
            Notification::Malformed { method, error } => {
                eprintln!("codex: cannot read the parameters of {method}: {error}");
            }
            Notification::TurnStarted(_)
            | Notification::WindowsSandboxSetupCompleted(_)
            | Notification::Other { .. } => {}
        }
    }

    /// The Kratos thread a Codex thread id belongs to, among the threads loaded in this server.
    pub(super) fn thread_of(&self, native_id: &str) -> Option<ThreadId> {
        self.threads
            .iter()
            .find(|(_, thread)| thread.native_id() == Some(native_id))
            .map(|(thread_id, _)| thread_id.clone())
    }

    fn on_item_started(&mut self, started: types::ItemStartedNotification) {
        let Some(thread_id) = self.thread_of(&started.thread_id) else {
            return;
        };
        if let types::ThreadItem::FileChange { id, changes, .. } = &started.item {
            // The patch's approval arrives after this, and is decided by these paths.
            let thread = self.threads.get_mut(&thread_id).expect("found above");
            thread
                .file_items
                .insert(id.clone(), wire::file_changes(changes));
        }
        if let Some(item) = wire::agent_item(&started.item) {
            self.hub.publish(
                &thread_id,
                LiveUpdate::ItemStarted {
                    turn_id: TurnId::new(started.turn_id),
                    item,
                },
            );
        }
    }

    async fn on_item_completed(&mut self, completed: types::ItemCompletedNotification) {
        let Some(thread_id) = self.thread_of(&completed.thread_id) else {
            return;
        };
        let Some(item) = wire::agent_item(&completed.item) else {
            return;
        };
        let thread = self.threads.get_mut(&thread_id).expect("found above");
        if let Some(work) = thread.turn.work_mut() {
            work.changes.record(&item);
            if let AgentItem::Message { text, .. } = &item {
                work.last_reply = Some(text.clone());
            }
        }
        let turn_id = TurnId::new(completed.turn_id);
        let _ = self
            .write(&thread_id, move |write| {
                write.append(ThreadEventBody::ItemCompleted { turn_id, item })?;
                Ok(())
            })
            .await;
    }

    async fn on_error(&mut self, error: types::ErrorNotification) {
        let Some(thread_id) = self.thread_of(&error.thread_id) else {
            return;
        };
        let body = ThreadEventBody::ProviderError {
            turn_id: TurnId::new(error.turn_id),
            message: error.error.message,
            will_retry: error.will_retry,
        };
        let _ = self
            .write(&thread_id, move |write| {
                write.append(body)?;
                Ok(())
            })
            .await;
    }

    fn on_token_usage(&mut self, usage: types::ThreadTokenUsageUpdatedNotification) {
        let Some(thread_id) = self.thread_of(&usage.thread_id) else {
            return;
        };
        let context = wire::context_usage(&usage.token_usage);
        self.learn_window(&thread_id, context.context_window);
        let thread = self.threads.get_mut(&thread_id).expect("found above");
        if let Some(work) = thread.turn.work_mut() {
            work.context = Some(context.clone());
        }
        self.hub.publish(
            &thread_id,
            LiveUpdate::Context {
                turn_id: TurnId::new(usage.turn_id),
                usage: context,
            },
        );
    }
}
