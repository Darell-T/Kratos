//! The queue's delivery rules (D11, D85), ported from the TypeScript provider's reliability and
//! turn tests.

use std::time::Duration;

use kratos_engine::providers::codex::{ProviderError, Sent};
use kratos_protocol::{MessageStatus, PauseReason, ThreadEventBody, TurnOutcome};
use serde_json::json;

use crate::harness::{Harness, Options, message_id};

const THREAD: &str = "thread-A";

#[tokio::test]
async fn runs_queued_messages_one_at_a_time_in_the_order_sent() {
    // Catches a queue that starts the next message before the turn before it is confirmed
    // finished (D11), or out of order.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.send("b", "second").await;

    let started = harness.held("turn/start", 1).await;
    assert_eq!(started[0]["params"]["clientUserMessageId"], "a");
    harness.settle_start("turn-1", "inProgress").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        harness.turn_requests().await.len(),
        1,
        "b waits while turn-1 runs"
    );

    harness.complete(THREAD, "turn-1", "completed").await;
    let started = harness.held("turn/start", 2).await;
    assert_eq!(started[1]["params"]["clientUserMessageId"], "b");
}

#[tokio::test]
async fn a_message_codex_refused_twice_before_running_it_fails_and_holds_the_queue() {
    // Catches a refusal that is retried without bound, or one that lets the next message run as
    // if nothing happened.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.send("b", "second").await;

    harness.held("turn/start", 1).await;
    harness.fail_start(-32602, "bad input").await;
    harness.held("turn/start", 2).await;
    harness.fail_start(-32602, "bad input").await;

    harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::ThreadPaused {
                    reason: PauseReason::Refused { .. }
                }
            )
        })
        .await;
    assert!(matches!(
        harness.message("a").await.status,
        MessageStatus::Failed { .. }
    ));
    assert_eq!(harness.message("b").await.status, MessageStatus::Queued);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        harness.turn_requests().await.len(),
        2,
        "one retry, then nothing more"
    );
}

#[tokio::test]
async fn an_uncertain_delivery_is_never_sent_again_and_holds_newer_messages() {
    // Catches the rule D11 forbids breaking: resending a message Codex may have acted on, or
    // letting a newer message overtake it.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;

    harness.fail_start(-32000, "connection reset").await;
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
    harness.send("b", "second").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(matches!(
        harness.message("a").await.status,
        MessageStatus::Unknown { .. }
    ));
    assert_eq!(harness.message("b").await.status, MessageStatus::Queued);
    assert_eq!(harness.turn_requests().await.len(), 1);
    let resumed = harness.provider.resume(harness.thread_id.clone()).await;
    assert!(
        matches!(resumed, Err(ProviderError::Conflict(_))),
        "{resumed:?}"
    );
}

#[tokio::test]
async fn discarding_an_uncertain_message_codex_never_got_cancels_it_and_frees_the_queue() {
    // Catches a discard that resends the message, or that leaves the queue held after the user
    // settled the only message holding it.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;
    harness.fail_start(-32000, "connection reset").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::ThreadPaused { .. }))
        .await;
    harness.send("b", "second").await;

    let discarded = harness
        .provider
        .discard(harness.thread_id.clone(), message_id("a"))
        .await
        .unwrap();

    assert_eq!(discarded.status, MessageStatus::Cancelled);
    let started = harness.held("turn/start", 2).await;
    assert_eq!(started[1]["params"]["clientUserMessageId"], "b");
}

#[tokio::test]
async fn a_delivery_the_history_shows_is_recorded_as_accepted() {
    // Catches a lost start answer treated as a lost message: Codex's history holds the message,
    // so it was delivered, and its turn runs.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    let held = harness.held("turn/start", 1).await;
    let history = json!({
        "id": THREAD, "cwd": harness.folder(), "model": "fake-model", "ephemeral": false,
        "status": { "type": "active", "activeFlags": [] }, "cliVersion": "0", "createdAt": 0,
        "updatedAt": 0, "modelProvider": "fake", "preview": "", "projectId": null,
        "sessionId": THREAD, "source": "appServer",
        "turns": [{ "id": "turn-1", "status": "inProgress", "items": [{
            "type": "userMessage", "id": "u", "clientId": "a", "content": held[0]["params"]["input"],
        }] }],
    });
    harness
        .control("test/setThread", json!({ "thread": history }))
        .await;

    harness.fail_start(-32000, "connection reset").await;

    let started = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;
    assert!(
        matches!(started, ThreadEventBody::TurnStarted { turn_id } if turn_id.as_str() == "turn-1")
    );
    assert!(matches!(
        harness.message("a").await.status,
        MessageStatus::Accepted { turn_id } if turn_id.as_str() == "turn-1"
    ));
}

#[tokio::test]
async fn sending_an_id_again_returns_the_first_message_and_queues_nothing() {
    // Catches a client retry after a lost reply queuing the same message twice, and identical
    // text under different ids merged into one (D85).
    let harness = Harness::start(Options::default()).await;

    let first = harness.send("a", "same text").await;
    let again = harness.send("a", "same text").await;
    let other = harness.send("b", "same text").await;

    assert!(matches!(first, Sent::Queued(_)));
    assert!(matches!(again, Sent::Existing(ref message) if message.id == message_id("a")));
    assert!(matches!(other, Sent::Queued(_)));
}

#[tokio::test]
async fn an_eleventh_waiting_message_is_refused_without_being_stored() {
    // Catches a queue with no limit, which lets a stuck thread grow without bound (D85).
    let mut harness = Harness::start(Options::default()).await;
    harness.send("m0", "first").await;
    harness.held("turn/start", 1).await;
    for n in 1..10 {
        harness.send(&format!("m{n}"), "more").await;
    }

    let full = harness.send("m10", "one too many").await;

    assert_eq!(full, Sent::Full { pending: 10 });
}

#[tokio::test]
async fn a_failed_turn_pauses_the_messages_behind_it_until_the_user_resumes() {
    // Catches follow-ups that run after a failed turn, when they may depend on its work.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.send("b", "second").await;
    harness.held("turn/start", 1).await;
    harness.settle_start("turn-1", "inProgress").await;

    harness.complete(THREAD, "turn-1", "failed").await;
    harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::ThreadPaused {
                    reason: PauseReason::TurnFailed { .. }
                }
            )
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(harness.turn_requests().await.len(), 1);

    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    harness.held("turn/start", 2).await;
}

#[tokio::test]
async fn a_failed_turn_with_nothing_queued_leaves_the_next_message_free_to_run() {
    // Catches a pause that outlives the failure it reports, so the user's next message would wait
    // for a resume nobody asked for (lane 7: the next message after a crash succeeds).
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;
    harness.settle_start("turn-1", "inProgress").await;
    harness.complete(THREAD, "turn-1", "failed").await;
    harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::TurnCompleted {
                    outcome: TurnOutcome::Failed { .. },
                    ..
                }
            )
        })
        .await;

    harness.send("b", "second").await;

    harness.held("turn/start", 2).await;
    assert_eq!(harness.details().await.paused, None);
}

#[tokio::test]
async fn a_completion_that_beats_the_start_answer_ends_the_turn_once_it_is_known() {
    // Catches a completion dropped because the turn's id was not known yet, which would leave the
    // thread showing a running turn forever.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.send("b", "second").await;
    harness.held("turn/start", 1).await;

    harness.complete(THREAD, "turn-1", "completed").await;
    harness.settle_start("turn-1", "inProgress").await;

    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let started = harness.held("turn/start", 2).await;
    assert_eq!(started[1]["params"]["clientUserMessageId"], "b");
}

#[tokio::test]
async fn a_stop_waits_for_the_turn_to_end_before_the_next_message_starts() {
    // Catches the next message starting when Codex accepts the stop, before the turn has ended.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.send("b", "second").await;
    harness.held("turn/start", 1).await;
    harness.settle_start("turn-1", "inProgress").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;

    let provider = harness.provider.clone();
    let thread_id = harness.thread_id.clone();
    let stop = tokio::spawn(async move { provider.stop(thread_id).await });
    harness.held("turn/interrupt", 1).await;
    harness
        .control(
            "test/settleTurn",
            json!({ "method": "turn/interrupt", "result": {} }),
        )
        .await;
    stop.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        harness.held("turn/start", 1).await.len(),
        1,
        "b waits for the turn's end"
    );

    harness.complete(THREAD, "turn-1", "interrupted").await;
    harness.held("turn/start", 2).await;
}

#[tokio::test]
async fn a_turn_that_runs_out_of_time_is_stopped_and_the_queue_behind_it_pauses() {
    // Catches a deadline that never fires, or one that lets dependent work start.
    let mut harness = Harness::start(Options {
        turn_timeout: Duration::from_millis(300),
        ..Options::default()
    })
    .await;
    harness.send("a", "first").await;
    harness.send("b", "second").await;
    harness.held("turn/start", 1).await;
    harness.settle_start("turn-1", "inProgress").await;

    harness.held("turn/interrupt", 1).await;

    harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::ThreadPaused {
                    reason: PauseReason::TimedOut { .. }
                }
            )
        })
        .await;
}

#[tokio::test]
async fn a_turn_whose_deadline_stop_codex_never_answers_ends_with_codex() {
    // Catches a timed-out turn left running when Codex does not answer the stop: nothing else
    // would stop it, so it would run and hold the queue for good.
    let mut harness = Harness::start(Options {
        turn_timeout: Duration::from_millis(300),
        ..Options::default()
    })
    .await;
    harness.send("a", "first").await;
    harness.send("b", "second").await;
    harness.held("turn/start", 1).await;
    harness.settle_start("turn-1", "inProgress").await;
    harness.held("turn/interrupt", 1).await;

    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    assert!(
        matches!(
            ended,
            ThreadEventBody::TurnCompleted {
                outcome: TurnOutcome::Failed { .. },
                ..
            }
        ),
        "{ended:?}"
    );
    let details = harness.details().await;
    assert_eq!(details.running_turn, None);
    assert!(details.paused.is_some(), "b waits for the user");
    assert_eq!(harness.message("b").await.status, MessageStatus::Queued);
}

#[tokio::test]
async fn a_turn_that_stops_on_request_but_never_ends_closes_codex() {
    // Catches a turn stuck after an accepted stop, which would hold the queue forever.
    let mut harness = Harness::start(Options {
        stop_timeout: Duration::from_millis(300),
        ..Options::default()
    })
    .await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;
    harness.settle_start("turn-1", "inProgress").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;

    let provider = harness.provider.clone();
    let thread_id = harness.thread_id.clone();
    let stop = tokio::spawn(async move { provider.stop(thread_id).await });
    harness.held("turn/interrupt", 1).await;
    harness
        .control(
            "test/settleTurn",
            json!({ "method": "turn/interrupt", "result": {} }),
        )
        .await;
    stop.await.unwrap().unwrap();

    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(
        matches!(
            ended,
            ThreadEventBody::TurnCompleted {
                outcome: TurnOutcome::Failed { .. },
                ..
            }
        ),
        "{ended:?}"
    );
}

#[tokio::test]
async fn codex_exiting_mid_start_leaves_the_message_uncertain() {
    // Catches a start whose answer died with the process treated as refused, which would let the
    // user resend work Codex may have begun.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;

    harness.exit_fake(1).await;

    harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::MessageStatusChanged {
                    status: MessageStatus::Unknown { .. },
                    ..
                }
            )
        })
        .await;
}
