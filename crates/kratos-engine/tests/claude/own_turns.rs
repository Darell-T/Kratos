//! Turns Claude Code starts by itself after a background command ends, with no message of the
//! user's in them.

use std::time::Duration;

use kratos_protocol::{
    AgentItem, ApprovalDecision, AskReason, LiveUpdate, RequiredText, ThreadEventBody,
    ThreadSettings, TurnOrigin, TurnOutcome,
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

#[tokio::test]
async fn a_turn_claude_starts_itself_runs_under_the_thread_s_current_permission() {
    // Catches the self-started turn deciding tools under the permission of the last turn a message
    // started: lowered from Full access to Ask me in between, its command ran unasked.
    let mut harness = Harness::start(Options {
        settings: kratos_protocol::ThreadSettings {
            permission: kratos_protocol::Permission::FullAccess,
            ..kratos_protocol::ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    let launch = after_a_finished_turn(&mut harness).await;
    harness
        .provider
        .set_settings(
            harness.thread_id.clone(),
            kratos_protocol::ThreadSettings::default(),
        )
        .await
        .unwrap();
    assert!(
        harness.is_running(&launch),
        "a permission change keeps the CLI"
    );

    background_task_ends(&harness, &launch).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;
    send(
        &harness,
        &launch,
        "command",
        "Bash",
        json!({ "command": "ls" }),
    )
    .await;

    let asked = approval(&mut harness).await;
    assert_eq!(asked.why, AskReason::AskEveryAction);
    harness
        .provider
        .decide(asked.id, ApprovalDecision::Decline)
        .await
        .unwrap();
    assert_eq!(
        response(&harness, &launch, "command").await["behavior"],
        "deny"
    );
}

/// Finishes the thread's first turn with a background command still running.
async fn a_command_runs_on_after_the_turn(harness: &mut Harness) -> Launch {
    let launch = harness.begin(A, 1).await;
    harness
        .emit(
            &launch,
            json!({ "type": "system", "subtype": "background_tasks_changed",
                    "tasks": [{ "task_id": "t1", "task_type": "local_bash" }] }),
        )
        .await;
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    launch
}

async fn change_model(harness: &Harness, model: &str) {
    let settings = ThreadSettings {
        model: RequiredText::new(model),
        ..ThreadSettings::default()
    };
    harness
        .provider
        .set_settings(harness.thread_id.clone(), settings)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_model_change_waits_for_a_background_command_and_the_turn_that_reports_it() {
    // Catches a settings change closing a CLI whose background command still runs, a message
    // that needs the new launch cutting the command or the turn that reports it short, and a
    // thread not told why its message waits.
    let mut harness = Harness::start(Options::default()).await;
    let launch = a_command_runs_on_after_the_turn(&mut harness).await;

    change_model(&harness, "claude-opus-test").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        harness.is_running(&launch),
        "the CLI and its command run on"
    );

    let (_, mut live) = harness.hub.watch_live(&harness.thread_id);
    harness.send(B, "B").await;
    let note = tokio::time::timeout(crate::harness::WAIT, async {
        loop {
            if let LiveUpdate::Waiting { reason } = live.recv().await.unwrap() {
                return reason;
            }
        }
    })
    .await
    .expect("the thread is told why its message waits");
    assert!(note.contains("background command"), "{note}");
    assert_eq!(harness.launches().len(), 1, "no new launch yet");

    background_task_ends(&harness, &launch).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        harness.is_running(&launch),
        "the turn that reports the command runs to its end"
    );
    harness.finish(&launch, &[], json!({})).await;
    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(
        matches!(
            &ended,
            ThreadEventBody::TurnCompleted {
                outcome: TurnOutcome::Completed,
                ..
            }
        ),
        "{ended:?}"
    );

    let second = harness.launch(2).await;
    harness.wait_ended(&launch).await;
    assert_eq!(second.value("--model"), Some("claude-opus-test"));
    harness.wait_written(&second, &[B]).await;
}

#[tokio::test]
async fn a_message_waiting_for_a_background_command_goes_ahead_when_no_report_turn_follows() {
    // Catches a message that waits forever when the CLI reports its command ended but starts no
    // turn to report it.
    let mut harness = Harness::start(Options {
        report_turn_wait: Duration::from_millis(300),
        ..Options::default()
    })
    .await;
    let launch = a_command_runs_on_after_the_turn(&mut harness).await;
    change_model(&harness, "claude-opus-test").await;
    harness.send(B, "B").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        harness.is_running(&launch),
        "the CLI and its command run on"
    );
    assert_eq!(harness.launches().len(), 1, "no new launch yet");

    harness
        .emit(
            &launch,
            json!({ "type": "system", "subtype": "task_notification", "task_id": "t1",
                    "status": "completed", "summary": SUMMARY }),
        )
        .await;
    harness
        .emit(
            &launch,
            json!({ "type": "system", "subtype": "background_tasks_changed", "tasks": [] }),
        )
        .await;

    let second = harness.launch(2).await;
    harness.wait_ended(&launch).await;
    assert_eq!(second.value("--model"), Some("claude-opus-test"));
    harness.wait_written(&second, &[B]).await;
}

#[tokio::test]
async fn a_command_that_ends_without_a_summary_still_holds_a_waiting_message_for_its_report() {
    // Catches the wait for the report turn keyed on the summary text, so a command that ends with
    // none lets a waiting message close the CLI under the turn that reports it.
    let mut harness = Harness::start(Options::default()).await;
    let launch = a_command_runs_on_after_the_turn(&mut harness).await;
    change_model(&harness, "claude-opus-test").await;
    harness.send(B, "B").await;

    harness
        .emit(
            &launch,
            json!({ "type": "system", "subtype": "task_notification", "task_id": "t1",
                    "status": "completed" }),
        )
        .await;
    harness
        .emit(
            &launch,
            json!({ "type": "system", "subtype": "background_tasks_changed", "tasks": [] }),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        harness.is_running(&launch),
        "the CLI waits for the turn that reports the command"
    );
    assert_eq!(harness.launches().len(), 1, "no new launch yet");

    harness.init(&launch).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;
    harness.finish(&launch, &[], json!({})).await;
    let second = harness.launch(2).await;
    harness.wait_ended(&launch).await;
    assert_eq!(second.value("--model"), Some("claude-opus-test"));
    harness.wait_written(&second, &[B]).await;
}
