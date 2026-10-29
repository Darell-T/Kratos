//! A thread's turns: when the next queued message starts, how a start whose answer was lost is
//! settled, and how a turn ends.

use std::time::Duration;

use kratos_codex_client::{Codex, RequestError, methods, rejected_before_execution, types};
use kratos_protocol::{
    Message, MessageId, MessageStatus, PauseReason, ThreadId, TurnId, TurnOutcome,
};
use serde_json::json;

use super::actor::{Actor, Done, MAX_STARTING, Reply};
use super::profiles::Profile;
use super::threads::Turn;
use super::{ProviderError, wire};

/// How long `turn/interrupt` may take to answer.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(5);

/// What follows a turn's end for the messages queued behind it.
pub(super) enum After {
    Continue,
    /// Pause when messages wait, so none starts after a turn that did not finish cleanly.
    PauseIfWaiting(PauseReason),
    /// Pause whatever waits, because something other than the queue decides what comes next.
    Pause(PauseReason),
}

/// Who asked Codex to stop a turn, which decides what follows when Codex does not stop it.
pub(super) enum Stopper {
    /// The turn ran out of time. Nothing would ask again, so Codex is closed instead.
    Deadline,
    /// Users who asked. They hear whether Codex stopped the turn, and may ask again.
    Users(Vec<Reply<()>>),
}

impl Stopper {
    fn tell(self, result: impl Fn() -> Result<(), ProviderError>) {
        if let Stopper::Users(waiters) = self {
            for waiter in waiters {
                let _ = waiter.send(result());
            }
        }
    }
}

impl Actor {
    /// Starts the thread's next queued message when nothing holds the queue: the turn before it is
    /// confirmed finished (D11), no steer is out, the queue is not paused, Codex has the thread
    /// loaded, and a start slot is free. Safe to call at any time; it does nothing otherwise.
    pub(super) async fn dispatch(&mut self, thread_id: &ThreadId) {
        let Some(thread) = self.threads.get(thread_id) else {
            return;
        };
        if !thread.turn.is_idle() || !thread.steering.is_empty() {
            return;
        }
        let Some(next) = self.next_to_start(thread_id).await else {
            return;
        };
        let native_id = match self.attached(thread_id).await {
            Ok(Some(native_id)) => native_id,
            // Loading; the load's completion dispatches again.
            Ok(None) => return,
            Err(error) => {
                let error = error.to_string();
                self.pause_if_waiting(thread_id, PauseReason::Unavailable { error })
                    .await;
                return;
            }
        };
        let starting = self
            .threads
            .values()
            .filter(|thread| matches!(thread.turn, Turn::Starting { .. }))
            .count();
        if starting >= MAX_STARTING {
            if !self.ready.contains(thread_id) {
                self.ready.push_back(thread_id.clone());
            }
            return;
        }
        self.start_turn(thread_id, native_id, next).await;
    }

    /// The thread's first waiting message when it may start: the queue is not paused, and the
    /// message is queued. A failed or uncertain message holds the queue until the user settles it.
    async fn next_to_start(&self, thread_id: &ThreadId) -> Option<Message> {
        let thread_id = thread_id.clone();
        let next = self
            .store
            .call(move |store| {
                let paused = store.provider_thread(&thread_id)?.paused;
                let first = store.pending_messages(&thread_id)?.into_iter().next();
                Ok::<_, kratos_store::StoreError>((paused, first))
            })
            .await;
        match next {
            Ok((None, Some(next))) if next.status == MessageStatus::Queued => Some(next),
            _ => None,
        }
    }

    /// Gives a freed start slot to the thread that waited longest.
    pub(super) async fn start_ready(&mut self) {
        while let Some(thread_id) = self.ready.pop_front() {
            self.dispatch(&thread_id).await;
            if self.ready.contains(&thread_id) {
                // Still no slot; it keeps its place.
                return;
            }
        }
    }

    async fn start_turn(&mut self, thread_id: &ThreadId, native_id: String, message: Message) {
        let Some(codex) = self.connection().ok().flatten() else {
            return;
        };
        let message_id = message.id.clone();
        let recorded = self
            .write(thread_id, {
                let message_id = message_id.clone();
                move |write| write.set_message_status(&message_id, MessageStatus::Submitting)
            })
            .await;
        if recorded.is_err() {
            return;
        }
        let attempt = self.next_attempt;
        self.next_attempt += 1;
        let thread = self
            .threads
            .get_mut(thread_id)
            .expect("dispatch checked the thread");
        thread.turn = Turn::Starting {
            attempt,
            message_id: message_id.clone(),
            early_completion: None,
            stop_waiters: Vec::new(),
        };
        thread.changes.take();
        thread.context = None;
        thread.file_items.clear();
        thread.turn_settings = thread.settings.clone();
        let mut params = turn_start_params(native_id, &message, thread.model(), &thread.settings);
        params.runtime_workspace_roots = Some(vec![types::AbsolutePathBuf(thread.folder.clone())]);
        let thread_id = thread_id.clone();
        self.spawn(async move {
            let result = match codex.request::<methods::TurnStart>(&params).await {
                // Refused before it ran, so a second try cannot run it twice.
                Err(RequestError::Rpc { code, .. }) if rejected_before_execution(code) => {
                    codex.request::<methods::TurnStart>(&params).await
                }
                result => result,
            };
            Done::StartAnswered {
                thread_id,
                attempt,
                result: result.map(|started| Box::new(started.turn)),
            }
        });
    }

    pub(super) async fn on_start_answered(
        &mut self,
        thread_id: ThreadId,
        attempt: u64,
        result: Result<Box<types::Turn>, RequestError>,
    ) {
        let Some(Turn::Starting {
            attempt: current,
            message_id,
            ..
        }) = self.threads.get(&thread_id).map(|thread| &thread.turn)
        else {
            return;
        };
        if *current != attempt {
            return;
        }
        let message_id = message_id.clone();
        match result {
            Ok(turn) => self.accept_start(&thread_id, &message_id, *turn).await,
            Err(RequestError::Rpc { code, message }) if rejected_before_execution(code) => {
                self.refuse_start(&thread_id, &message_id, message).await;
            }
            Err(error) => {
                self.check_delivery(&thread_id, &message_id, Some(attempt), error.to_string());
            }
        }
        self.start_ready().await;
    }

    /// Codex refused the message before acting on it. It may be sent again, by the user.
    async fn refuse_start(&mut self, thread_id: &ThreadId, message_id: &MessageId, error: String) {
        self.end_start(
            thread_id,
            Err(ProviderError::Conflict(format!(
                "Codex refused the message: {error}"
            ))),
        );
        let message_id = message_id.clone();
        let _ = self
            .write(thread_id, move |write| {
                write.set_message_status(
                    &message_id,
                    MessageStatus::Failed {
                        error: error.clone(),
                    },
                )?;
                write.pause(PauseReason::Refused { message_id, error })?;
                Ok(())
            })
            .await;
    }

    /// The answer to `turn/start` or `turn/steer` was lost, so Codex may or may not have taken the
    /// message. Codex's history says which; the message is never sent again to find out (D11).
    pub(super) fn check_delivery(
        &self,
        thread_id: &ThreadId,
        message_id: &MessageId,
        attempt: Option<u64>,
        error: String,
    ) {
        let (thread_id, message_id) = (thread_id.clone(), message_id.clone());
        let codex = match &self.server {
            super::actor::Server::Up { codex, .. } => Some(codex.clone()),
            _ => None,
        };
        let native_id = self
            .threads
            .get(&thread_id)
            .and_then(|thread| thread.native_id().map(str::to_owned));
        self.spawn(async move {
            let found = match (codex, native_id) {
                (Some(codex), Some(native_id)) => read_history(&codex, &native_id)
                    .await
                    .ok()
                    .and_then(|thread| {
                        wire::turn_with_message(&thread.turns, message_id.as_str())
                            .cloned()
                            .map(Box::new)
                    }),
                _ => None,
            };
            Done::DeliveryChecked {
                thread_id,
                message_id,
                attempt,
                error,
                found,
            }
        });
    }

    pub(super) async fn on_delivery_checked(
        &mut self,
        thread_id: ThreadId,
        message_id: MessageId,
        attempt: Option<u64>,
        error: String,
        found: Option<Box<types::Turn>>,
    ) {
        match attempt {
            Some(attempt) => {
                let is_current = matches!(
                    self.threads.get(&thread_id).map(|thread| &thread.turn),
                    Some(Turn::Starting { attempt: current, .. }) if *current == attempt
                );
                if !is_current {
                    return;
                }
                match found {
                    Some(turn) => self.accept_start(&thread_id, &message_id, *turn).await,
                    None => {
                        self.end_start(&thread_id, Ok(()));
                        self.record_uncertain(&thread_id, message_id, error).await;
                    }
                }
            }
            None => {
                if let Some(thread) = self.threads.get_mut(&thread_id) {
                    thread.steering.remove(&message_id);
                }
                match found {
                    Some(turn) => {
                        let turn_id = TurnId::new(turn.id);
                        let _ = self
                            .write(&thread_id, move |write| {
                                write.set_message_status(
                                    &message_id,
                                    MessageStatus::Accepted { turn_id },
                                )
                            })
                            .await;
                    }
                    None => self.record_uncertain(&thread_id, message_id, error).await,
                }
                self.dispatch(&thread_id).await;
            }
        }
    }

    async fn record_uncertain(
        &mut self,
        thread_id: &ThreadId,
        message_id: MessageId,
        error: String,
    ) {
        let _ = self
            .write(thread_id, move |write| {
                write.set_message_status(
                    &message_id,
                    MessageStatus::Unknown {
                        error: error.clone(),
                    },
                )?;
                write.pause(PauseReason::DeliveryUncertain { message_id, error })?;
                Ok(())
            })
            .await;
    }

    /// Codex took the message into `turn`. A turn already finished, by the time its id was known,
    /// ends here too.
    async fn accept_start(
        &mut self,
        thread_id: &ThreadId,
        message_id: &MessageId,
        turn: types::Turn,
    ) {
        let turn_id = TurnId::new(turn.id.clone());
        let recorded = self
            .write(thread_id, {
                let (message_id, turn_id) = (message_id.clone(), turn_id.clone());
                move |write| {
                    write.set_message_status(
                        &message_id,
                        MessageStatus::Accepted {
                            turn_id: turn_id.clone(),
                        },
                    )?;
                    write.start_turn(turn_id)
                }
            })
            .await;
        let thread = self.threads.get_mut(thread_id).expect("a starting thread");
        let Turn::Starting {
            early_completion,
            stop_waiters,
            ..
        } = std::mem::replace(
            &mut thread.turn,
            Turn::Running {
                turn_id: turn_id.clone(),
                stopping: false,
                deadline_epoch: 0,
            },
        )
        else {
            unreachable!("accept_start follows a start");
        };
        if recorded.is_err() {
            return;
        }
        let finished = early_completion
            .filter(|completed| completed.id == turn.id)
            .map(|completed| *completed)
            .or_else(|| wire::turn_outcome(&turn).map(|_| turn.clone()));
        if let Some(finished) = finished {
            for waiter in stop_waiters {
                let _ = waiter.send(Ok(()));
            }
            self.finish_turn(thread_id, finished).await;
            return;
        }
        self.arm_deadline(thread_id);
        if !stop_waiters.is_empty() {
            self.interrupt(thread_id, Stopper::Users(stop_waiters));
        }
    }

    /// Leaves the starting state, answering any stop that waited for the start.
    fn end_start(&mut self, thread_id: &ThreadId, for_stop: Result<(), ProviderError>) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        if let Turn::Starting { stop_waiters, .. } = std::mem::replace(&mut thread.turn, Turn::Idle)
        {
            for waiter in stop_waiters {
                let _ = waiter.send(match &for_stop {
                    Ok(()) => Ok(()),
                    Err(error) => Err(ProviderError::Conflict(error.to_string())),
                });
            }
        }
    }

    /// Codex reported `turn` finished. A completion that arrives before the start's answer waits
    /// for it; one for a turn not running, such as one already recorded, changes nothing.
    pub(super) async fn finish_turn(&mut self, thread_id: &ThreadId, turn: types::Turn) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        if let Turn::Starting {
            early_completion, ..
        } = &mut thread.turn
        {
            *early_completion = Some(Box::new(turn));
            return;
        }
        let is_running = thread
            .turn
            .running_id()
            .is_some_and(|running| running.as_str() == turn.id);
        if !is_running {
            return;
        }
        let turn_id = TurnId::new(turn.id.clone());
        thread.turn = Turn::Idle;
        let changes = thread.changes.take();
        let outcome = wire::turn_outcome(&turn).unwrap_or(TurnOutcome::Failed {
            error: "Codex reported the turn finished while still in progress".to_owned(),
        });
        let at_limit = wire::stopped_at_usage_limit(&turn);
        let after = match &outcome {
            _ if at_limit => After::Pause(PauseReason::UsageLimit {
                turn_id: turn_id.clone(),
                resets_at: None,
            }),
            TurnOutcome::Failed { error } => After::PauseIfWaiting(PauseReason::TurnFailed {
                turn_id: turn_id.clone(),
                error: error.clone(),
            }),
            TurnOutcome::Completed | TurnOutcome::Interrupted => After::Continue,
        };
        if self
            .record_turn_end(thread_id, turn_id.clone(), outcome, changes, after)
            .await
            .is_err()
        {
            return;
        }
        if at_limit {
            self.report_limit_stop(thread_id, turn_id);
        }
        self.dispatch(thread_id).await;
    }

    /// Sets the running turn's deadline again, from now.
    pub(super) fn arm_deadline(&mut self, thread_id: &ThreadId) {
        let Some(Turn::Running {
            turn_id,
            deadline_epoch,
            ..
        }) = self
            .threads
            .get_mut(thread_id)
            .map(|thread| &mut thread.turn)
        else {
            return;
        };
        *deadline_epoch += 1;
        let (epoch, turn_id, thread_id) = (*deadline_epoch, turn_id.clone(), thread_id.clone());
        let timeout = self.settings.turn_timeout;
        self.spawn(async move {
            tokio::time::sleep(timeout).await;
            Done::Deadline {
                thread_id,
                turn_id,
                epoch,
            }
        });
    }

    /// The turn ran out of time. One that waits on the user is not counted, and its deadline
    /// starts again; any other is stopped, and the queue behind it pauses.
    pub(super) async fn on_deadline(&mut self, thread_id: ThreadId, turn_id: TurnId, epoch: u64) {
        let is_current = matches!(
            self.threads.get(&thread_id).map(|thread| &thread.turn),
            Some(Turn::Running { turn_id: running, stopping: false, deadline_epoch })
                if *running == turn_id && *deadline_epoch == epoch
        );
        if !is_current {
            return;
        }
        if self.requests.blocks(&thread_id) {
            self.arm_deadline(&thread_id);
            return;
        }
        let paused = self
            .write(&thread_id, move |write| {
                write.pause(PauseReason::TimedOut { turn_id })
            })
            .await;
        if paused.is_ok() {
            self.interrupt(&thread_id, Stopper::Deadline);
        }
    }

    /// Asks Codex to stop the running turn.
    pub(super) fn interrupt(&mut self, thread_id: &ThreadId, stopper: Stopper) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        let Turn::Running {
            turn_id, stopping, ..
        } = &mut thread.turn
        else {
            stopper.tell(|| Ok(()));
            return;
        };
        *stopping = true;
        let turn_id = turn_id.clone();
        let (Some(native_id), Some(codex)) = (
            thread.native_id().map(str::to_owned),
            self.connection().ok().flatten(),
        ) else {
            stopper.tell(|| Err(ProviderError::Unavailable("Codex is not running".into())));
            return;
        };
        let thread_id = thread_id.clone();
        self.spawn(async move {
            let params = types::TurnInterruptParams {
                thread_id: native_id,
                turn_id: turn_id.as_str().to_owned(),
            };
            let result = codex
                .request_within::<methods::TurnInterrupt>(&params, INTERRUPT_TIMEOUT)
                .await
                .map(|_| ());
            Done::Interrupted {
                thread_id,
                turn_id,
                result,
                stopper,
            }
        });
    }

    pub(super) async fn on_interrupted(
        &mut self,
        thread_id: ThreadId,
        turn_id: TurnId,
        result: Result<(), RequestError>,
        stopper: Stopper,
    ) {
        let still_running = self
            .threads
            .get(&thread_id)
            .and_then(|thread| thread.turn.running_id())
            == Some(&turn_id);
        match result {
            Ok(()) => {
                if still_running {
                    let timeout = self.settings.stop_timeout;
                    self.spawn(async move {
                        tokio::time::sleep(timeout).await;
                        Done::StopExpired { thread_id, turn_id }
                    });
                }
                stopper.tell(|| Ok(()));
            }
            Err(error) if still_running && matches!(stopper, Stopper::Deadline) => {
                self.stop_server(&format!("a timed-out turn did not stop: {error}"))
                    .await;
            }
            Err(error) => {
                if still_running
                    && let Some(Turn::Running { stopping, .. }) = self
                        .threads
                        .get_mut(&thread_id)
                        .map(|thread| &mut thread.turn)
                {
                    // Another stop may try again; the queue stays where it is.
                    *stopping = false;
                }
                stopper.tell(|| {
                    Err(ProviderError::Unavailable(format!(
                        "Codex did not stop the turn: {error}"
                    )))
                });
            }
        }
    }

    /// Codex accepted the stop but the turn did not end in time: Codex is closed, which ends the
    /// turn as failed.
    pub(super) async fn on_stop_expired(&mut self, thread_id: &ThreadId, turn_id: &TurnId) {
        let still_stopping = matches!(
            self.threads.get(thread_id).map(|thread| &thread.turn),
            Some(Turn::Running { turn_id: running, stopping: true, .. }) if running == turn_id
        );
        if still_stopping {
            self.stop_server("a stopped turn did not end").await;
        }
    }
}

fn turn_start_params(
    native_id: String,
    message: &Message,
    model: Option<&str>,
    settings: &kratos_protocol::ThreadSettings,
) -> types::TurnStartParams {
    let mut params: types::TurnStartParams = wire::required(json!({
        "threadId": native_id,
        "input": wire::user_input(&message.input),
    }));
    params.client_user_message_id = Some(message.id.as_str().to_owned());
    params.model = settings.model.clone();
    params.approval_policy = Some(types::AskForApproval::Untrusted);
    params.approvals_reviewer = Some(types::ApprovalsReviewer::User);
    params.permissions = Some(Profile::for_settings(settings).id().to_owned());
    params.collaboration_mode = model.map(|model| types::CollaborationMode {
        mode: if settings.plan_mode {
            types::ModeKind::Plan
        } else {
            types::ModeKind::Default
        },
        settings: types::Settings {
            developer_instructions: None,
            model: model.to_owned(),
            reasoning_effort: None,
        },
    });
    params
}

pub(super) async fn read_history(
    codex: &Codex,
    native_id: &str,
) -> Result<types::Thread, RequestError> {
    let params = types::ThreadReadParams {
        include_turns: Some(true),
        thread_id: native_id.to_owned(),
    };
    Ok(codex.request::<methods::ThreadRead>(&params).await?.thread)
}
