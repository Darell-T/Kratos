//! Steering a running turn, stopping one before Codex named it, settling a refused message, and
//! sharing Codex's start slots between threads.

use std::time::Duration;

use kratos_engine::providers::codex::ProviderError;
use kratos_protocol::{
    AccessLimit, MessageOrigin, MessageStatus, PauseReason, Permission, ThreadEventBody,
};
use serde_json::json;

use crate::harness::{Harness, Options, message_id, text_input};

const THREAD: &str = "thread-A";

#[tokio::test]
async fn another_thread_s_message_waits_for_a_turn_of_its_own() {
    // Catches a message from another thread steered into a running turn, where it would run with
    // that turn's permission instead of no more than its sender's.
    let mut harness = Harness::start(Options::default()).await;
    let sender = harness.another_thread("sender").await;
    harness.running_turn("a", "turn-1").await;
    let limit = AccessLimit {
        permission: Permission::AskMe,
        plan_mode: false,
    };
    let origin = MessageOrigin::Thread {
        thread_id: sender,
        limit,
    };
    harness.send_as("b", "also this", origin).await;

    let steered = harness
        .provider
        .steer(harness.thread_id.clone(), message_id("b"))
        .await;

    assert!(
        matches!(&steered, Err(ProviderError::Conflict(reason)) if reason.contains("another thread")),
        "{steered:?}"
    );
    assert_eq!(harness.message("b").await.status, MessageStatus::Queued);
    let requests = harness.turn_requests().await;
    assert!(
        requests
            .iter()
            .all(|request| request["method"] != "turn/steer"),
        "{requests:?}"
    );
}

#[tokio::test]
async fn a_steered_message_joins_the_running_turn() {
    // Catches a steer sent as a turn of its own, or one whose message stays queued after Codex
    // took it.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness.send("b", "also this").await;

    let steering = harness
        .provider
        .steer(harness.thread_id.clone(), message_id("b"))
        .await
        .unwrap();
    assert_eq!(steering.status, MessageStatus::Submitting);
    let steer = &harness.held("turn/steer", 1).await[0]["params"];
    assert_eq!(steer["expectedTurnId"], "turn-1");
    assert_eq!(steer["clientUserMessageId"], "b");
    harness
        .control(
            "test/settleTurn",
            json!({ "method": "turn/steer", "result": { "turnId": "turn-1" } }),
        )
        .await;

    harness
        .wait_for(|event| {
            matches!(event, ThreadEventBody::MessageStatusChanged { status: MessageStatus::Accepted { .. }, message_id } if message_id.as_str() == "b")
        })
        .await;
    harness.complete(THREAD, "turn-1", "completed").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        harness.held("turn/start", 1).await.len(),
        1,
        "b ran in turn-1"
    );
}

#[tokio::test]
async fn a_steer_codex_refuses_puts_the_message_back_in_the_queue() {
    // Catches a refused steer that loses the message, or marks it failed when it can simply run
    // as the next turn.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness.send("b", "also this").await;
    harness
        .provider
        .steer(harness.thread_id.clone(), message_id("b"))
        .await
        .unwrap();
    harness.held("turn/steer", 1).await;

    harness
        .control(
            "test/settleTurn",
            json!({ "method": "turn/steer", "code": -32600, "error": "turn is not steerable" }),
        )
        .await;
    harness
        .wait_for(|event| {
            matches!(event, ThreadEventBody::MessageStatusChanged { status: MessageStatus::Queued, message_id } if message_id.as_str() == "b")
        })
        .await;
    harness.complete(THREAD, "turn-1", "completed").await;

    let started = harness.held("turn/start", 2).await;
    assert_eq!(started[1]["params"]["clientUserMessageId"], "b");
}

#[tokio::test]
async fn a_steer_whose_answer_is_lost_is_never_sent_again() {
    // Catches D11 broken for steering: a steer that may have reached Codex retried, or run as the
    // next turn.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness.send("b", "also this").await;
    harness
        .provider
        .steer(harness.thread_id.clone(), message_id("b"))
        .await
        .unwrap();
    harness.held("turn/steer", 1).await;

    harness
        .control(
            "test/settleTurn",
            json!({ "method": "turn/steer", "code": -32000, "error": "connection reset" }),
        )
        .await;

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
        harness.message("b").await.status,
        MessageStatus::Unknown { .. }
    ));
    harness.complete(THREAD, "turn-1", "completed").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(harness.held("turn/steer", 1).await.len(), 1);
    assert_eq!(harness.held("turn/start", 1).await.len(), 1);
}

#[tokio::test]
async fn a_stop_sent_before_codex_names_the_turn_stops_it_once_named() {
    // Catches a stop lost because the turn had no id yet, which would leave the turn running after
    // the user saw the stop succeed.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;

    let provider = harness.provider.clone();
    let thread_id = harness.thread_id.clone();
    let stop = tokio::spawn(async move { provider.stop(thread_id).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!stop.is_finished(), "the stop waits for the turn's id");
    harness.settle_start("turn-1", "inProgress").await;

    let interrupt = &harness.held("turn/interrupt", 1).await[0]["params"];
    assert_eq!(interrupt["turnId"], "turn-1");
    harness
        .control(
            "test/settleTurn",
            json!({ "method": "turn/interrupt", "result": {} }),
        )
        .await;
    stop.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_refused_message_sent_again_runs_and_lifts_the_pause() {
    // Catches a retry that leaves the queue paused, or one that skips ahead of the messages
    // queued after it.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.send("b", "second").await;
    for attempt in 1..=2 {
        harness.held("turn/start", attempt).await;
        harness.fail_start(-32602, "bad input").await;
    }
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::ThreadPaused { .. }))
        .await;

    let retried = harness
        .provider
        .retry(harness.thread_id.clone(), message_id("a"))
        .await
        .unwrap();

    assert_eq!(retried.status, MessageStatus::Queued);
    let started = harness.held("turn/start", 3).await;
    assert_eq!(started[2]["params"]["clientUserMessageId"], "a");
    assert_eq!(harness.details().await.paused, None);
}

#[tokio::test]
async fn removing_a_refused_message_lets_the_next_one_run() {
    // Catches a removal that leaves the queue held by a message that no longer exists.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.send("b", "second").await;
    for attempt in 1..=2 {
        harness.held("turn/start", attempt).await;
        harness.fail_start(-32602, "bad input").await;
    }
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::ThreadPaused { .. }))
        .await;

    let removed = harness
        .provider
        .remove(harness.thread_id.clone(), message_id("a"))
        .await
        .unwrap();

    assert_eq!(removed.status, MessageStatus::Cancelled);
    let started = harness.held("turn/start", 3).await;
    assert_eq!(started[2]["params"]["clientUserMessageId"], "b");
}

#[tokio::test]
async fn at_most_four_threads_wait_on_codex_to_start_a_turn() {
    // Catches a burst of messages to many threads flooding Codex with starts (D85), and a waiting
    // thread that never gets the slot a finished start frees.
    let mut harness = Harness::start(Options::default()).await;
    let mut threads = vec![harness.thread_id.clone()];
    for n in 2..=5 {
        threads.push(harness.another_thread(&format!("project-{n}")).await);
    }

    for (n, thread_id) in threads.iter().enumerate() {
        harness
            .provider
            .send(
                thread_id.clone(),
                message_id(&format!("m{n}")),
                text_input("go"),
                MessageOrigin::User,
            )
            .await
            .unwrap();
    }

    harness.held("turn/start", 4).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(harness.turn_requests().await.len(), 4);
    harness.settle_start("turn-1", "inProgress").await;
    harness.held("turn/start", 5).await;
}
