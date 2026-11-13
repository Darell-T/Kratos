//! What a CLI sends: messages become thread events and live updates, and controls reach Kratos
//! permission, question, hook, and MCP handlers.

use std::time::Instant;

use kratos_claude_client::{Assistant, Incoming, IterationTokens, StreamEvent, StreamMessage};
use kratos_protocol::{LiveUpdate, ThreadEventBody, ThreadId, TurnId};

use super::actor::{Actor, SessionEvent};
use super::threads::ThreadState;
use super::{ends, wire};

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
        if let Some(open) = self
            .threads
            .get_mut(&thread_id)
            .and_then(ThreadState::open_mut)
        {
            open.last_active = Instant::now();
        }
        match incoming {
            Incoming::Message(message) => self.on_message(&thread_id, message).await,
            Incoming::Control(control) => self.on_control(&thread_id, control).await,
            Incoming::Cancelled { id } => {
                self.close_requests(&thread_id, Some(&id), "Claude no longer needs an answer")
                    .await
            }
            Incoming::Oversized { bytes } => {
                let message =
                    format!("Claude sent a line of {bytes} bytes, more than Kratos reads");
                self.provider_error(&thread_id, message, false).await;
            }
            Incoming::Unreadable(error) => {
                self.provider_error(&thread_id, error.to_string(), false)
                    .await;
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
            StreamMessage::RateLimit(event) => self.on_rate_limit(thread_id, event).await,
            // The CLI retries by itself and Kratos sends nothing for it, but the user sees why the
            // turn is waiting.
            StreamMessage::ApiRetry(retry) => {
                let status = retry
                    .error_status
                    .map_or_else(String::new, |status| format!(", status {status}"));
                let message = format!(
                    "Claude's request failed ({}{status}). Claude tries again in {} ms, attempt {} \
                     of {}",
                    retry.error, retry.retry_delay_ms, retry.attempt, retry.max_retries
                );
                self.provider_error(thread_id, message, true).await;
            }
            StreamMessage::Unrecognized { .. } => {}
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
        if let Some(thread) = self.threads.get_mut(thread_id)
            && let Some(model) = assistant.message["model"].as_str()
        {
            thread.answering_model = Some(model.to_owned());
        }
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

    /// Shows a reply as it is written, and the context each request fills as it starts and ends
    /// (D76), to the clients following the thread.
    fn on_stream(&mut self, thread_id: &ThreadId, stream: &StreamEvent) {
        let Some((turn_id, window)) = self
            .threads
            .get(thread_id)
            .and_then(|thread| Some((thread.run.clone()?, thread.context_window)))
        else {
            return;
        };
        if let Some((item_id, delta)) = wire::reply_delta(stream) {
            self.hub.publish(
                thread_id,
                LiveUpdate::ReplyDelta {
                    turn_id: turn_id.clone(),
                    item_id,
                    delta,
                },
            );
        }
        let context = IterationTokens::of_stream_event(&stream.event)
            .and_then(|tokens| ends::context_of(tokens, window));
        if let Some(usage) = context {
            self.hub
                .publish(thread_id, LiveUpdate::Context { turn_id, usage });
        }
    }

    /// Records trouble Claude's output caused in the running turn, and whether Claude retries by
    /// itself. Outside a turn it only logs.
    pub(super) async fn provider_error(
        &mut self,
        thread_id: &ThreadId,
        message: String,
        will_retry: bool,
    ) {
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
                    will_retry,
                })?;
                Ok(())
            })
            .await;
    }
}
