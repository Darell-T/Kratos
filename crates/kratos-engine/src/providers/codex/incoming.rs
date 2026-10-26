//! What Codex sends: notifications become thread events and live updates, and requests become
//! approvals and questions, or answers Kratos gives itself.

use kratos_codex_client::{
    Incoming, IncomingRequest, Notification, RequestId, ServerRequest, types,
};
use kratos_protocol::{
    Approval, ApprovalDecision, ApprovalId, ApprovalRequest, AskReason, LiveUpdate, Provider,
    QuestionId, ThreadEventBody, ThreadId, TurnId,
};
use serde_json::json;
use uuid::Uuid;

use super::actor::{Actor, settle_closed};
use super::policy::{self, CommandRequest, Verdict};
use super::requests::{Answer, Waiting};
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
    fn thread_of(&self, native_id: &str) -> Option<ThreadId> {
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
        let thread = self.threads.get_mut(&thread_id).expect("found above");
        thread.changes.record(&completed.item);
        let Some(item) = wire::agent_item(&completed.item) else {
            return;
        };
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
        let thread = self.threads.get_mut(&thread_id).expect("found above");
        let loaded_without_limit = matches!(
            thread.link,
            super::threads::Link::Attached {
                context_limit: None,
                ..
            }
        );
        if loaded_without_limit
            && let (Some(window), Some(model)) = (context.context_window, thread.model())
        {
            // The model's own window, seen with no setting of Kratos's in force (D76).
            self.windows.insert(model.to_owned(), window);
        }
        let thread = self.threads.get_mut(&thread_id).expect("found above");
        thread.context = Some(context.clone());
        self.hub.publish(
            &thread_id,
            LiveUpdate::Context {
                turn_id: TurnId::new(usage.turn_id),
                usage: context,
            },
        );
    }

    /// Codex no longer needs an answer: the request closes, and an answer sent later is dropped.
    async fn withdraw(&mut self, codex_id: &RequestId) {
        if let Some((thread_id, closed)) = self.requests.withdraw(codex_id) {
            let _ = self
                .write(&thread_id, move |write| {
                    settle_closed(write, &closed, "Codex no longer needs an answer")
                })
                .await;
        }
    }

    async fn on_request(&mut self, request: IncomingRequest) {
        let native_id = request_thread(&request.request);
        let Some(thread_id) = self.thread_of(&native_id) else {
            // A thread Kratos does not run cannot be granted anything.
            cancel(request);
            return;
        };
        match &request.request {
            ServerRequest::CommandApproval(params) => {
                let verdict = {
                    let thread = &self.threads[&thread_id];
                    let ask = CommandRequest {
                        command: params.command.as_deref(),
                        reason: params.reason.as_deref(),
                        wants_more_permissions: params.additional_permissions.is_some(),
                        wants_network: params.network_approval_context.is_some(),
                    };
                    policy::decide_command(&thread.turn_settings, &ask, self.settings.platform)
                };
                let shown = ApprovalRequest::Command {
                    command: params.command.clone().unwrap_or_default(),
                    cwd: params.cwd.as_ref().map(|cwd| cwd.0.clone()),
                    reason: params.reason.clone(),
                };
                let turn_id = TurnId::new(params.turn_id.clone());
                self.settle_or_ask(
                    thread_id,
                    turn_id,
                    request,
                    verdict,
                    shown,
                    Answer::Decision,
                )
                .await;
            }
            ServerRequest::FileChangeApproval(params) => {
                let thread = &self.threads[&thread_id];
                let changes = thread.file_items.get(&params.item_id).cloned();
                let verdict = policy::decide_patch(
                    &thread.turn_settings,
                    changes.as_deref(),
                    params.grant_root.is_some(),
                    std::path::Path::new(&thread.folder),
                    &self.settings.protected,
                );
                let shown = ApprovalRequest::FileChanges {
                    changes: changes.unwrap_or_default(),
                    reason: params.reason.clone(),
                };
                let turn_id = TurnId::new(params.turn_id.clone());
                self.settle_or_ask(
                    thread_id,
                    turn_id,
                    request,
                    verdict,
                    shown,
                    Answer::Decision,
                )
                .await;
            }
            ServerRequest::Permissions(params) => {
                let shown = ApprovalRequest::Permissions {
                    reason: params.reason.clone(),
                };
                let (turn_id, requested) = (
                    TurnId::new(params.turn_id.clone()),
                    params.permissions.clone(),
                );
                let verdict = Verdict::Ask(AskReason::Escalation);
                self.settle_or_ask(
                    thread_id,
                    turn_id,
                    request,
                    verdict,
                    shown,
                    Answer::Permissions(requested),
                )
                .await;
            }
            ServerRequest::UserInput(params) => {
                let question = wire::question(QuestionId::new(Uuid::new_v4().to_string()), params);
                self.ask_question(thread_id, request, question).await;
            }
            // Kratos shows no MCP forms yet, so the tool call that asked is declined.
            ServerRequest::McpElicitation(_) => {
                request.answer(&json!({ "action": "cancel", "content": null, "_meta": null }));
            }
        }
    }

    async fn settle_or_ask(
        &mut self,
        thread_id: ThreadId,
        turn_id: TurnId,
        request: IncomingRequest,
        verdict: Verdict,
        shown: ApprovalRequest,
        answer: Answer,
    ) {
        let why = match verdict {
            Verdict::Accept => {
                request.answer(&wire::command_decision(ApprovalDecision::Accept));
                return;
            }
            Verdict::Ask(why) => why,
        };
        let approval = Approval {
            id: ApprovalId::new(Uuid::new_v4().to_string()),
            turn_id: turn_id.clone(),
            request: shown,
            why,
        };
        let id = approval.id.clone();
        if self
            .write(&thread_id, move |write| write.request_approval(approval))
            .await
            .is_err()
        {
            cancel(request);
            return;
        }
        self.requests.approvals.insert(
            id,
            Waiting {
                thread_id,
                turn_id,
                codex_id: request.id.clone(),
                request,
                answer,
            },
        );
    }

    async fn ask_question(
        &mut self,
        thread_id: ThreadId,
        request: IncomingRequest,
        question: kratos_protocol::Question,
    ) {
        let (id, turn_id) = (question.id.clone(), question.turn_id.clone());
        if self
            .write(&thread_id, move |write| write.ask_question(question))
            .await
            .is_err()
        {
            request.answer(&json!({ "answers": {} }));
            return;
        }
        self.requests.questions.insert(
            id,
            Waiting {
                thread_id,
                turn_id,
                codex_id: request.id.clone(),
                request,
                answer: Answer::Answers,
            },
        );
    }
}

fn request_thread(request: &ServerRequest) -> String {
    match request {
        ServerRequest::CommandApproval(params) => params.thread_id.clone(),
        ServerRequest::FileChangeApproval(params) => params.thread_id.clone(),
        ServerRequest::UserInput(params) => params.thread_id.clone(),
        ServerRequest::Permissions(params) => params.thread_id.clone(),
        ServerRequest::McpElicitation(params) => wire::elicitation_thread(params),
    }
}

/// Answers a request Kratos will not put to the user with the refusal of its kind.
fn cancel(request: IncomingRequest) {
    let answer = match &request.request {
        ServerRequest::CommandApproval(_) | ServerRequest::FileChangeApproval(_) => {
            wire::command_decision(ApprovalDecision::Cancel)
        }
        ServerRequest::Permissions(_) => json!({ "permissions": {}, "scope": "turn" }),
        ServerRequest::UserInput(_) => json!({ "answers": {} }),
        ServerRequest::McpElicitation(_) => {
            json!({ "action": "cancel", "content": null, "_meta": null })
        }
    };
    request.answer(&answer);
}
