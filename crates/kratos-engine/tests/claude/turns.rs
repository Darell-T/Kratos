//! The scenarios of `packages/engine/test/claude-turns.test.ts` against the Rust provider: when a
//! message is written to Claude, what accepts it, what ends a turn, and how a stop settles.

use std::time::Duration;

use kratos_engine::providers::Sent;
use kratos_protocol::{MessageStatus, PauseReason, Provider, ThreadEventBody, TurnId, TurnOutcome};
use serde_json::json;

use crate::harness::{A, B, C, D, E, Harness, Options, interrupted, message_id};

fn accepted_by(status: &MessageStatus, turn: &str) -> bool {
    matches!(status, MessageStatus::Accepted { turn_id } if turn_id.as_str() == turn)
}

/// The turns the log shows started and ended, in order, as `started A` and `ended A interrupted`.
fn turn_trail(log: &[ThreadEventBody]) -> Vec<String> {
    log.iter()
        .filter_map(|event| match event {
            ThreadEventBody::TurnStarted { turn_id } => Some(format!("started {turn_id}")),
            ThreadEventBody::TurnCompleted {
                turn_id, outcome, ..
            } => Some(match outcome {
                TurnOutcome::Completed => format!("ended {turn_id} completed"),
                TurnOutcome::Interrupted => format!("ended {turn_id} interrupted"),
                TurnOutcome::Failed { error } => format!("ended {turn_id} failed {error}"),
            }),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn launches_claude_lazily_and_delivers_queued_messages_one_turn_at_a_time_in_order() {
    // Catches a CLI started before a message needs one (L5), a launch missing the flags that
    // isolate settings and remove Claude's command tools, a second message written before the
    // turn ahead of it has a result (D11), and a turn that does not end on its result.
    let harness = Harness::start(Options::default()).await;
    assert!(
        harness.launches().is_empty(),
        "nothing starts before a message"
    );

    let Sent::Queued(_) = harness.send(A, "Fix the test").await else {
        panic!("the message is queued");
    };
    let launch = harness.launch(1).await;
    assert_eq!(launch.cwd, harness.folder().display().to_string());
    assert!(launch.has("--replay-user-messages") && launch.has("--include-partial-messages"));
    assert_eq!(launch.value("--permission-mode"), Some("default"));
    assert_eq!(launch.value("--setting-sources"), Some("user"));
    assert!(launch.value("--resume").is_none() && launch.value("--model").is_none());
    assert!(!launch.has("--allow-dangerously-skip-permissions"));
    let at = launch
        .arguments
        .iter()
        .position(|a| a == "--disallowedTools");
    assert_eq!(
        at.map(|at| launch.arguments[at + 1].as_str()),
        Some("Bash,PowerShell,Monitor")
    );

    harness.wait_written(&launch, &[A]).await;
    let received = harness.received(&launch).await;
    assert_eq!(received[0]["request"]["subtype"], "initialize");
    let user = received
        .iter()
        .find(|frame| frame["type"] == "user")
        .unwrap();
    assert_eq!(
        user["message"]["content"],
        json!([{ "type": "text", "text": "Fix the test" }])
    );
    assert_eq!(harness.message(A).await.status, MessageStatus::Submitting);
    assert_eq!(harness.details().await.running_turn, None);

    harness.init(&launch).await;
    harness.accept(&launch, A).await;
    harness.message_becomes(A, |s| accepted_by(s, A)).await;
    assert_eq!(
        harness.details().await.running_turn,
        Some(TurnId::new(A.to_owned()))
    );

    harness.send(B, "Update the docs").await;
    harness.send(C, "Open a PR").await;
    harness.assert_written(&launch, &[A]).await;

    harness.finish(&launch, &[A], json!({})).await;
    harness.wait_written(&launch, &[A, B]).await;
    harness.accept(&launch, B).await;
    harness.finish(&launch, &[B], interrupted()).await;
    harness.wait_written(&launch, &[A, B, C]).await;
    harness.accept(&launch, C).await;
    harness.finish(&launch, &[C], json!({})).await;
    harness.message_becomes(C, |s| accepted_by(s, C)).await;
    let ended = harness.details().await;

    assert_eq!(harness.launches().len(), 1, "one process serves the thread");
    assert_eq!(ended.running_turn, None);
    assert_eq!(
        ended.paused, None,
        "an interrupted turn does not pause the queue"
    );
    assert_eq!(
        turn_trail(&harness.log().await),
        [
            format!("started {A}"),
            format!("ended {A} completed"),
            format!("started {B}"),
            format!("ended {B} interrupted"),
            format!("started {C}"),
            format!("ended {C} completed"),
        ]
    );
}

#[tokio::test]
async fn records_the_plan_that_pays_for_the_session_and_never_the_account() {
    // Catches the account's email or organization reaching the records (D22, D24), and a session
    // that is not recorded under the plan Claude reported.
    let harness = Harness::start(Options::default()).await;
    let launch = {
        harness.send(A, "hello").await;
        harness.launch(1).await
    };
    harness.wait_written(&launch, &[A]).await;
    harness.init(&launch).await;
    harness.accept(&launch, A).await;
    harness.message_becomes(A, |s| accepted_by(s, A)).await;

    let log = harness.log().await;
    let sessions: Vec<_> = log
        .iter()
        .filter_map(|event| match event {
            ThreadEventBody::ProviderSession { provider, plan } => Some((*provider, plan.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(
        sessions,
        [(Provider::Claude, Some("Claude Pro".to_owned()))]
    );
    let text = serde_json::to_string(&log).unwrap();
    assert!(
        !text.contains("synthetic-account") && !text.contains('@'),
        "{text}"
    );
}

#[tokio::test]
async fn keeps_queued_messages_until_the_stopped_turn_reports_its_result_settles_stop_on_that_result_and_keeps_the_turn_after_a_failed_stop()
 {
    // Catches a queued message started on a stop's acknowledgement instead of the stopped turn's
    // result, a second stop that sends a second interrupt, a stop that outlives the result that
    // ended it, and a failed stop after which Kratos stops or pauses the turn on its own.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    harness.send(B, "B").await;

    harness.interrupts(&launch, "hang").await;
    let (first, second, ()) = tokio::join!(
        harness.provider.stop(harness.thread_id.clone()),
        harness.provider.stop(harness.thread_id.clone()),
        async {
            while harness.controls(&launch, "interrupt").await == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            harness.finish(&launch, &[A], interrupted()).await;
        },
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(
        harness.controls(&launch, "interrupt").await,
        1,
        "one interrupt for two stops"
    );
    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(
        matches!(ended, ThreadEventBody::TurnCompleted { outcome: TurnOutcome::Interrupted, ref turn_id, .. } if turn_id.as_str() == A)
    );
    harness.wait_written(&launch, &[A, B]).await;
    harness.accept(&launch, B).await;
    harness.message_becomes(B, |s| accepted_by(s, B)).await;

    harness.interrupts(&launch, "reject").await;
    let refused = harness.provider.stop(harness.thread_id.clone()).await;
    assert!(
        matches!(&refused, Err(error) if error.to_string().contains("Interrupt rejected")),
        "{refused:?}"
    );
    harness.interrupts(&launch, "ack").await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(harness.details().await.paused, None);
    assert!(harness.is_running(&launch), "the turn still runs");
    assert_eq!(
        harness.controls(&launch, "interrupt").await,
        2,
        "only the user stops a turn"
    );
    harness.finish(&launch, &[B], json!({})).await;
    harness.message_becomes(B, |s| accepted_by(s, B)).await;
}

#[tokio::test]
async fn closes_a_session_that_never_acknowledges_a_stop_and_resumes_it_for_the_next_message() {
    // Catches a stop that waits forever on a CLI that does not answer, a closed CLI whose end
    // leaves no reason in the records, and a relaunch that starts a new session instead of
    // resuming the thread's own.
    let harness = Harness::start(Options {
        stop_timeout: Duration::from_millis(400),
        ..Options::default()
    })
    .await;
    let first = harness.begin(A, 1).await;
    harness.send(B, "B").await;

    harness.interrupts(&first, "hang").await;
    harness
        .provider
        .stop(harness.thread_id.clone())
        .await
        .unwrap();
    harness.wait_ended(&first).await;
    let message = "Claude did not stop the turn; its process was closed";
    let paused = harness
        .paused_for(|r| matches!(r, PauseReason::TurnFailed { .. }))
        .await;

    assert!(
        matches!(&paused, PauseReason::TurnFailed { error, .. } if error.contains(message)),
        "{paused:?}"
    );
    let details = harness.details().await;
    assert_eq!(
        details.running_turn, None,
        "no turn shows running without a process (L1)"
    );
    assert_eq!(details.queue.len(), 1);
    assert_eq!(harness.message(B).await.status, MessageStatus::Queued);

    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    let second = harness.launch(2).await;
    assert_eq!(
        second.value("--resume"),
        Some(first.session().to_string().as_str())
    );
    assert!(second.value("--session-id").is_none());
    harness.wait_written(&second, &[B]).await;
    harness.init(&second).await;
    harness.accept(&second, B).await;
    harness.message_becomes(B, |s| accepted_by(s, B)).await;
}

#[tokio::test]
async fn pauses_follow_up_messages_after_a_failed_result_even_one_that_raced_a_stop_until_the_queue_is_resumed()
 {
    // Catches a follow-up message started after a failed turn, which may no longer make sense,
    // and a failure's text lost when the failed result arrives after a stop.
    let harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    harness.send(B, "B").await;
    harness.send(C, "C").await;

    harness
        .provider
        .stop(harness.thread_id.clone())
        .await
        .unwrap();
    harness
        .finish(
            &launch,
            &[A],
            json!({ "is_error": true, "result": "API Error: 529 overloaded", "terminal_reason": "api_error" }),
        )
        .await;
    let paused = harness
        .paused_for(|r| matches!(r, PauseReason::TurnFailed { .. }))
        .await;

    assert!(
        matches!(&paused, PauseReason::TurnFailed { error, .. } if error == "API Error: 529 overloaded")
    );
    harness.assert_written(&launch, &[A]).await;

    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    harness.wait_written(&launch, &[A, B]).await;
    harness.accept(&launch, B).await;
    harness
        .finish(&launch, &[B], json!({ "subtype": "error_max_turns", "errors": ["Reached max turns", "Stopped early"] }))
        .await;
    let paused = harness
        .paused_for(
            |r| matches!(r, PauseReason::TurnFailed { turn_id, .. } if turn_id.as_str() == B),
        )
        .await;

    assert!(
        matches!(&paused, PauseReason::TurnFailed { error, .. } if error == "Reached max turns\nStopped early")
    );
    harness.assert_written(&launch, &[A, B]).await;
    assert_eq!(harness.message(C).await.status, MessageStatus::Queued);
}

#[tokio::test]
async fn keeps_the_thread_busy_until_every_steered_message_has_a_result_folded_into_the_turn_or_run_on_its_own()
 {
    // Catches a queued message started while a steered one still has no result, a folded message
    // that is accepted by a turn of its own, and a steered message that runs after the turn
    // that is not accepted by its own turn.
    let harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    for (id, text) in [(B, "B"), (C, "C"), (D, "D"), (E, "E")] {
        harness.send(id, text).await;
    }

    let steered = harness
        .provider
        .steer(harness.thread_id.clone(), message_id(B))
        .await
        .unwrap();
    assert_eq!(steered.status, MessageStatus::Submitting);
    let again = harness
        .provider
        .steer(harness.thread_id.clone(), message_id(B))
        .await;
    assert!(
        again.is_err(),
        "a message already steered cannot be steered again"
    );
    harness.wait_written(&launch, &[A, B]).await;
    harness.accept(&launch, B).await;
    harness.message_becomes(B, |s| accepted_by(s, A)).await;
    harness.finish(&launch, &[A, B], json!({})).await;
    harness.wait_written(&launch, &[A, B, C]).await;

    harness.accept(&launch, C).await;
    harness
        .provider
        .steer(harness.thread_id.clone(), message_id(D))
        .await
        .unwrap();
    harness.wait_written(&launch, &[A, B, C, D]).await;
    harness.finish(&launch, &[C], json!({})).await;
    harness.assert_written(&launch, &[A, B, C, D]).await;

    harness.accept(&launch, D).await;
    harness.finish(&launch, &[D], json!({})).await;
    harness.wait_written(&launch, &[A, B, C, D, E]).await;
    harness.accept(&launch, E).await;
    harness.finish(&launch, &[E], json!({})).await;
    harness.message_becomes(E, |s| accepted_by(s, E)).await;

    for (id, turn) in [(A, A), (B, A), (C, C), (D, D), (E, E)] {
        let message = harness.message(id).await;
        assert!(
            accepted_by(&message.status, turn),
            "{id}: {:?}",
            message.status
        );
    }
}

#[tokio::test]
async fn lets_steered_input_that_outlives_a_stopped_run_finish_and_stops_it_with_a_new_interrupt_that_a_late_failure_of_the_first_cannot_undo()
 {
    // Catches a stop timer that outlives the result it waited for and closes a CLI still doing the
    // steered work, an earlier interrupt's timeout that undoes a later stop, and a second stop that
    // sends nothing.
    let harness = Harness::start(Options {
        stop_timeout: Duration::from_millis(900),
        control_timeout: Duration::from_millis(1500),
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    harness.send(B, "B").await;
    harness
        .provider
        .steer(harness.thread_id.clone(), message_id(B))
        .await
        .unwrap();
    harness.wait_written(&launch, &[A, B]).await;
    harness.accept(&launch, B).await;

    harness.interrupts(&launch, "hang").await;
    let stopped_a = tokio::spawn({
        let (provider, thread_id) = (harness.provider.clone(), harness.thread_id.clone());
        async move { provider.stop(thread_id).await }
    });
    while harness.controls(&launch, "interrupt").await == 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    harness.finish(&launch, &[A], interrupted()).await;
    stopped_a.await.unwrap().unwrap();
    // The first stop's timer would have run out in here; its interrupt has not yet timed out.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        harness.is_running(&launch),
        "the steered message is still running"
    );
    assert_eq!(harness.details().await.paused, None);

    let stopped_b = tokio::spawn({
        let (provider, thread_id) = (harness.provider.clone(), harness.thread_id.clone());
        async move { provider.stop(thread_id).await }
    });
    while harness.controls(&launch, "interrupt").await < 2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    harness.wait_ended(&launch).await;
    stopped_b.await.unwrap().unwrap();

    let completed: Vec<_> = harness
        .log()
        .await
        .into_iter()
        .filter_map(|event| match event {
            ThreadEventBody::TurnCompleted {
                turn_id, outcome, ..
            } => Some((turn_id, outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(
        completed,
        [(TurnId::new(A.to_owned()), TurnOutcome::Interrupted)]
    );
    let paused = harness.paused_for(|_| true).await;
    assert!(
        matches!(&paused, PauseReason::Unavailable { error } if error.contains("process was closed")),
        "{paused:?}"
    );
}
