//! What an engine that starts again finds of the work the last one left (L1): no Codex process
//! outlives the engine that started it.

use kratos_protocol::{ApprovalOutcome, MessageStatus, PauseReason, ThreadEventBody, TurnOutcome};
use serde_json::json;

use crate::harness::{Harness, Options};

const THREAD: &str = "thread-A";

/// Codex's history of thread-A after a restart: one turn that took message `a` and never finished.
fn history_with_message_a(harness: &Harness, input: serde_json::Value) -> serde_json::Value {
    json!([{
        "id": THREAD, "cwd": harness.folder(), "model": "fake-model", "ephemeral": false,
        "status": { "type": "idle" }, "cliVersion": "0", "createdAt": 0, "updatedAt": 0,
        "modelProvider": "fake", "preview": "", "projectId": null, "sessionId": THREAD,
        "source": "appServer",
        "turns": [{ "id": "turn-1", "status": "interrupted", "items": [{
            "type": "userMessage", "id": "u", "clientId": "a", "content": input,
        }] }],
    }])
}

#[tokio::test]
async fn a_turn_running_when_the_engine_stopped_ends_interrupted_and_holds_the_queue() {
    // Catches a thread that shows a turn running forever after a restart, or that runs the next
    // message after work that was cut off.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness.send("b", "second").await;
    let (root, thread_id) = harness.stop().await;

    let mut harness = Harness::start_in(root, Options::default(), Some(thread_id)).await;

    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(matches!(
        ended,
        ThreadEventBody::TurnCompleted {
            outcome: TurnOutcome::Interrupted,
            ..
        }
    ));
    let details = harness.details().await;
    assert_eq!(details.running_turn, None);
    assert_eq!(details.paused, Some(PauseReason::Interrupted));
    assert_eq!(harness.message("b").await.status, MessageStatus::Queued);

    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    let started = harness.held("turn/start", 1).await;
    assert_eq!(started[0]["params"]["clientUserMessageId"], "b");
}

#[tokio::test]
async fn a_message_being_sent_when_the_engine_stopped_is_settled_by_codex_s_history() {
    // Catches the rule D11 forbids breaking after a restart: sending again a message Codex may
    // have taken. Here the history has it, so it is accepted, and the message behind it waits
    // because the turn that took it was cut off.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    let held = harness.held("turn/start", 1).await;
    harness.send("b", "second").await;
    let history = history_with_message_a(&harness, held[0]["params"]["input"].clone());
    let (root, thread_id) = harness.stop().await;
    let state_file = root.path().join("codex-state.json");
    std::fs::write(&state_file, json!({ "threads": history }).to_string()).unwrap();

    let options = Options {
        fake_args: vec!["--state-file".into(), state_file.into()],
        ..Options::default()
    };
    let mut harness = Harness::start_in(root, options, Some(thread_id)).await;

    let accepted = harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::MessageStatusChanged {
                    status: MessageStatus::Accepted { .. },
                    ..
                }
            )
        })
        .await;
    assert!(matches!(
        accepted,
        ThreadEventBody::MessageStatusChanged { status: MessageStatus::Accepted { ref turn_id }, .. } if turn_id.as_str() == "turn-1"
    ));
    harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::ThreadPaused {
                    reason: PauseReason::Interrupted
                }
            )
        })
        .await;
    assert_eq!(harness.message("b").await.status, MessageStatus::Queued);
    assert!(
        harness.turn_requests().await.is_empty(),
        "nothing is sent again"
    );
}

#[tokio::test]
async fn a_message_codex_s_history_lacks_after_a_restart_waits_for_the_user() {
    // Catches a restart that treats a missing message as never sent and sends it again: Codex may
    // have lost it, or may still be writing it to its history.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;
    harness.send("b", "second").await;
    let (root, thread_id) = harness.stop().await;

    let mut harness = Harness::start_in(root, Options::default(), Some(thread_id)).await;

    let resumed = loop {
        let opened = harness.control("test/threadRequests", json!({})).await;
        if let Some(first) = opened.as_array().and_then(|opened| opened.first()) {
            break first.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        resumed["method"], "thread/resume",
        "Codex reads the history"
    );
    assert_eq!(resumed["params"]["threadId"], THREAD);
    harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::ThreadPaused {
                    reason: PauseReason::DeliveryUncertain { .. }
                }
            )
        })
        .await;
    assert!(matches!(
        harness.message("a").await.status,
        MessageStatus::Unknown { .. }
    ));
    assert!(harness.turn_requests().await.is_empty());

    let discarded = harness
        .provider
        .discard(harness.thread_id.clone(), crate::harness::message_id("a"))
        .await
        .unwrap();
    assert_eq!(discarded.status, MessageStatus::Cancelled);
    let started = harness.held("turn/start", 1).await;
    assert_eq!(started[0]["params"]["clientUserMessageId"], "b");
}

#[tokio::test]
async fn approvals_waiting_when_the_engine_stopped_close() {
    // Catches an approval left open for a Codex process that is gone, which the user could still
    // accept with nothing to receive the answer.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness
        .ask(
            "ask-1",
            "item/commandExecution/requestApproval",
            json!({ "threadId": THREAD, "turnId": "turn-1", "itemId": "item-1", "startedAtMs": 0,
                    "command": "ls" }),
        )
        .await;
    let requested = harness
        .wait_for(|event| matches!(event, ThreadEventBody::ApprovalRequested { .. }))
        .await;
    let ThreadEventBody::ApprovalRequested { approval } = requested else {
        unreachable!("wait_for picked an approval request");
    };
    let (root, thread_id) = harness.stop().await;

    let mut harness = Harness::start_in(root, Options::default(), Some(thread_id)).await;

    harness
        .wait_for(|event| matches!(event, ThreadEventBody::ApprovalClosed { .. }))
        .await;
    assert!(harness.details().await.approvals.is_empty());
    let late = harness
        .provider
        .decide(approval.id, kratos_protocol::ApprovalDecision::Accept)
        .await
        .unwrap();
    assert!(matches!(late, ApprovalOutcome::Closed { .. }), "{late:?}");
}
