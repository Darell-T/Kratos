//! `wait_for_turn`'s wait: from a thread's details, then its events, until the thread needs
//! nothing from the waiting agent's turn to go on.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use kratos_api_client::ClientError;
use kratos_protocol::{
    AgentItem, Approval, PauseReason, Question, Seq, ThreadDetails, ThreadEventBody, ThreadId,
    TurnOutcome,
};
use serde::Serialize;

use super::Call;

/// How often a wait checks whether Codex cancelled the call.
const CANCEL_CHECK: Duration = Duration::from_secs(1);

#[derive(Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WaitOutcome {
    Idle,
    WaitingForInput {
        approvals: Vec<Approval>,
        questions: Vec<Question>,
    },
    Paused {
        reason: PauseReason,
    },
    TurnEnded {
        outcome: TurnOutcome,
        reply: Option<String>,
    },
    TimedOut {
        last_seq: Seq,
    },
}

/// Waits from `details` until the thread settles, Codex cancels the call, or `timeout` passes.
pub fn wait(
    call: &Call,
    thread_id: ThreadId,
    details: ThreadDetails,
    timeout: Duration,
) -> Result<WaitOutcome, String> {
    let mut last_seq = details.last_seq;
    if let Some(outcome) = settled(details) {
        return Ok(outcome);
    }
    let deadline = Instant::now() + timeout;
    let mut follower = call.caller.client.follow(thread_id, last_seq);
    let mut reply = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if call.cancelled.load(Ordering::Relaxed) {
            return Err("Cancelled".to_owned());
        }
        let event = match follower.next_event(left.min(CANCEL_CHECK)) {
            Ok(event) => event,
            Err(ClientError::TimedOut(_)) => continue,
            Err(error) => return Err(format!("Kratos is unreachable: {error}")),
        };
        last_seq = event.seq;
        match event.event {
            ThreadEventBody::ItemCompleted {
                item: AgentItem::Message { text, .. },
                ..
            } => reply = Some(text),
            ThreadEventBody::TurnCompleted { outcome, .. } => {
                return Ok(WaitOutcome::TurnEnded { outcome, reply });
            }
            ThreadEventBody::ApprovalRequested { approval } => {
                return Ok(WaitOutcome::WaitingForInput {
                    approvals: vec![approval],
                    questions: Vec::new(),
                });
            }
            ThreadEventBody::QuestionAsked { question } => {
                return Ok(WaitOutcome::WaitingForInput {
                    approvals: Vec::new(),
                    questions: vec![question],
                });
            }
            ThreadEventBody::ThreadPaused { reason } => return Ok(WaitOutcome::Paused { reason }),
            ThreadEventBody::UserMessage { .. }
            | ThreadEventBody::MessageQueued { .. }
            | ThreadEventBody::MessageStatusChanged { .. }
            | ThreadEventBody::TurnStarted { .. }
            | ThreadEventBody::ItemCompleted { .. }
            | ThreadEventBody::ApprovalDecided { .. }
            | ThreadEventBody::ApprovalClosed { .. }
            | ThreadEventBody::QuestionAnswered { .. }
            | ThreadEventBody::QuestionClosed { .. }
            | ThreadEventBody::ThreadResumed
            | ThreadEventBody::SettingsChanged { .. }
            | ThreadEventBody::ProviderError { .. }
            | ThreadEventBody::ContinuationChanged { .. }
            | ThreadEventBody::ProviderSession { .. } => {}
        }
    }
    Ok(WaitOutcome::TimedOut { last_seq })
}

fn settled(details: ThreadDetails) -> Option<WaitOutcome> {
    if !details.approvals.is_empty() || !details.questions.is_empty() {
        return Some(WaitOutcome::WaitingForInput {
            approvals: details.approvals,
            questions: details.questions,
        });
    }
    if let Some(reason) = details.paused {
        return Some(WaitOutcome::Paused { reason });
    }
    (details.running_turn.is_none() && details.queue.is_empty()).then_some(WaitOutcome::Idle)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use axum::extract::WebSocketUpgrade;
    use axum::extract::ws::Message;
    use axum::{Router, routing};
    use kratos_api_client::Client;
    use serde_json::{Value, json};

    use super::*;
    use crate::mcp::Caller;

    fn busy() -> ThreadDetails {
        serde_json::from_value(json!({
            "id": "target", "workspaceId": "workspace", "title": "work", "provider": "codex",
            "createdAt": "2026-10-30T12:00:00.000Z", "updatedAt": "2026-10-30T12:00:00.000Z",
            "settings": { "model": null, "permission": "askMe", "planMode": false,
                          "contextLimitTokens": null },
            "runningTurn": "turn", "paused": null, "queue": [], "approvals": [],
            "questions": [], "lastSeq": 10
        }))
        .unwrap()
    }

    async fn after_events(events: Vec<Value>) -> Value {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let app = Router::new().route(
            "/api/events",
            routing::get(move |upgrade: WebSocketUpgrade| async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    let Some(Ok(Message::Text(subscription))) = socket.recv().await else {
                        panic!("the follower must subscribe before reading events");
                    };
                    assert_eq!(
                        serde_json::from_str::<Value>(&subscription).unwrap(),
                        json!({ "type": "subscribe", "threadId": "target", "after": 10 })
                    );
                    for (index, event) in events.into_iter().enumerate() {
                        let message = json!({ "type": "event", "event": {
                            "threadId": "target", "seq": index + 11,
                            "at": "2026-10-30T12:00:00.000Z", "event": event
                        }});
                        socket
                            .send(Message::Text(message.to_string().into()))
                            .await
                            .unwrap();
                    }
                })
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let outcome = tokio::task::spawn_blocking(move || {
            let caller = Caller::new(client, ThreadId::new("caller".to_owned())).unwrap();
            let cancelled = AtomicBool::new(false);
            let call = Call {
                caller: &caller,
                cancelled: &cancelled,
            };
            let details = busy();
            serde_json::to_value(
                wait(
                    &call,
                    details.thread.id.clone(),
                    details,
                    Duration::from_secs(5),
                )
                .unwrap(),
            )
            .unwrap()
        })
        .await
        .unwrap();
        server.abort();
        outcome
    }

    #[tokio::test]
    async fn a_wait_keeps_the_latest_reply_across_unrelated_events_until_the_turn_ends() {
        let outcome = after_events(vec![
            json!({ "kind": "item.completed", "turnId": "turn",
                    "item": { "type": "message", "id": "first", "text": "Earlier." } }),
            json!({ "kind": "item.completed", "turnId": "turn",
                    "item": { "type": "message", "id": "last", "text": "Latest. 日本語" } }),
            json!({ "kind": "user.message", "text": "An unrelated message" }),
            json!({ "kind": "turn.completed", "turnId": "turn", "outcome": "failed",
                    "error": "provider stopped", "context": null,
                    "changes": { "files": [], "editedSinceCommand": false, "ranCommand": false } }),
        ])
        .await;

        assert_eq!(
            outcome,
            json!({ "state": "turnEnded", "reply": "Latest. 日本語",
                                   "outcome": { "outcome": "failed", "error": "provider stopped" } })
        );
    }

    #[tokio::test]
    async fn a_wait_ends_for_a_question_or_pause_before_the_turn_completes() {
        let question = json!({ "id": "question", "turnId": "turn", "items": [], "blocking": true });
        assert_eq!(
            after_events(vec![
                json!({ "kind": "question.asked", "question": question })
            ])
            .await,
            json!({ "state": "waitingForInput", "approvals": [], "questions": [question] })
        );
        assert_eq!(
            after_events(vec![
                json!({ "kind": "thread.paused", "reason": "interrupted" })
            ])
            .await,
            json!({ "state": "paused", "reason": { "reason": "interrupted" } })
        );
    }
}
