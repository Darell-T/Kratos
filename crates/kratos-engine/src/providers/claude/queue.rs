//! A thread's queue of messages: sending, removing, retrying, and discarding them, resuming a
//! paused queue, and settling an uncertain message from Claude's own transcript (D11, D85). What
//! holds or pauses the queue is written inside the write of the step that needs it.

use std::path::Path;

use kratos_claude_client::{DeliveredMessages, transcript_path};
use kratos_protocol::{
    ContinuationId, Message, MessageId, MessageInput, MessageOrigin, MessageStatus, PauseReason,
    ThreadId,
};
use kratos_store::{Enqueued, QueuePlace};

use super::actor::{Actor, Done, Reply, TranscriptWork};
use super::{MAX_PENDING_MESSAGES, ProviderError, Sent, wire};
use crate::providers::Continued;
use crate::providers::queue::{
    continuation_write, pause_if_waiting, resume_queue, resume_when_settled,
};

impl Actor {
    pub(super) async fn send(
        &mut self,
        thread_id: &ThreadId,
        message_id: MessageId,
        input: Vec<MessageInput>,
        origin: MessageOrigin,
    ) -> Result<Sent, ProviderError> {
        self.running()?;
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
        self.running()?;
        self.load(thread_id).await?;
        let message = self.message(thread_id, &message_id).await?;
        if !matches!(message.status, MessageStatus::Failed { .. }) {
            return Err(ProviderError::Conflict(
                "Only a message Claude never received can be sent again".into(),
            ));
        }
        self.write(thread_id, move |write| {
            let queued = write.set_message_status(&message_id, MessageStatus::Queued)?;
            resume_when_settled(write)?;
            Ok(queued)
        })
        .await
    }

    /// Queues the continuation's message and lets the queue run. See [`continuation_write`].
    pub(super) async fn continue_thread(
        &mut self,
        thread_id: &ThreadId,
        continuation_id: ContinuationId,
    ) -> Result<Continued, ProviderError> {
        self.running()?;
        self.load(thread_id).await?;
        self.write(thread_id, continuation_write(continuation_id))
            .await
    }

    /// Lets a paused queue run again, which also lets a CLI that kept failing start once more.
    pub(super) async fn resume(&mut self, thread_id: &ThreadId) -> Result<(), ProviderError> {
        self.running()?;
        self.load(thread_id).await?;
        let resumed = self.write(thread_id, resume_queue).await?;
        resumed.map_err(ProviderError::Conflict)?;
        if let Some(thread) = self.threads.get_mut(thread_id) {
            thread.restarts.reset();
        }
        Ok(())
    }

    /// Settles an uncertain message from Claude's transcript, which is complete once the thread
    /// is idle.
    pub(super) async fn discard(
        &mut self,
        thread_id: ThreadId,
        message_id: MessageId,
        reply: Reply<Message>,
    ) {
        let path = match self.discard_ready(&thread_id, &message_id).await {
            Ok(path) => path,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        self.spawn(async move {
            let result = match path {
                Some(path) => kratos_claude_client::delivered_message_ids(&path).await,
                None => Ok(DeliveredMessages::default()),
            };
            Done::Transcript {
                thread_id,
                work: TranscriptWork::Discard { message_id, reply },
                result,
            }
        });
    }

    async fn discard_ready(
        &mut self,
        thread_id: &ThreadId,
        message_id: &MessageId,
    ) -> Result<Option<std::path::PathBuf>, ProviderError> {
        self.load(thread_id).await?;
        let message = self.message(thread_id, message_id).await?;
        if !matches!(message.status, MessageStatus::Unknown { .. }) {
            return Err(ProviderError::Conflict(
                "Only a message whose delivery is uncertain can be discarded".into(),
            ));
        }
        let thread = &self.threads[thread_id];
        if thread.is_busy() {
            return Err(ProviderError::Conflict(
                "Stop the turn before discarding a message".into(),
            ));
        }
        Ok(self.transcript_of(thread_id))
    }

    /// Where Claude keeps the thread's history, once the thread has a session. The file may not exist:
    /// Claude writes it when it takes the first message.
    pub(super) fn transcript_of(&self, thread_id: &ThreadId) -> Option<std::path::PathBuf> {
        let thread = self.threads.get(thread_id)?;
        let id = thread.session_id?;
        Some(transcript_path(
            &self.settings.claude_home,
            Path::new(&thread.folder),
            id,
        ))
    }

    /// Records the transcript's verdict on a discarded message: accepted when Claude's history
    /// holds it, cancelled when it does not. Never sends it again (D11).
    pub(super) async fn finish_discard(
        &mut self,
        thread_id: ThreadId,
        message_id: MessageId,
        reply: Reply<Message>,
        result: Result<DeliveredMessages, kratos_claude_client::TranscriptError>,
    ) {
        let delivered = match result {
            Ok(delivered) => delivered,
            Err(error) => {
                let _ = reply.send(Err(ProviderError::Unavailable(format!(
                    "Kratos could not read Claude's history: {error}"
                ))));
                return;
            }
        };
        if self
            .threads
            .get(&thread_id)
            .is_some_and(|thread| thread.is_busy())
        {
            let _ = reply.send(Err(ProviderError::Conflict(
                "Claude must be idle before a message is discarded".into(),
            )));
            return;
        }
        let found = delivered
            .ids
            .contains(&wire::native_id(&message_id).to_string());
        let settled = self
            .write(&thread_id, move |write| {
                // Discarding prevents a replay; it cannot undo work Claude may have done.
                let status = if found {
                    MessageStatus::Accepted {
                        turn_id: wire::turn_of(&message_id),
                    }
                } else {
                    MessageStatus::Cancelled
                };
                let settled = write.set_message_status(&message_id, status)?;
                resume_when_settled(write)?;
                Ok(settled)
            })
            .await;
        let _ = reply.send(settled);
        self.dispatch(&thread_id).await;
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

    /// Pauses waiting work while keeping any existing pause.
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
