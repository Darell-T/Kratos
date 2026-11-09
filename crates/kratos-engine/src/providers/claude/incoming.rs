//! What a CLI sends: messages become thread events and live updates, and requests are refused
//! until Kratos's permission rules exist.

use kratos_claude_client::{Assistant, Incoming, StreamEvent, StreamMessage};
use kratos_protocol::{LiveUpdate, ThreadEventBody, ThreadId, TurnId};

use super::actor::{Actor, SessionEvent};
use super::{permissions, wire};

impl Actor {
    pub(super) async fn on_event(&mut self, event: SessionEvent) {
        let SessionEvent {
            thread_id,
            epoch,
            incoming,
        } = event;
        let current = self
            .threads
            .get(&thread_id)
            .is_some_and(|thread| thread.epoch == epoch && thread.open().is_some());
        if !current {
            // A request dropped here is answered with an error, so the old CLI is not left waiting.
            return;
        }
        match incoming {
            Incoming::Message(message) => self.on_message(&thread_id, message).await,
            Incoming::Control(control) => permissions::answer(control),
            Incoming::Oversized { bytes } => {
                let message =
                    format!("Claude sent a line of {bytes} bytes, more than Kratos reads");
                self.provider_error(&thread_id, message).await;
            }
            Incoming::Unreadable(error) => {
                self.provider_error(&thread_id, error.to_string()).await;
            }
            Incoming::Exited(exit) => self.on_exit(&thread_id, exit).await,
        }
    }

    async fn on_message(&mut self, thread_id: &ThreadId, message: StreamMessage) {
        match message {
            StreamMessage::Init(_) => self.mark_started(thread_id),
            StreamMessage::Replay { uuid } => {
                self.mark_started(thread_id);
                self.on_replay(thread_id, &uuid).await;
            }
            StreamMessage::Assistant(assistant) => self.on_assistant(thread_id, &assistant).await,
            StreamMessage::StreamEvent(stream) => self.on_stream(thread_id, &stream),
            StreamMessage::Result(result) => self.on_result(thread_id, result).await,
            StreamMessage::RateLimit(_) | StreamMessage::Unrecognized { .. } => {}
        }
    }

    /// Claude has begun a turn, so a message it was sent is no longer certain to be unseen.
    fn mark_started(&mut self, thread_id: &ThreadId) {
        if let Some(open) = self
            .threads
            .get_mut(thread_id)
            .and_then(|thread| thread.open_mut())
        {
            open.started = true;
        }
    }

    /// Saves each reply and tool call of a completed assistant message (D88).
    async fn on_assistant(&mut self, thread_id: &ThreadId, assistant: &Assistant) {
        let items = wire::assistant_items(assistant);
        let Some(head) = self.threads.get(thread_id).and_then(|thread| {
            thread
                .pending
                .first()
                .map(|flight| flight.message_id.clone())
        }) else {
            return;
        };
        if items.is_empty() {
            return;
        }
        let Ok(turn_id) = self.record_run(thread_id, vec![head], None).await else {
            return;
        };
        let _ = self
            .write(thread_id, move |write| {
                for item in items {
                    write.append(ThreadEventBody::ItemCompleted {
                        turn_id: turn_id.clone(),
                        item,
                    })?;
                }
                Ok(())
            })
            .await;
    }

    /// Shows a reply as it is written, to the clients following the thread.
    fn on_stream(&mut self, thread_id: &ThreadId, stream: &StreamEvent) {
        let Some(turn_id) = self
            .threads
            .get(thread_id)
            .and_then(|thread| thread.run.clone())
        else {
            return;
        };
        if let Some((item_id, delta)) = wire::reply_delta(stream) {
            self.hub.publish(
                thread_id,
                LiveUpdate::ReplyDelta {
                    turn_id,
                    item_id,
                    delta,
                },
            );
        }
    }

    /// Records trouble Claude's output caused in the running turn. Outside a turn it only logs.
    async fn provider_error(&mut self, thread_id: &ThreadId, message: String) {
        let run = self
            .threads
            .get(thread_id)
            .and_then(|thread| thread.run.clone());
        let Some(turn_id): Option<TurnId> = run else {
            eprintln!("claude: {message}");
            return;
        };
        let _ = self
            .write(thread_id, move |write| {
                write.append(ThreadEventBody::ProviderError {
                    turn_id,
                    message,
                    will_retry: false,
                })?;
                Ok(())
            })
            .await;
    }
}
