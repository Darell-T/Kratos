//! What handles ask of the actor.

use std::collections::BTreeMap;
use std::time::Duration;

use kratos_codex_client::{Codex, RequestError, methods, types};
use kratos_protocol::{
    ApprovalDecision, ApprovalId, ApprovalOutcome, CommandResult, ContinuationId,
    ContinuationState, Message, MessageId, MessageInput, MessageOrigin, MessageStatus, PauseReason,
    Provider, QuestionId, QuestionOutcome, RequiredText, Skill, ThreadId, ThreadSettings,
};
use kratos_store::{Enqueued, QueuePlace, Settle, StoreError, ThreadWrite};
use serde_json::json;
use uuid::Uuid;

use super::actor::{Actor, Command, Done, Reply};
use super::profiles::Profile;
use super::requests::Answer;
use super::threads::Turn;
use super::turns::read_history;
use super::{Continued, MAX_PENDING_MESSAGES, ProviderError, Sent, wire};

/// The text of the message Kratos sends to carry on a turn that stopped at a usage limit (D39).
const CONTINUATION_TEXT: &str = "Continue the task you were working on. You stopped earlier because the usage limit was reached.";
/// The smallest context window a thread may ask for (D76).
const MIN_CONTEXT_TOKENS: u64 = 1_000;

impl Command {
    /// Answers the command with `error`, for a command that cannot run.
    pub(super) fn fail(self, error: ProviderError) {
        let message = error.to_string();
        let again = || ProviderError::Unavailable(message.clone());
        match self {
            Command::Send { reply, .. } => drop(reply.send(Err(error))),
            Command::Steer { reply, .. }
            | Command::Remove { reply, .. }
            | Command::Retry { reply, .. }
            | Command::Discard { reply, .. } => drop(reply.send(Err(again()))),
            Command::Stop { reply, .. }
            | Command::Resume { reply, .. }
            | Command::SetSettings { reply, .. }
            | Command::RefreshUsage { reply } => drop(reply.send(Err(again()))),
            Command::Continue { reply, .. } => drop(reply.send(Err(again()))),
            Command::Decide { reply, .. } => drop(reply.send(Err(again()))),
            Command::Answer { reply, .. } => drop(reply.send(Err(again()))),
            Command::Skills { reply, .. } => drop(reply.send(Err(again()))),
            Command::Run { reply, .. } => drop(reply.send(Err(again()))),
            Command::Shutdown { reply } => drop(reply.send(())),
        }
    }
}

impl Actor {
    // #lizard forgives: one arm per command, each a call.
    pub(super) async fn on_command(&mut self, command: Command) {
        match command {
            Command::Send {
                thread_id,
                message_id,
                input,
                reply,
            } => {
                let sent = self.send(&thread_id, message_id, input).await;
                let _ = reply.send(sent);
                self.dispatch(&thread_id).await;
            }
            Command::Steer {
                thread_id,
                message_id,
                reply,
            } => self.steer(&thread_id, message_id, reply).await,
            Command::Remove {
                thread_id,
                message_id,
                reply,
            } => {
                let removed = self.remove(&thread_id, message_id).await;
                let _ = reply.send(removed);
                self.dispatch(&thread_id).await;
            }
            Command::Retry {
                thread_id,
                message_id,
                reply,
            } => {
                let retried = self.retry(&thread_id, message_id).await;
                let _ = reply.send(retried);
                self.dispatch(&thread_id).await;
            }
            Command::Discard {
                thread_id,
                message_id,
                reply,
            } => self.discard(thread_id, message_id, reply).await,
            Command::Stop { thread_id, reply } => self.stop(&thread_id, reply).await,
            Command::Resume { thread_id, reply } => {
                let resumed = self.resume(&thread_id).await;
                let _ = reply.send(resumed);
                self.dispatch(&thread_id).await;
            }
            Command::SetSettings {
                thread_id,
                settings,
                reply,
            } => {
                let set = self.set_settings(&thread_id, settings).await;
                let _ = reply.send(set);
            }
            Command::Decide {
                approval_id,
                decision,
                reply,
            } => {
                let decided = self.decide(approval_id, decision).await;
                let _ = reply.send(decided);
            }
            Command::Answer {
                question_id,
                answers,
                reply,
            } => {
                let answered = self.answer(question_id, answers).await;
                let _ = reply.send(answered);
            }
            Command::Skills { thread_id, reply } => self.skills(thread_id, reply).await,
            Command::Run {
                thread_id,
                command,
                timeout,
                reply,
            } => self.run_command(thread_id, command, timeout, reply).await,
            Command::RefreshUsage { reply } => match self.connection() {
                Ok(Some(codex)) => self.read_usage(codex, Some(reply)),
                Ok(None) => self.deferred.push(Command::RefreshUsage { reply }),
                Err(error) => drop(reply.send(Err(error))),
            },
            Command::Continue {
                thread_id,
                continuation_id,
                reply,
            } => {
                let continued = self.continue_thread(&thread_id, continuation_id).await;
                let _ = reply.send(continued);
                self.dispatch(&thread_id).await;
            }
            Command::Shutdown { reply } => drop(reply.send(())),
        }
    }

    async fn send(
        &mut self,
        thread_id: &ThreadId,
        message_id: MessageId,
        input: Vec<MessageInput>,
    ) -> Result<Sent, ProviderError> {
        self.load(thread_id).await?;
        let enqueued = self
            .write(thread_id, move |write| {
                write.enqueue_message(
                    &message_id,
                    input,
                    MessageOrigin::User,
                    QueuePlace::Back,
                    MAX_PENDING_MESSAGES,
                )
            })
            .await?;
        Ok(match enqueued {
            Enqueued::Added(message) => Sent::Queued(message),
            Enqueued::Existing(message) => Sent::Existing(message),
            Enqueued::Full { pending } => Sent::Full { pending },
        })
    }

    /// Adds a queued message to the running turn. Answers once Codex has the request; the outcome
    /// arrives as the message's status.
    async fn steer(&mut self, thread_id: &ThreadId, message_id: MessageId, reply: Reply<Message>) {
        let result = self.start_steer(thread_id, message_id).await;
        let _ = reply.send(result);
    }

    async fn start_steer(
        &mut self,
        thread_id: &ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        self.load(thread_id).await?;
        let thread = &self.threads[thread_id];
        let (
            Turn::Running {
                turn_id,
                stopping: false,
                ..
            },
            Some(native_id),
        ) = (&thread.turn, thread.native_id())
        else {
            return Err(ProviderError::Conflict(
                "Only a running turn can be steered".into(),
            ));
        };
        let (turn_id, native_id) = (turn_id.clone(), native_id.to_owned());
        let message = self.message(thread_id, &message_id).await?;
        if message.status != MessageStatus::Queued {
            return Err(ProviderError::Conflict(format!(
                "Only a queued message can steer the turn; message {message_id} is not queued"
            )));
        }
        let codex = self
            .connection()?
            .ok_or_else(|| ProviderError::Unavailable("Codex is starting".into()))?;
        let submitting = self
            .write(thread_id, {
                let message_id = message_id.clone();
                move |write| write.set_message_status(&message_id, MessageStatus::Submitting)
            })
            .await?;
        self.threads
            .get_mut(thread_id)
            .expect("loaded above")
            .steering
            .insert(message_id.clone());
        let params = types::TurnSteerParams {
            additional_context: None,
            client_user_message_id: Some(message_id.as_str().to_owned()),
            expected_turn_id: turn_id.as_str().to_owned(),
            input: wire::user_input(&message.input),
            responsesapi_client_metadata: None,
            thread_id: native_id,
        };
        let thread_id = thread_id.clone();
        self.spawn(async move {
            let result = codex
                .request::<methods::TurnSteer>(&params)
                .await
                .map(|_| ());
            Done::Steered {
                thread_id,
                message_id,
                result,
            }
        });
        Ok(submitting)
    }

    pub(super) async fn on_steered(
        &mut self,
        thread_id: ThreadId,
        message_id: MessageId,
        result: Result<(), RequestError>,
    ) {
        let turn_id = self
            .threads
            .get(&thread_id)
            .and_then(|thread| thread.turn.running_id().cloned());
        match (result, turn_id) {
            (Ok(()), Some(turn_id)) => {
                let _ = self
                    .write(&thread_id, {
                        let message_id = message_id.clone();
                        move |write| {
                            write.set_message_status(
                                &message_id,
                                MessageStatus::Accepted { turn_id },
                            )
                        }
                    })
                    .await;
            }
            (Err(RequestError::Rpc { code, .. }), _)
                if kratos_codex_client::rejected_before_execution(code) =>
            {
                // Refused before it ran: the message goes back to the queue.
                let _ = self
                    .write(&thread_id, {
                        let message_id = message_id.clone();
                        move |write| write.set_message_status(&message_id, MessageStatus::Queued)
                    })
                    .await;
            }
            (result, _) => {
                let error = result.err().map_or_else(
                    || "The turn ended before Codex confirmed the steer".to_owned(),
                    |error| error.to_string(),
                );
                // The message stays in `steering` until the history check settles it.
                self.check_delivery(&thread_id, &message_id, None, error);
                return;
            }
        }
        if let Some(thread) = self.threads.get_mut(&thread_id) {
            thread.steering.remove(&message_id);
        }
        self.dispatch(&thread_id).await;
    }

    async fn remove(
        &mut self,
        thread_id: &ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        self.load(thread_id).await?;
        let message = self.message(thread_id, &message_id).await?;
        if !matches!(
            message.status,
            MessageStatus::Queued | MessageStatus::Failed { .. }
        ) {
            return Err(ProviderError::Conflict(
                "Only a queued or failed message can be removed".into(),
            ));
        }
        self.write(thread_id, move |write| {
            let removed = write.set_message_status(&message_id, MessageStatus::Cancelled)?;
            resume_when_settled(write)?;
            Ok(removed)
        })
        .await
    }

    async fn retry(
        &mut self,
        thread_id: &ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        self.load(thread_id).await?;
        let message = self.message(thread_id, &message_id).await?;
        if !matches!(message.status, MessageStatus::Failed { .. }) {
            return Err(ProviderError::Conflict(
                "Only a message Codex refused before acting on it can be sent again".into(),
            ));
        }
        self.write(thread_id, move |write| {
            let queued = write.set_message_status(&message_id, MessageStatus::Queued)?;
            resume_when_settled(write)?;
            Ok(queued)
        })
        .await
    }

    /// Settles an uncertain message from Codex's history. Needs the thread loaded and idle, so the
    /// history is complete.
    async fn discard(&mut self, thread_id: ThreadId, message_id: MessageId, reply: Reply<Message>) {
        let ready = self.discard_ready(&thread_id, &message_id).await;
        let native_id = match ready {
            Ok(Some(native_id)) => native_id,
            Ok(None) => {
                // Codex or the thread is loading; this runs again once it has.
                self.deferred.push(Command::Discard {
                    thread_id,
                    message_id,
                    reply,
                });
                return;
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let codex = self
            .connection()
            .ok()
            .flatten()
            .expect("attached implies connected");
        self.spawn(async move {
            let result = read_history(&codex, &native_id).await.map(Box::new);
            Done::HistoryForDiscard {
                thread_id,
                message_id,
                result,
                reply,
            }
        });
    }

    async fn discard_ready(
        &mut self,
        thread_id: &ThreadId,
        message_id: &MessageId,
    ) -> Result<Option<String>, ProviderError> {
        self.load(thread_id).await?;
        let message = self.message(thread_id, message_id).await?;
        if !matches!(message.status, MessageStatus::Unknown { .. }) {
            return Err(ProviderError::Conflict(
                "Only a message whose delivery is uncertain can be discarded".into(),
            ));
        }
        let thread = &self.threads[thread_id];
        if !thread.turn.is_idle() || !thread.steering.is_empty() {
            return Err(ProviderError::Conflict(
                "Stop the turn before discarding a message".into(),
            ));
        }
        self.attached(thread_id).await
    }

    pub(super) async fn on_history_for_discard(
        &mut self,
        thread_id: ThreadId,
        message_id: MessageId,
        result: Result<Box<types::Thread>, RequestError>,
        reply: Reply<Message>,
    ) {
        let history = match result {
            Ok(history) => history,
            Err(error) => {
                let _ = reply.send(Err(ProviderError::Unavailable(format!(
                    "Codex could not show the thread's history: {error}"
                ))));
                return;
            }
        };
        if !matches!(history.status, types::ThreadStatus::Idle) {
            let _ = reply.send(Err(ProviderError::Conflict(
                "Codex must be idle before a message is discarded".into(),
            )));
            return;
        }
        let found = wire::turn_with_message(&history.turns, message_id.as_str())
            .map(|turn| kratos_protocol::TurnId::new(turn.id.clone()));
        let settled = self
            .write(&thread_id, move |write| {
                // Discarding prevents a replay; it cannot undo work Codex may have done.
                let status = match found {
                    Some(turn_id) => MessageStatus::Accepted { turn_id },
                    None => MessageStatus::Cancelled,
                };
                let settled = write.set_message_status(&message_id, status)?;
                resume_when_settled(write)?;
                Ok(settled)
            })
            .await;
        let _ = reply.send(settled);
        self.dispatch(&thread_id).await;
    }

    async fn stop(&mut self, thread_id: &ThreadId, reply: Reply<()>) {
        if let Err(error) = self.load(thread_id).await {
            let _ = reply.send(Err(error));
            return;
        }
        let thread = self.threads.get_mut(thread_id).expect("loaded above");
        match &mut thread.turn {
            Turn::Idle => drop(reply.send(Ok(()))),
            // Stopped as soon as its id is known.
            Turn::Starting { stop_waiters, .. } => stop_waiters.push(reply),
            Turn::Running { .. } => self.interrupt(thread_id, vec![reply]),
        }
    }

    /// Lets a paused queue run again. Refused while a message holds the queue, because only the
    /// user may settle it (D11). An open continuation is cancelled: the user took over (D39).
    async fn resume(&mut self, thread_id: &ThreadId) -> Result<(), ProviderError> {
        self.load(thread_id).await?;
        self.write(thread_id, |write| {
            let held = write.pending_messages()?.into_iter().find(|message| {
                !matches!(message.status, MessageStatus::Queued)
            });
            if let Some(held) = held {
                return Err(StoreError::MessageTransition {
                    message_id: held.id,
                    from: "held",
                    to: "resumed",
                });
            }
            if let Some(open) = write.open_continuation()? {
                write.update_continuation(
                    &open.id,
                    open.resets_at,
                    ContinuationState::Cancelled {
                        reason: "The user resumed the thread".into(),
                    },
                )?;
            }
            write.resume()?;
            Ok(())
        })
        .await
        .map_err(|error| match error {
            ProviderError::Store(StoreError::MessageTransition { message_id, .. }) => {
                ProviderError::Conflict(format!(
                    "Settle message {message_id} before resuming: retry or remove it if it failed, discard it if its delivery is uncertain"
                ))
            }
            other => other,
        })
    }

    /// Replaces the thread's settings. They apply from the next turn (D87): a context limit
    /// applies once Codex loads the thread again, which happens before that turn.
    async fn set_settings(
        &mut self,
        thread_id: &ThreadId,
        settings: ThreadSettings,
    ) -> Result<(), ProviderError> {
        self.load(thread_id).await?;
        if settings
            .model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(ProviderError::Unsupported(
                "model: must not be blank".into(),
            ));
        }
        if let Some(limit) = settings.context_limit_tokens {
            self.check_context_limit(thread_id, settings.model.as_deref(), limit)?;
        }
        let stored = settings.clone();
        self.write(thread_id, move |write| write.set_settings(stored))
            .await?;
        self.threads
            .get_mut(thread_id)
            .expect("loaded above")
            .settings = settings;
        Ok(())
    }

    /// A context window between the floor and the window the model reported with no setting of
    /// Kratos's in force. Kratos never claims a window larger than the model's (D76).
    fn check_context_limit(
        &self,
        thread_id: &ThreadId,
        model: Option<&str>,
        limit: u64,
    ) -> Result<(), ProviderError> {
        let model = model.or_else(|| self.threads[thread_id].model());
        let window = model.and_then(|model| self.windows.get(model)).copied();
        match window {
            _ if limit < MIN_CONTEXT_TOKENS => Err(ProviderError::Unsupported(format!(
                "contextLimitTokens: must be at least {MIN_CONTEXT_TOKENS}"
            ))),
            Some(window) if limit > window as u64 => Err(ProviderError::Unsupported(format!(
                "contextLimitTokens: the model's context window is {window} tokens"
            ))),
            Some(_) => Ok(()),
            None => Err(ProviderError::Unsupported(
                "contextLimitTokens: Codex has not reported this model's context window yet; run a turn first"
                    .into(),
            )),
        }
    }

    async fn decide(
        &mut self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<ApprovalOutcome, ProviderError> {
        let Some(waiting) = self.requests.approvals.remove(&approval_id) else {
            return self.recorded_approval(&approval_id).await;
        };
        let outcome = {
            let approval_id = approval_id.clone();
            self.write(&waiting.thread_id, move |write| {
                write.settle_approval(&approval_id, Settle::Decide(decision))
            })
            .await
        };
        match (&outcome, &waiting.answer) {
            (
                Ok(ApprovalOutcome::Decided { decision: recorded }),
                Answer::Permissions(requested),
            ) if *recorded == decision => {
                waiting
                    .request
                    .answer(&wire::permissions_decision(decision, requested));
            }
            (Ok(ApprovalOutcome::Decided { decision: recorded }), _) if *recorded == decision => {
                waiting.request.answer(&wire::command_decision(decision));
            }
            // Recorded otherwise, or not recorded: dropping the request answers Codex.
            _ => {}
        }
        outcome
    }

    async fn recorded_approval(
        &self,
        approval_id: &ApprovalId,
    ) -> Result<ApprovalOutcome, ProviderError> {
        let lookup = approval_id.clone();
        self.store
            .call(move |store| store.approval(&lookup))
            .await?
            .map(|stored| stored.outcome)
            .ok_or_else(|| ProviderError::UnknownApproval(approval_id.clone()))
    }

    async fn answer(
        &mut self,
        question_id: QuestionId,
        answers: BTreeMap<String, Vec<String>>,
    ) -> Result<QuestionOutcome, ProviderError> {
        let Some(waiting) = self.requests.questions.remove(&question_id) else {
            let lookup = question_id.clone();
            return self
                .store
                .call(move |store| store.question(&lookup))
                .await?
                .map(|stored| stored.outcome)
                .ok_or(ProviderError::UnknownQuestion(question_id));
        };
        let outcome = self
            .write(&waiting.thread_id, move |write| {
                write.settle_question(&question_id, Settle::Decide(()))
            })
            .await;
        if matches!(outcome, Ok(QuestionOutcome::Answered)) {
            waiting.request.answer(&wire::answers(answers));
        }
        outcome
    }

    async fn skills(&mut self, thread_id: ThreadId, reply: Reply<Vec<Skill>>) {
        if let Err(error) = self.load(&thread_id).await {
            let _ = reply.send(Err(error));
            return;
        }
        let codex = match self.connection() {
            Ok(Some(codex)) => codex,
            Ok(None) => {
                self.deferred.push(Command::Skills { thread_id, reply });
                return;
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let folder = self.threads[&thread_id].folder.clone();
        tokio::spawn(async move {
            let _ = reply.send(list_skills(&codex, folder).await);
        });
    }

    async fn run_command(
        &mut self,
        thread_id: ThreadId,
        command: Vec<String>,
        timeout: Duration,
        reply: Reply<CommandResult>,
    ) {
        if let Err(error) = self.load(&thread_id).await {
            let _ = reply.send(Err(error));
            return;
        }
        let codex = match self.connection() {
            Ok(Some(codex)) => codex,
            Ok(None) => {
                self.deferred.push(Command::Run {
                    thread_id,
                    command,
                    timeout,
                    reply,
                });
                return;
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let thread = &self.threads[&thread_id];
        let profile = Profile::for_settings(&thread.settings).id().to_owned();
        let folder = thread.folder.clone();
        tokio::spawn(async move {
            let _ = reply.send(Ok(exec(&codex, command, folder, profile, timeout).await));
        });
    }

    /// Queues the continuation's message ahead of the waiting ones, marks it sent, and lets the
    /// queue run, in one write (D39). A continuation that is no longer open sends nothing. One
    /// that would run past a message waiting on the user, or past a pause the limit did not cause,
    /// or that finds the queue full (D85), goes back to the user.
    async fn continue_thread(
        &mut self,
        thread_id: &ThreadId,
        continuation_id: ContinuationId,
    ) -> Result<Continued, ProviderError> {
        self.load(thread_id).await?;
        let message_id = MessageId::new(Uuid::new_v4().to_string());
        let input = vec![MessageInput::Text {
            text: RequiredText::new(CONTINUATION_TEXT).expect("the continuation text is not blank"),
        }];
        self.write(thread_id, move |write| {
            if let Some(why) = blocks_continuation(write)? {
                return Ok(Continued::NeedsUser(why));
            }
            let sent = write.send_continuation(
                &continuation_id,
                &message_id,
                input,
                MAX_PENDING_MESSAGES,
            )?;
            Ok(match sent {
                Some(Enqueued::Added(_) | Enqueued::Existing(_)) => {
                    write.resume()?;
                    Continued::Queued
                }
                Some(Enqueued::Full { pending }) => Continued::NeedsUser(format!(
                    "The thread's queue is full with {pending} messages: remove one, then let Kratos continue"
                )),
                None => Continued::NotOpen,
            })
        })
        .await
    }

    /// Reads the account's usage; a read finished after the account changed is dropped (D38).
    pub(super) fn read_usage(&self, codex: Codex, reply: Option<Reply<()>>) {
        let tracker_generation = self.usage.generation(Provider::Codex);
        self.spawn(async move {
            let result = codex
                .request::<methods::RateLimitsRead>(&None)
                .await
                .map(Box::new);
            Done::UsageRead {
                tracker_generation,
                result,
                reply,
            }
        });
    }

    pub(super) fn on_usage_read(
        &mut self,
        tracker_generation: u64,
        result: Result<Box<types::GetAccountRateLimitsResponse>, RequestError>,
        reply: Option<Reply<()>>,
    ) {
        let answer = match result {
            Ok(read) => {
                let usage = wire::account_usage(&read, self.plan.clone());
                self.usage.record_read(tracker_generation, usage);
                Ok(())
            }
            Err(error) => {
                let reason = format!("Codex did not report the account's usage: {error}");
                self.usage
                    .record_unavailable(Provider::Codex, reason.clone());
                Err(ProviderError::Unavailable(reason))
            }
        };
        if let Some(reply) = reply {
            let _ = reply.send(answer);
        }
    }

    async fn message(
        &self,
        thread_id: &ThreadId,
        message_id: &MessageId,
    ) -> Result<Message, ProviderError> {
        let (thread, lookup) = (thread_id.clone(), message_id.clone());
        self.store
            .call(move |store| store.message(&thread, &lookup))
            .await?
            .ok_or_else(|| ProviderError::UnknownMessage(message_id.clone()))
    }
}

/// Why the thread cannot take a continuation on its own, if it cannot: a message waits on the user,
/// or the queue is paused for something the reset does not fix.
fn blocks_continuation(write: &mut ThreadWrite<'_>) -> Result<Option<String>, StoreError> {
    if let Some(held) = write.pending_messages()?.into_iter().find(|message| {
        matches!(
            message.status,
            MessageStatus::Failed { .. } | MessageStatus::Unknown { .. }
        )
    }) {
        return Ok(Some(format!(
            "Message {} waits for you to retry, remove, or discard it",
            held.id
        )));
    }
    Ok(match write.provider_thread()?.paused {
        None | Some(PauseReason::UsageLimit { .. } | PauseReason::Unavailable { .. }) => None,
        Some(_) => Some("The thread is paused for another reason; resume it yourself".to_owned()),
    })
}

/// Lifts a pause that a refused or uncertain message caused, once no such message is left.
fn resume_when_settled(write: &mut ThreadWrite<'_>) -> Result<(), StoreError> {
    let held = write.pending_messages()?.iter().any(|message| {
        matches!(
            message.status,
            MessageStatus::Failed { .. } | MessageStatus::Unknown { .. }
        )
    });
    let paused_by_message = matches!(
        write.provider_thread()?.paused,
        Some(PauseReason::Refused { .. } | PauseReason::DeliveryUncertain { .. })
    );
    if paused_by_message && !held {
        write.resume()?;
    }
    Ok(())
}

async fn list_skills(codex: &Codex, folder: String) -> Result<Vec<Skill>, ProviderError> {
    let params = types::SkillsListParams {
        cwds: vec![folder],
        force_reload: Some(false),
    };
    let listed = codex
        .request::<methods::SkillsList>(&params)
        .await
        .map_err(|error| {
            ProviderError::Unavailable(format!("Codex did not list skills: {error}"))
        })?;
    Ok(listed
        .data
        .into_iter()
        .flat_map(|entry| entry.skills)
        .map(|skill| Skill {
            name: skill.name,
            description: skill.description,
            path: skill.path.0,
            enabled: skill.enabled,
        })
        .collect())
}

/// Runs `command` through Codex's command runner, in `folder`, inside `profile`'s sandbox.
async fn exec(
    codex: &Codex,
    command: Vec<String>,
    folder: String,
    profile: String,
    timeout: Duration,
) -> CommandResult {
    let mut params: types::CommandExecParams = wire::required(json!({ "command": command }));
    params.cwd = Some(folder);
    params.permission_profile = Some(profile);
    params.timeout_ms = Some(i64::try_from(timeout.as_millis()).unwrap_or(i64::MAX));
    // The runner's own deadline answers first; this one only bounds a Codex that never answers.
    let deadline = timeout + Duration::from_secs(10);
    match codex
        .request_within::<methods::CommandExec>(&params, deadline)
        .await
    {
        Ok(done) => wire::command_result(done),
        // On Windows a command that runs out of time fails the request with this text.
        Err(kratos_codex_client::RequestError::Rpc { message, .. })
            if message.contains("timed out") =>
        {
            CommandResult::TimedOut {
                output: String::new(),
            }
        }
        Err(error) => CommandResult::Refused {
            error: error.to_string(),
        },
    }
}
