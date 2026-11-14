//! Turns Claude Code starts by itself after a background command ends, with no message of the
//! user's in them.

use kratos_protocol::{
    AgentItem, ApprovalDecision, AskReason, ThreadEventBody, TurnOrigin, TurnOutcome,
};
use serde_json::json;

use crate::harness::{A, B, Harness, Launch, Options};
use crate::requests::{approval, response, send};

const SUMMARY: &str = "Background command \"cargo build\" completed (exit code 0)";

/// Plays what the real CLI sent (measured with 2.1.322) when a background command ended after its
/// turn: the task's notification, then a turn of its own.
async fn background_task_ends(harness: &Harness, launch: &Launch) {
    harness
        .emit(
            launch,
            json!({ "type": "system", "subtype": "task_notification", "task_id": "t1",
                    "status": "completed", "summary": SUMMARY }),
        )
        .await;
    harness
        .emit(
            launch,
            json!({ "type": "system", "subtype": "background_tasks_changed", "tasks": [] }),
        )
        .await;
    harness.init(launch).await;
}

async fn after_a_finished_turn(harness: &mut Harness) -> Launch {
    let launch = harness.begin(A, 1).await;
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    launch
}

#[tokio::test]
async fn a_turn_claude_starts_after_a_background_command_is_recorded_with_its_replies() {
    // Catches the turn the CLI starts by itself dropped: its replies lost, and its result ignored,
    // so the user never sees that Claude went on working.
    let mut harness = Harness::start(Options::default()).await;
    let launch = after_a_finished_turn(&mut harness).await;

    background_task_ends(&harness, &launch).await;
    let started = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;
    let ThreadEventBody::TurnStarted { turn_id, origin } = started else {
        unreachable!()
    };
    assert_eq!(
        origin,
        TurnOrigin::BackgroundTask {
            summary: Some(SUMMARY.to_owned())
        }
    );
    assert_ne!(turn_id.as_str(), A, "no message of the user's started it");

    harness
        .emit(
            &launch,
            json!({ "type": "assistant", "uuid": "u-own", "session_id": "s",
                    "message": { "id": "msg_own", "content": [
                        { "type": "text", "text": "The build finished cleanly." } ] } }),
        )
        .await;
    let replied = harness
        .wait_for(|event| matches!(event, ThreadEventBody::ItemCompleted { .. }))
        .await;
    assert!(
        matches!(&replied, ThreadEventBody::ItemCompleted { turn_id: replied_in, item: AgentItem::Message { text, .. } }
            if *replied_in == turn_id && text == "The build finished cleanly."),
        "{replied:?}"
    );

    harness.finish(&launch, &[], json!({})).await;
    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(
        matches!(&ended, ThreadEventBody::TurnCompleted { turn_id: ended_id, outcome: TurnOutcome::Completed, .. } if *ended_id == turn_id),
        "{ended:?}"
    );

    harness.send(B, "B").await;
    harness.wait_written(&launch, &[A, B]).await;
}

#[tokio::test]
async fn an_approval_in_a_turn_claude_started_reaches_the_user_and_the_answer_is_honored() {
    // Catches a tool request in that turn refused as "No Claude turn is running" instead of
    // reaching the user, and an answer the turn never receives.
    let mut harness = Harness::start(Options::default()).await;
    let launch = after_a_finished_turn(&mut harness).await;
    background_task_ends(&harness, &launch).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;

    send(
        &harness,
        &launch,
        "write",
        "Write",
        json!({ "file_path": "notes.txt" }),
    )
    .await;
    let asked = approval(&mut harness).await;
    assert_eq!(asked.why, AskReason::AskEveryAction);
    harness
        .provider
        .decide(asked.id, ApprovalDecision::Accept)
        .await
        .unwrap();

    assert_eq!(
        response(&harness, &launch, "write").await["behavior"],
        "allow"
    );
}
