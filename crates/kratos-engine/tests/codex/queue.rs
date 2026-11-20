//! The queue's delivery rules (D11, D85), ported from the TypeScript provider's reliability and
//! turn tests.

use std::time::Duration;

use kratos_engine::providers::{ProviderError, Sent};
use kratos_protocol::{MessageStatus, PauseReason, ThreadEventBody, TurnId, TurnOutcome};
use serde_json::json;

use crate::harness::{Harness, Options, message_id};

const THREAD: &str = "thread-A";

#[tokio::test]
async fn resuming_while_a_message_is_being_sent_tells_the_user_to_wait() {
    // Catches resume telling the user to retry or discard a message still being sent.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;
    harness.send("b", "second").await;
    let before = harness.details().await;

    let error = harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap_err();

    assert!(matches!(error, ProviderError::Conflict(_)), "{error:?}");
    let error = error.to_string();
    assert_eq!(
        error,
        "Wait for message a to finish sending before resuming"
    );
    assert_eq!(harness.details().await.paused, before.paused);
    assert_eq!(harness.message("a").await.status, MessageStatus::Submitting);
    assert_eq!(harness.message("b").await.status, MessageStatus::Queued);
    assert_eq!(harness.turn_requests().await.len(), 1);

    harness.settle_start("turn-1", "completed").await;
    let starts = harness.held("turn/start", 2).await;
    assert_eq!(starts[1]["params"]["clientUserMessageId"], "b");
}

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
    harness.refuse("turn/start", -32602, "bad input").await;
    harness.held("turn/start", 2).await;
    harness.refuse("turn/start", -32602, "bad input").await;

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

    harness
        .refuse("turn/start", -32000, "connection reset")
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
    harness
        .refuse("turn/start", -32000, "connection reset")
        .await;
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

    harness
        .refuse("turn/start", -32000, "connection reset")
        .await;

    let started = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;
    assert!(
        matches!(started, ThreadEventBody::TurnStarted { turn_id, origin: kratos_protocol::TurnOrigin::Messages } if turn_id.as_str() == "turn-1")
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
    harness.settle("turn/interrupt", json!({})).await;
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
async fn a_long_turn_is_never_stopped_by_kratos_and_the_queue_runs_after_it() {
    // Catches Kratos stopping a healthy turn because it ran long, or pausing the queue behind it.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.send("b", "second").await;
    harness.held("turn/start", 1).await;
    harness.settle_start("turn-1", "inProgress").await;

    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let interrupts = harness
        .turn_requests()
        .await
        .into_iter()
        .filter(|request| request["method"] == "turn/interrupt")
        .count();
    assert_eq!(interrupts, 0);
    assert_eq!(harness.details().await.paused, None);

    harness.complete(THREAD, "turn-1", "completed").await;
    harness.held("turn/start", 2).await;
}

#[tokio::test]
async fn a_limit_failure_after_stop_is_interrupted_and_does_not_pause() {
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "t1").await;
    let provider = harness.provider.clone();
    let id = harness.thread_id.clone();
    let stop = tokio::spawn(async move { provider.stop(id).await });
    harness.held("turn/interrupt", 1).await;
    harness.settle("turn/interrupt", json!({})).await;
    stop.await.unwrap().unwrap();
    harness
        .emit(
            "turn/completed",
            json!({
                "threadId": THREAD, "turn": {"id":"t1","items":[],"status":"failed",
                "error":{"message":"limit","codexErrorInfo":"usageLimitExceeded"}}
            }),
        )
        .await;
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
    assert!(harness.details().await.paused.is_none());
    assert!(matches!(
        harness.stops.as_mut().unwrap().try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

async fn stop_a_turn_codex_does_not_end(answer: &str) -> ThreadEventBody {
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
    match answer {
        "accept" => harness.settle("turn/interrupt", json!({})).await,
        "refuse" => harness.refuse("turn/interrupt", -32603, "busy").await,
        _ => {}
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        stop.await.unwrap().unwrap();

        harness
            .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
            .await
    })
    .await
    .expect("interrupt RPC timeout must not postpone the stop deadline")
}

#[tokio::test]
async fn a_stopped_turn_codex_does_not_end_closes_codex_and_ends_interrupted() {
    for answer in ["accept", "refuse", "silent"] {
        let ended = stop_a_turn_codex_does_not_end(answer).await;
        assert!(
            matches!(
                ended,
                ThreadEventBody::TurnCompleted {
                    outcome: TurnOutcome::Interrupted,
                    ..
                }
            ),
            "{answer}: {ended:?}"
        );
    }
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

#[tokio::test]
async fn a_thread_codex_closes_mid_turn_ends_its_turn_as_failed() {
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;
    harness.settle_start("turn-1", "inProgress").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;

    harness
        .emit("thread/closed", json!({ "threadId": THREAD }))
        .await;

    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(
        matches!(&ended, ThreadEventBody::TurnCompleted { outcome: TurnOutcome::Failed { error }, .. } if error.contains("closed the thread")),
        "{ended:?}"
    );
}

#[tokio::test]
async fn a_late_start_answer_for_a_discarded_message_leaves_the_next_message_alone() {
    // Catches a turn/start answer credited to whichever message is starting when it arrives.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;
    harness.send("b", "second").await;
    harness
        .emit("thread/closed", json!({ "threadId": THREAD }))
        .await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::ThreadPaused { .. }))
        .await;
    harness
        .provider
        .discard(harness.thread_id.clone(), message_id("a"))
        .await
        .unwrap();
    let held = harness.held("turn/start", 2).await;
    assert_eq!(held[1]["params"]["clientUserMessageId"], "b");

    harness.settle_start("turn-a", "inProgress").await;
    harness.settle_start("turn-b", "inProgress").await;

    let started = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
        .await;
    assert!(
        matches!(&started, ThreadEventBody::TurnStarted { turn_id, .. } if turn_id.as_str() == "turn-b"),
        "{started:?}"
    );
    assert_eq!(
        harness.message("b").await.status,
        MessageStatus::Accepted {
            turn_id: TurnId::new("turn-b".to_owned())
        }
    );
    assert_eq!(harness.message("a").await.status, MessageStatus::Cancelled);
}

#[tokio::test]
async fn a_refused_start_for_a_discarded_message_is_not_sent_again() {
    // Catches the one retry of a refused turn/start going out after the user discarded the
    // message, so Codex runs work Kratos shows as cancelled.
    let mut harness = Harness::start(Options::default()).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;
    harness
        .emit("thread/closed", json!({ "threadId": THREAD }))
        .await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::ThreadPaused { .. }))
        .await;
    let discarded = harness
        .provider
        .discard(harness.thread_id.clone(), message_id("a"))
        .await
        .unwrap();
    assert_eq!(discarded.status, MessageStatus::Cancelled);

    harness
        .refuse("turn/start", -32600, "thread not found")
        .await;
    harness.send("b", "second").await;

    let started = harness.held("turn/start", 2).await;
    assert_eq!(
        started[1]["params"]["clientUserMessageId"], "b",
        "the discarded message was sent again"
    );
    assert_eq!(harness.message("a").await.status, MessageStatus::Cancelled);
}

#[tokio::test]
async fn a_stop_codex_ignores_ends_only_its_own_thread_s_turn() {
    let mut harness = Harness::start(Options {
        stop_timeout: Duration::from_millis(300),
        ..Options::default()
    })
    .await;
    let other = harness.another_thread("other").await;
    harness.send("a", "stuck").await;
    harness.held("turn/start", 1).await;
    harness
        .settle_start_everywhere("turn-1", "inProgress")
        .await;
    harness
        .provider
        .send(
            other.clone(),
            message_id("b"),
            crate::harness::text_input("finishes"),
            kratos_protocol::MessageOrigin::User,
        )
        .await
        .unwrap();
    harness.held("turn/start", 2).await;
    harness
        .settle_start_everywhere("turn-2", "inProgress")
        .await;
    let (stuck, finishing) = (harness.fake_of("a").await, harness.fake_of("b").await);
    assert_ne!(stuck, finishing, "each thread runs a server of its own");

    let provider = harness.provider.clone();
    let thread_id = harness.thread_id.clone();
    let stop = tokio::spawn(async move { provider.stop(thread_id).await });
    harness.held("turn/interrupt", 1).await;
    harness
        .control_at(
            stuck,
            fake_agents::codex_control::settle("turn/interrupt", json!({}), None),
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
                outcome: TurnOutcome::Interrupted,
                ..
            }
        ),
        "{ended:?}"
    );

    harness
        .control_at(
            finishing,
            fake_agents::codex_control::complete(THREAD, "turn-2", "completed"),
        )
        .await;
    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    let finished = loop {
        let other = other.clone();
        let events = harness
            .store
            .call(move |store| {
                store.events_after(&other, kratos_protocol::Seq::BEFORE_FIRST, usize::MAX)
            })
            .await
            .unwrap();
        let finished = events.into_iter().find_map(|event| match event.event {
            ThreadEventBody::TurnCompleted { outcome, .. } => Some(outcome),
            _ => None,
        });
        if let Some(finished) = finished {
            break finished;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the other thread's turn never ended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        finished,
        TurnOutcome::Completed,
        "the other thread's turn finishes normally, untouched by the stop"
    );
}
