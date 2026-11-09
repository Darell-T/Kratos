//! A thread's turns: when the next queued message is written to Claude, how a message joins the
//! turn that runs, how a stop ends it, and how a result closes it.
//!
//! A turn lasts while any message written to Claude lacks a result (D11). Claude may take a
//! message into the running turn or run it after, so each result closes the run it reports, and a
//! message still waiting for its result starts the next run when Claude takes it.

use kratos_claude_client::{Control, TurnResult};
use kratos_protocol::{
    LiveUpdate, Message, MessageId, MessageStatus, PauseReason, Provider, ThreadId, TurnId,
};
use kratos_store::{StoreError, ThreadWrite};

use super::actor::{Actor, Done, Reply};
use super::ends::{Stop, TurnEnd, end_turn};
use super::threads::{InFlight, SessionConfig};
use super::{ProviderError, wire};
use crate::providers::queue::mark_refused;

/// Why Kratos ends a CLI that was asked to stop and did not.
const STOP_FAILED: &str = "Claude did not stop the turn; its process was closed";

impl Actor {
    /// Writes the thread's next queued message to Claude when nothing holds the queue: no message
    /// is in flight, the queue is not paused, and a process with the right settings runs. Safe to
    /// call at any time; it does nothing otherwise.
    pub(super) async fn dispatch(&mut self, thread_id: &ThreadId) {
        if self.parked.is_some() {
            return;
        }
        let Some(thread) = self.threads.get(thread_id) else {
            return;
        };
        if thread.is_busy() {
            return;
        }
        let Some(next) = self.next_to_start(thread_id).await else {
            // Nothing waits to be sent, so a pending start has no work.
            self.threads
                .get_mut(thread_id)
                .expect("found above")
                .restarts
                .cancel_timer();
            return;
        };
        let thread = &self.threads[thread_id];
        let wanted = SessionConfig::of(&effective(thread, &next));
        match thread.open() {
            Some(open) if open.config == wanted => self.start_turn(thread_id, next).await,
            Some(_) => {
                self.retire_session(thread_id);
                let _ = self.open_session(thread_id, wanted).await;
            }
            None => {
                let _ = self.open_session(thread_id, wanted).await;
            }
        }
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

    async fn start_turn(&mut self, thread_id: &ThreadId, message: Message) {
        let settings = &self.threads[thread_id].settings;
        let lowered = message
            .origin
            .limit()
            .map(|limit| settings.limited_to(limit))
            .filter(|lowered| lowered != settings);
        let recorded = self
            .write(thread_id, {
                let (message_id, lowered) = (message.id.clone(), lowered.clone());
                move |write| {
                    write.set_message_status(&message_id, MessageStatus::Submitting)?;
                    lowered.map_or(Ok(()), |settings| write.set_settings(settings))
                }
            })
            .await;
        if recorded.is_err() {
            return;
        }
        if let Some(lowered) = lowered {
            self.threads
                .get_mut(thread_id)
                .expect("dispatch loaded the thread")
                .settings = lowered;
        }
        let thread = self.threads.get_mut(thread_id).expect("dispatch loaded it");
        thread.turn_settings = Some(thread.settings.clone());
        thread.command_grants.clear();
        self.deliver(thread_id, &message);
    }

    /// Writes `message` to Claude. It counts as in flight from here until a result names it.
    fn deliver(&mut self, thread_id: &ThreadId, message: &Message) {
        let thread = self
            .threads
            .get_mut(thread_id)
            .expect("the caller loaded the thread");
        let claude = thread
            .open()
            .expect("a message is written to an open session")
            .claude
            .clone();
        let native = wire::native_id(&message.id).to_string();
        let first = thread.pending.is_empty();
        if first {
            // What an earlier turn or an idle moment left must not decide this turn's end.
            thread.rejection = None;
            thread.answering_model = None;
        }
        thread.pending.push(InFlight {
            message_id: message.id.clone(),
            native: native.clone(),
        });
        let epoch = thread.epoch;
        if first {
            self.arm_deadline(thread_id);
        }
        let (thread_id, message_id) = (thread_id.clone(), message.id.clone());
        let content = wire::user_content(&message.input);
        self.spawn(async move {
            let result = claude.send(&native, content).await;
            Done::Sent {
                thread_id,
                epoch,
                message_id,
                result,
            }
        });
    }

    pub(super) async fn on_sent(
        &mut self,
        thread_id: ThreadId,
        epoch: u64,
        message_id: MessageId,
        result: Result<(), kratos_claude_client::RequestError>,
    ) {
        let Some(thread) = self.threads.get_mut(&thread_id) else {
            return;
        };
        if thread.epoch != epoch {
            return;
        }
        match result {
            Ok(()) => {
                if !thread.stop_waiters.is_empty() && !thread.stopping {
                    let waiters = std::mem::take(&mut thread.stop_waiters);
                    self.interrupt(&thread_id, waiters, false);
                }
            }
            Err(error) => {
                // The message never reached Claude's input, so sending it again is safe.
                thread
                    .pending
                    .retain(|flight| flight.message_id != message_id);
                if thread.pending.is_empty() {
                    thread.deadline_epoch += 1;
                }
                let error = format!("Claude was not sent the message: {error}");
                let _ = self
                    .write(&thread_id, move |write| {
                        mark_refused(write, message_id, error)
                    })
                    .await;
                self.settle_idle_stops(&thread_id);
            }
        }
    }

    /// Adds a queued message to the turn that runs instead of waiting for it to end.
    pub(super) async fn steer(
        &mut self,
        thread_id: &ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        self.running()?;
        self.load(thread_id).await?;
        let thread = &self.threads[thread_id];
        if thread.pending.is_empty() || thread.stopping || thread.open().is_none() {
            return Err(ProviderError::Conflict(
                "Only a running turn can be steered".into(),
            ));
        }
        let message = self.message(thread_id, &message_id).await?;
        if message.status != MessageStatus::Queued {
            return Err(ProviderError::Conflict(format!(
                "Only a queued message can steer the turn; message {message_id} is not queued"
            )));
        }
        if message.origin.limit().is_some() {
            return Err(ProviderError::Conflict(
                "A message from another thread runs in a turn of its own, with no more than that thread's permission".into(),
            ));
        }
        let submitting = self
            .set_message_status(thread_id, &message_id, MessageStatus::Submitting)
            .await?;
        self.deliver(thread_id, &message);
        Ok(submitting)
    }

    pub(super) async fn set_message_status(
        &mut self,
        thread_id: &ThreadId,
        message_id: &MessageId,
        status: MessageStatus,
    ) -> Result<Message, ProviderError> {
        let message_id = message_id.clone();
        self.write(thread_id, move |write| {
            write.set_message_status(&message_id, status)
        })
        .await
    }

    /// Claude took the message with this uuid: the turn it joins is the one running, or starts.
    pub(super) async fn on_replay(&mut self, thread_id: &ThreadId, uuid: &str) {
        let Some(flight) = self
            .threads
            .get(thread_id)
            .and_then(|thread| thread.pending.iter().find(|flight| flight.native == uuid))
        else {
            return;
        };
        let message_id = flight.message_id.clone();
        let _ = self.record_run(thread_id, vec![message_id], None).await;
    }

    /// Claude ended the turn it ran for the messages the result names.
    pub(super) async fn on_result(&mut self, thread_id: &ThreadId, result: TurnResult) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        let ended: Vec<MessageId> = thread
            .pending
            .iter()
            .filter(|flight| result.message_ids.contains(&flight.native))
            .map(|flight| flight.message_id.clone())
            .collect();
        if ended.is_empty() {
            return;
        }
        if let Some(open) = thread.open_mut() {
            open.worked = true;
        }
        thread.restarts.reset();
        self.close_requests(thread_id, None, "Claude's turn ended")
            .await;
        let thread = self.threads.get_mut(thread_id).expect("found above");
        let rejection = thread.rejection.take();
        let end = TurnEnd::of(
            &result,
            rejection.as_ref(),
            thread.answering_model.as_deref(),
            thread.stopping,
        );
        let (context, stop) = (end.context.clone(), end.stop.clone());
        let Ok(turn_id) = self.record_run(thread_id, ended.clone(), Some(end)).await else {
            return;
        };
        if let Some(usage) = context {
            let turn_id = turn_id.clone();
            self.hub
                .publish(thread_id, LiveUpdate::Context { turn_id, usage });
        }
        if matches!(stop, Stop::UsageLimit { .. }) {
            self.report_limit_stop(thread_id, turn_id);
        }
        let thread = self.threads.get_mut(thread_id).expect("found above");
        thread
            .pending
            .retain(|flight| !ended.contains(&flight.message_id));
        thread.run = None;
        thread.command_grants.clear();
        thread.stopping = false;
        thread.stop_epoch += 1;
        for waiter in std::mem::take(&mut thread.stop_waiters) {
            let _ = waiter.send(Ok(()));
        }
        if thread.pending.is_empty() {
            thread.deadline_epoch += 1;
        } else {
            self.arm_deadline(thread_id);
        }
        self.dispatch(thread_id).await;
    }

    /// Records that Claude took `accept` into the turn that runs, starting one when none does, and
    /// ends the turn with `end` when it has an outcome. The first write for a thread also records
    /// the session and the plan that pays for it (D24). One write, so the log never shows a message
    /// accepted by a turn it has not started.
    pub(super) async fn record_run(
        &mut self,
        thread_id: &ThreadId,
        accept: Vec<MessageId>,
        end: Option<TurnEnd>,
    ) -> Result<TurnId, ProviderError> {
        let thread = &self.threads[thread_id];
        let (turn_id, started) = match &thread.run {
            Some(turn_id) => (turn_id.clone(), true),
            None => (wire::turn_of(&accept[0]), false),
        };
        let session = match (thread.recorded, thread.session_id, thread.open()) {
            (false, Some(id), Some(open)) => Some((id.to_string(), open.plan.clone())),
            _ => None,
        };
        let ends = end.is_some();
        let changes = if ends {
            std::mem::take(
                &mut self
                    .threads
                    .get_mut(thread_id)
                    .expect("found above")
                    .changes,
            )
            .finish()
        } else {
            crate::providers::changes::ChangesRecorder::default().finish()
        };
        let recorded_turn = turn_id.clone();
        self.write(thread_id, move |write| {
            if let Some((native_id, plan)) = session {
                write.begin_provider_session(Provider::Claude, &native_id, plan)?;
            }
            accept_submitted(write, &accept, &recorded_turn)?;
            if !started {
                write.start_turn(recorded_turn.clone())?;
            }
            end.map_or(Ok(()), |end| end_turn(write, recorded_turn, end, changes))
        })
        .await?;
        let thread = self.threads.get_mut(thread_id).expect("found above");
        thread.recorded = true;
        thread.run = (!ends).then(|| turn_id.clone());
        Ok(turn_id)
    }

    /// Answers every stop that waited for work, when no work is left for it to stop.
    pub(super) fn settle_idle_stops(&mut self, thread_id: &ThreadId) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        if thread.is_busy() {
            return;
        }
        for waiter in std::mem::take(&mut thread.stop_waiters) {
            let _ = waiter.send(Ok(()));
        }
    }

    pub(super) async fn stop(&mut self, thread_id: &ThreadId, reply: Reply<()>) {
        if let Err(error) = self.load(thread_id).await {
            let _ = reply.send(Err(error));
            return;
        }
        let thread = self.threads.get_mut(thread_id).expect("loaded above");
        if !thread.is_busy() {
            let _ = reply.send(Ok(()));
        } else if thread.stopping || thread.pending.is_empty() || thread.open().is_none() {
            // Stopped when the interrupt is answered, or once the message is written.
            thread.stop_waiters.push(reply);
        } else {
            self.interrupt(thread_id, vec![reply], false);
        }
    }

    /// Asks Claude to stop the turn. The turn ends with its result, which settles every waiting
    /// stop. A CLI that neither stops nor answers is closed after the stop timeout.
    pub(super) fn interrupt(
        &mut self,
        thread_id: &ThreadId,
        waiters: Vec<Reply<()>>,
        by_deadline: bool,
    ) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        thread.stop_waiters.extend(waiters);
        let Some(open) = thread.open() else {
            for waiter in std::mem::take(&mut thread.stop_waiters) {
                let _ = waiter.send(Err(ProviderError::Unavailable(
                    "Claude is not running".into(),
                )));
            }
            return;
        };
        let claude = open.claude.clone();
        thread.stopping = true;
        thread.cancel_commands(None);
        thread.stop_epoch += 1;
        let (epoch, stop_epoch) = (thread.epoch, thread.stop_epoch);
        let (timeout, control_timeout) =
            (self.settings.stop_timeout, self.settings.control_timeout);
        let thread_id = thread_id.clone();
        let expired = thread_id.clone();
        self.spawn(async move {
            let result = claude
                .control_within(Control::Interrupt, control_timeout)
                .await
                .map(|_| ());
            Done::Interrupted {
                thread_id,
                epoch,
                stop_epoch,
                by_deadline,
                result,
            }
        });
        self.spawn(async move {
            tokio::time::sleep(timeout).await;
            Done::StopExpired {
                thread_id: expired,
                stop_epoch,
            }
        });
    }

    pub(super) async fn on_interrupted(
        &mut self,
        thread_id: ThreadId,
        epoch: u64,
        stop_epoch: u64,
        by_deadline: bool,
        result: Result<(), kratos_claude_client::RequestError>,
    ) {
        let Some(thread) = self.threads.get_mut(&thread_id) else {
            return;
        };
        if thread.epoch != epoch || thread.stop_epoch != stop_epoch || !thread.stopping {
            return;
        }
        match result {
            Ok(()) => {
                for waiter in std::mem::take(&mut thread.stop_waiters) {
                    let _ = waiter.send(Ok(()));
                }
            }
            // The CLI is ending, and its exit answers every waiting stop.
            Err(
                kratos_claude_client::RequestError::Exited(_)
                | kratos_claude_client::RequestError::Closing,
            ) => {}
            Err(error) if by_deadline => {
                let reason = format!("{STOP_FAILED} ({error})");
                self.close_session(&thread_id, &reason);
            }
            Err(error) => {
                // Another stop may try again, and the turn's deadline runs again.
                thread.stopping = false;
                thread.stop_epoch += 1;
                for waiter in std::mem::take(&mut thread.stop_waiters) {
                    let _ = waiter.send(Err(ProviderError::Unavailable(format!(
                        "Claude did not stop the turn: {error}"
                    ))));
                }
                self.arm_deadline(&thread_id);
            }
        }
    }

    /// The turn was asked to stop and still runs, so Claude is closed.
    pub(super) fn on_stop_expired(&mut self, thread_id: &ThreadId, stop_epoch: u64) {
        let expired = self
            .threads
            .get(thread_id)
            .is_some_and(|thread| thread.stopping && thread.stop_epoch == stop_epoch);
        if expired {
            self.close_session(thread_id, STOP_FAILED);
        }
    }

    pub(super) fn arm_deadline(&mut self, thread_id: &ThreadId) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        thread.deadline_epoch += 1;
        let (deadline_epoch, thread_id) = (thread.deadline_epoch, thread_id.clone());
        let timeout = self.settings.turn_timeout;
        self.spawn(async move {
            tokio::time::sleep(timeout).await;
            Done::Deadline {
                thread_id,
                deadline_epoch,
            }
        });
    }

    /// The turn ran out of time: the queue behind it pauses, and Claude is asked to stop it.
    pub(super) async fn on_deadline(&mut self, thread_id: ThreadId, deadline_epoch: u64) {
        let Some(thread) = self.threads.get(&thread_id) else {
            return;
        };
        if thread.deadline_epoch != deadline_epoch || thread.stopping {
            return;
        }
        if self.requests.blocks(&thread_id) {
            self.arm_deadline(&thread_id);
            return;
        }
        let Some(turn_id) = thread.run.clone().or_else(|| {
            thread
                .pending
                .first()
                .map(|flight| wire::turn_of(&flight.message_id))
        }) else {
            return;
        };
        let paused = self
            .write(&thread_id, move |write| {
                write.pause(PauseReason::TimedOut { turn_id })
            })
            .await;
        if paused.is_ok() {
            self.interrupt(&thread_id, Vec::new(), true);
        }
    }
}

/// Accepts each of `messages` that is still being sent into `turn_id`, in the caller's write.
fn accept_submitted(
    write: &mut ThreadWrite<'_>,
    messages: &[MessageId],
    turn_id: &TurnId,
) -> Result<(), StoreError> {
    let submitting: Vec<MessageId> = write
        .pending_messages()?
        .into_iter()
        .filter(|message| {
            message.status == MessageStatus::Submitting && messages.contains(&message.id)
        })
        .map(|message| message.id)
        .collect();
    for message_id in submitting {
        let accepted = MessageStatus::Accepted {
            turn_id: turn_id.clone(),
        };
        write.set_message_status(&message_id, accepted)?;
    }
    Ok(())
}

/// The thread's settings for a message: no more access than the thread that sent it has.
fn effective(
    thread: &super::threads::ThreadState,
    message: &Message,
) -> kratos_protocol::ThreadSettings {
    match message.origin.limit() {
        Some(limit) => thread.settings.limited_to(limit),
        None => thread.settings.clone(),
    }
}
