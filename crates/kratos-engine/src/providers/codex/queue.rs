//! A thread's queue of messages: sending, removing, retrying, and discarding them, settling
//! uncertain ones from Codex's history, resuming a paused queue, and continuing after a usage limit
//! (D11, D39, D85). What holds or pauses the queue is written inside the write of the step that
//! needs it.

use kratos_codex_client::{RequestError, types};
use kratos_protocol::{
    ContinuationId, Message, MessageId, MessageInput, MessageOrigin, MessageStatus, PauseReason,
    RequiredText, ThreadId,
};
use kratos_store::{Enqueued, QueuePlace, StoreError, ThreadWrite};
use uuid::Uuid;

use super::actor::{Actor, Command, Done, Reply};
use super::threads::{Link, Turn};
use super::turns::read_history;
use super::{Continued, MAX_PENDING_MESSAGES, ProviderError, Sent, wire};
use crate::providers::queue::{
    held_message, pause_if_waiting, resume_queue, resume_when_settled, settling_action,
};

/// The text of the message Kratos sends to carry on a turn that stopped at a usage limit (D39).
const CONTINUATION_TEXT: &str = "Continue the task you were working on. You stopped earlier because the usage limit was reached.";

impl Actor {
    pub(super) async fn send(
        &mut self,
        thread_id: &ThreadId,
        message_id: MessageId,
        input: Vec<MessageInput>,
        origin: MessageOrigin,
    ) -> Result<Sent, ProviderError> {
        self.load(thread_id).await?;
        let enqueued = self
            .write(thread_id, move |write| {
                write.enqueue_message(
                    &message_id,
                    input,
                    origin,
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

    pub(super) async fn remove(
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

    pub(super) async fn retry(
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
    pub(super) async fn discard(
        &mut self,
        thread_id: ThreadId,
        message_id: MessageId,
        reply: Reply<Message>,
    ) {
        let ready = self.discard_ready(&thread_id, &message_id).await;
        let native_id = match ready {
            Ok(Some(native_id)) => native_id,
            Ok(None) => {
                // This runs again once Codex has loaded the thread, or once Codex has started.
                let thread = self.threads.get_mut(&thread_id).expect("loaded above");
                if matches!(thread.link, Link::Attaching) {
                    thread.discards.push((message_id, reply));
                } else {
                    self.deferred.push(Command::Discard {
                        thread_id,
                        message_id,
                        reply,
                    });
                }
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
        if !matches!(thread.turn, Turn::Idle) || !thread.steering.is_empty() {
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

    /// Lets a paused queue run again. See [`resume_queue`].
    pub(super) async fn resume(&mut self, thread_id: &ThreadId) -> Result<(), ProviderError> {
        self.load(thread_id).await?;
        let resumed = self.write(thread_id, resume_queue).await?;
        resumed.map_err(ProviderError::Conflict)
    }

    /// Queues the continuation's message ahead of the waiting ones, marks it sent, and lets the
    /// queue run, in one write (D39). A continuation that is no longer open sends nothing. One
    /// that would run past a message waiting on the user, or past a pause the limit did not cause,
    /// or that finds the queue full (D85), goes back to the user.
    pub(super) async fn continue_thread(
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

    pub(super) async fn message(
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

    /// Pauses waiting work while keeping any existing pause and its continuation.
    pub(super) async fn pause_unavailable_if_waiting(
        &mut self,
        thread_id: &ThreadId,
        error: String,
    ) {
        let _ = self
            .write(thread_id, move |write| {
                if write.provider_thread()?.paused.is_none() {
                    pause_if_waiting(write, PauseReason::Unavailable { error })?;
                }
                Ok(())
            })
            .await;
    }
}

/// Why the thread cannot take a continuation on its own, if it cannot: a message waits on the user,
/// or the queue is paused for something the reset does not fix.
fn blocks_continuation(write: &mut ThreadWrite<'_>) -> Result<Option<String>, StoreError> {
    if let Some(held) = held_message(write)? {
        return Ok(settling_action(&held));
    }
    Ok(match write.provider_thread()?.paused {
        None | Some(PauseReason::UsageLimit { .. } | PauseReason::Unavailable { .. }) => None,
        Some(_) => Some("The thread is paused for another reason; resume it yourself".to_owned()),
    })
}
