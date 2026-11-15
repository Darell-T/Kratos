//! What Codex sends: notifications become thread events and live updates, and requests become
//! approvals and questions, or answers Kratos gives itself.

use kratos_codex_client::{Incoming, Notification, RequestId, types};
use kratos_protocol::{AgentItem, LiveUpdate, Provider, ThreadEventBody, ThreadId, TurnId};

use super::actor::Actor;
use super::servers::ServerKey;
use super::threads::{Link, Turn};
use super::wire;

impl Actor {
    pub(super) async fn on_incoming(&mut self, key: ServerKey, incoming: Incoming) {
        match incoming {
            Incoming::Notification(notification) => {
                self.on_notification(&key, notification).await;
            }
            Incoming::Request(request) => self.on_request(&key, request).await,
            Incoming::Exited(exit) => self.on_exit(key, exit).await,
        }
    }

    // #lizard forgives: one arm per notification Codex sends, each a call or two.
    async fn on_notification(&mut self, key: &ServerKey, notification: Notification) {
        match notification {
            Notification::AgentMessageDelta(delta) => {
                if let Some(thread_id) = self.thread_of(key, &delta.thread_id) {
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
            Notification::ItemStarted(started) => self.on_item_started(key, started),
            Notification::ItemCompleted(completed) => self.on_item_completed(key, completed).await,
            Notification::TurnCompleted(completed) => {
                if let Some(thread_id) = self.thread_of(key, &completed.thread_id) {
                    self.finish_turn(&thread_id, completed.turn).await;
                    self.make_room_if_waited_for();
                }
            }
            Notification::Error(error) => self.on_error(key, error).await,
            Notification::ServerRequestResolved(resolved) => {
                self.withdraw(key, &RequestId::from(&resolved.request_id))
                    .await;
            }
            Notification::ThreadClosed(closed) => {
                if let Some(thread_id) = self.thread_of(key, &closed.thread_id) {
                    self.on_thread_closed(&thread_id).await;
                }
            }
            Notification::ThreadTokenUsageUpdated(usage) => self.on_token_usage(key, usage),
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

    /// Codex closed the thread. Work that runs in it can never finish, so it ends as it would if
    /// Codex had exited. An idle thread only detaches and is loaded again for its next turn.
    async fn on_thread_closed(&mut self, thread_id: &ThreadId) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        if matches!(thread.turn, Turn::Idle) && thread.steering.is_empty() {
            thread.link = Link::Detached;
            return;
        }
        self.detach(thread_id, "Codex closed the thread while it worked")
            .await;
        self.dispatch(thread_id).await;
    }

    /// The Kratos thread `native_id` names among those `key`'s server runs. A thread's server runs
    /// only that thread, so another server's thread is never named, even when two servers give
    /// their threads the same id. The account's server runs none.
    pub(super) fn thread_of(&self, key: &ServerKey, native_id: &str) -> Option<ThreadId> {
        let ServerKey::Thread(thread_id) = key else {
            return None;
        };
        let thread = self.threads.get(thread_id)?;
        (thread.native_id() == Some(native_id)).then(|| thread_id.clone())
    }

    fn on_item_started(&mut self, key: &ServerKey, started: types::ItemStartedNotification) {
        let Some(thread_id) = self.thread_of(key, &started.thread_id) else {
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

    async fn on_item_completed(
        &mut self,
        key: &ServerKey,
        completed: types::ItemCompletedNotification,
    ) {
        let Some(thread_id) = self.thread_of(key, &completed.thread_id) else {
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

    async fn on_error(&mut self, key: &ServerKey, error: types::ErrorNotification) {
        let Some(thread_id) = self.thread_of(key, &error.thread_id) else {
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

    fn on_token_usage(
        &mut self,
        key: &ServerKey,
        usage: types::ThreadTokenUsageUpdatedNotification,
    ) {
        let Some(thread_id) = self.thread_of(key, &usage.thread_id) else {
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
