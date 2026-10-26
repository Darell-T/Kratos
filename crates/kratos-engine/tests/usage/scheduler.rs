//! The usage scheduler continuing a turn that stopped at a usage limit (D39, D41, L6), on a clock
//! the test moves by hand.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use kratos_engine::usage::scheduler::Clock;
use kratos_protocol::{
    Continuation, ContinuationState, MessageInput, MessageOrigin, ThreadEventBody,
};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::harness::{Harness, Options, WAIT};

const THREAD: &str = "thread-A";
/// When the spent window resets, in seconds since the epoch: 2100-01-01.
const RESET: u64 = 4_102_444_800;
const HOUR: u64 = 3_600;

fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

/// A clock that moves only when the test moves it, and remembers every wake-up asked of it.
struct ManualClock {
    now: watch::Sender<SystemTime>,
    asked: Mutex<Vec<SystemTime>>,
}

impl ManualClock {
    fn starting_at(now: SystemTime) -> Arc<ManualClock> {
        Arc::new(ManualClock {
            now: watch::channel(now).0,
            asked: Mutex::default(),
        })
    }

    fn advance_to(&self, now: SystemTime) {
        self.now.send_replace(now);
    }

    /// Waits until the scheduler asked to wake at `wake`.
    async fn wait_for_wake_up_at(&self, wake: SystemTime) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !self
            .asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&wake)
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the scheduler never asked to wake at {wake:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.now.borrow()
    }

    fn sleep_until(&self, wake: SystemTime) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(wake);
        let mut now = self.now.subscribe();
        Box::pin(async move {
            while *now.borrow_and_update() < wake {
                if now.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
    }
}

/// A full read of the account: `allowed` is Codex's answer, `bucket` the spent or free bucket.
fn read(account: &str, allowed: bool, bucket: Value) -> Value {
    json!({ "accountId": account, "ordinaryUsageAllowed": allowed, "rateLimits": bucket })
}

fn five_hour_window(used_percent: i64, resets_at: u64) -> Value {
    json!({ "limitId": "codex", "planType": "plus",
            "primary": { "usedPercent": used_percent, "windowDurationMins": 300, "resetsAt": resets_at } })
}

fn spent() -> Value {
    read("account-1", false, five_hour_window(100, RESET))
}

fn free() -> Value {
    read("account-1", true, five_hour_window(0, RESET + 5 * HOUR))
}

/// Runs turn-1 until it stops at the usage limit with `usage` as Codex's account read.
async fn stop_at_the_limit(harness: &mut Harness, usage: Value) {
    harness.running_turn("a", "turn-1").await;
    harness.control("test/rateLimits", usage).await;
    harness
        .emit(
            "turn/completed",
            json!({ "threadId": THREAD, "turn": { "id": "turn-1", "items": [], "status": "failed",
                    "error": { "message": "You've hit your usage limit.", "codexErrorInfo": "usageLimitExceeded" } } }),
        )
        .await;
}

async fn next_continuation(harness: &mut Harness) -> Continuation {
    let changed = harness
        .wait_for(|event| matches!(event, ThreadEventBody::ContinuationChanged { .. }))
        .await;
    let ThreadEventBody::ContinuationChanged { continuation } = changed else {
        unreachable!("wait_for picked a continuation change");
    };
    continuation
}

/// Waits for the continuation's message to start as a turn, and checks it is the continuation's.
async fn continuation_starts(harness: &mut Harness) {
    let started = harness.held("turn/start", 2).await;
    let text = started[1]["params"]["input"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.starts_with("Continue the task"), "{text}");
}

#[tokio::test]
async fn a_stop_at_the_five_hour_window_continues_at_the_reset_once_codex_allows_it() {
    // Catches a continuation sent before the reset, or sent at the reset on the strength of the
    // reset time alone instead of a fresh read (D39).
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    let mut harness = Harness::start(Options::default()).await;
    harness.start_scheduler(clock.clone()).await;

    stop_at_the_limit(&mut harness, spent()).await;
    let scheduled = next_continuation(&mut harness).await;
    assert_eq!(scheduled.state, ContinuationState::Scheduled);
    assert_eq!(scheduled.waits_on.limit_ids, ["codex"]);
    clock.wait_for_wake_up_at(at(RESET)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        harness.turn_requests().await.len(),
        1,
        "nothing goes before the reset"
    );

    harness.control("test/rateLimits", free()).await;
    clock.advance_to(at(RESET));

    let queued = harness
        .wait_for(|event| matches!(event, ThreadEventBody::MessageQueued { .. }))
        .await;
    let ThreadEventBody::MessageQueued { origin, input, .. } = queued else {
        unreachable!("wait_for picked a queued message");
    };
    assert_eq!(
        origin,
        MessageOrigin::Continuation {
            continuation_id: scheduled.id.clone()
        }
    );
    assert!(
        matches!(&input[0], MessageInput::Text { text } if text.as_str().starts_with("Continue"))
    );
    let sent = next_continuation(&mut harness).await;
    assert!(
        matches!(sent.state, ContinuationState::Sent { .. }),
        "{sent:?}"
    );
    continuation_starts(&mut harness).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        harness.turn_requests().await.len(),
        2,
        "one continuation, one turn"
    );
    let continuations = harness
        .details()
        .await
        .queue
        .into_iter()
        .chain([harness.message("a").await])
        .filter(|message| matches!(message.origin, MessageOrigin::Continuation { .. }))
        .count();
    assert!(continuations <= 1);
}

#[tokio::test]
async fn a_reset_that_finds_the_limit_still_spent_waits_for_the_next_reset() {
    // Catches a continuation sent while Codex still says the account is spent, or sent on the
    // strength of a rolling update, which never says whether the account may run.
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    let mut harness = Harness::start(Options::default()).await;
    harness.start_scheduler(clock.clone()).await;
    stop_at_the_limit(&mut harness, spent()).await;
    next_continuation(&mut harness).await;
    clock.wait_for_wake_up_at(at(RESET)).await;
    harness
        .emit(
            "account/rateLimits/updated",
            json!({ "rateLimits": { "limitId": "codex", "primary": { "usedPercent": 0 } } }),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        harness.turn_requests().await.len(),
        1,
        "an update is no permission"
    );

    harness
        .control(
            "test/rateLimits",
            read("account-1", false, five_hour_window(100, RESET + HOUR)),
        )
        .await;
    clock.advance_to(at(RESET));

    let rescheduled = next_continuation(&mut harness).await;
    assert_eq!(rescheduled.state, ContinuationState::Scheduled);
    assert_eq!(
        rescheduled.resets_at.and_then(|reset| reset.instant()),
        Some(at(RESET + HOUR))
    );
    assert_eq!(harness.turn_requests().await.len(), 1);
    clock.wait_for_wake_up_at(at(RESET + HOUR)).await;
    harness.control("test/rateLimits", free()).await;
    clock.advance_to(at(RESET + HOUR));
    continuation_starts(&mut harness).await;
}

#[tokio::test]
async fn a_weekly_limit_waits_for_the_user_s_go() {
    // Catches Kratos resuming on its own after a wait longer than the five-hour window (D41).
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    let mut harness = Harness::start(Options::default()).await;
    let scheduler = harness.start_scheduler(clock.clone()).await;
    let weekly = json!({ "limitId": "codex", "secondary": {
        "usedPercent": 100, "windowDurationMins": 10080, "resetsAt": RESET } });

    stop_at_the_limit(&mut harness, read("account-1", false, weekly)).await;
    let waiting = next_continuation(&mut harness).await;
    assert!(
        matches!(waiting.state, ContinuationState::AwaitingUser { .. }),
        "{waiting:?}"
    );
    clock.advance_to(at(RESET));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        harness.turn_requests().await.len(),
        1,
        "nothing goes without the user"
    );

    harness.control("test/rateLimits", free()).await;
    let scheduled = scheduler.schedule(waiting.id).await.unwrap();

    assert_eq!(scheduled.state, ContinuationState::Scheduled);
    continuation_starts(&mut harness).await;
}

#[tokio::test]
async fn three_checks_without_an_answer_hand_the_decision_to_the_user() {
    // Catches a scheduler that retries forever when Codex never says whether the account may run,
    // or that treats no answer as permission.
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    let mut harness = Harness::start(Options::default()).await;
    harness.start_scheduler(clock.clone()).await;
    stop_at_the_limit(&mut harness, spent()).await;
    next_continuation(&mut harness).await;
    clock.wait_for_wake_up_at(at(RESET)).await;
    let no_answer = json!({ "accountId": "account-1", "ordinaryUsageAllowed": null,
                            "rateLimits": five_hour_window(0, RESET + 5 * HOUR) });
    harness.control("test/rateLimits", no_answer.clone()).await;

    clock.advance_to(at(RESET));
    for (check, read) in [
        json!({ "error": "not signed in", "code": -32600 }),
        no_answer,
    ]
    .into_iter()
    .enumerate()
    {
        let next = RESET + 300 * (check as u64 + 1);
        clock.wait_for_wake_up_at(at(next)).await;
        harness.control("test/rateLimits", read).await;
        clock.advance_to(at(next));
    }

    let handed = next_continuation(&mut harness).await;
    assert!(
        matches!(handed.state, ContinuationState::AwaitingUser { ref why } if why.contains("did not say")),
        "{handed:?}"
    );
    assert_eq!(harness.turn_requests().await.len(), 1);
}

#[tokio::test]
async fn a_different_account_at_the_reset_hands_the_decision_to_the_user() {
    // Catches a continuation sent on another account's quota after the user signed in as someone
    // else.
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    let mut harness = Harness::start(Options::default()).await;
    harness.start_scheduler(clock.clone()).await;
    stop_at_the_limit(&mut harness, spent()).await;
    next_continuation(&mut harness).await;
    clock.wait_for_wake_up_at(at(RESET)).await;

    harness
        .control(
            "test/rateLimits",
            read("account-2", true, five_hour_window(0, RESET + 5 * HOUR)),
        )
        .await;
    clock.advance_to(at(RESET));

    let handed = next_continuation(&mut harness).await;
    assert!(
        matches!(handed.state, ContinuationState::AwaitingUser { ref why } if why.contains("account")),
        "{handed:?}"
    );
}

#[tokio::test]
async fn the_user_resuming_the_thread_cancels_the_continuation() {
    // Catches a continuation sent after the user took over, which would add a turn they did not
    // ask for.
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    let mut harness = Harness::start(Options::default()).await;
    harness.start_scheduler(clock.clone()).await;
    stop_at_the_limit(&mut harness, spent()).await;
    next_continuation(&mut harness).await;
    clock.wait_for_wake_up_at(at(RESET)).await;

    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    let cancelled = next_continuation(&mut harness).await;
    assert!(
        matches!(cancelled.state, ContinuationState::Cancelled { .. }),
        "{cancelled:?}"
    );
    harness.control("test/rateLimits", free()).await;
    clock.advance_to(at(RESET));
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(harness.turn_requests().await.len(), 1);
}

#[tokio::test]
async fn a_scheduled_continuation_survives_a_restart() {
    // Catches a plan kept only in memory, which a restart before the reset would lose (L6).
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    let mut harness = Harness::start(Options::default()).await;
    harness.start_scheduler(clock.clone()).await;
    stop_at_the_limit(&mut harness, spent()).await;
    next_continuation(&mut harness).await;
    let (root, thread_id) = harness.stop().await;
    let state_file = root.path().join("codex-state.json");
    std::fs::write(&state_file, json!({ "rateLimits": free() }).to_string()).unwrap();

    let options = Options {
        fake_args: vec!["--state-file".into(), state_file.into()],
        ..Options::default()
    };
    let mut harness = Harness::start_in(root, options, Some(thread_id)).await;
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    harness.start_scheduler(clock.clone()).await;
    clock.wait_for_wake_up_at(at(RESET)).await;
    clock.advance_to(at(RESET));

    let started = harness.held("turn/start", 1).await;
    let text = started[0]["params"]["input"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Continue the task"), "{text}");
}

#[tokio::test]
async fn one_bucket_still_spent_holds_the_continuation_though_the_account_may_run() {
    // Catches a continuation sent when Codex allows the account but a bucket the turn waited on
    // is still spent (D38).
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    let mut harness = Harness::start(Options::default()).await;
    harness.start_scheduler(clock.clone()).await;
    let by_limit = |model_used: i64| {
        json!({
            "codex": { "limitId": "codex", "primary": {
                "usedPercent": 100, "windowDurationMins": 300, "resetsAt": RESET } },
            "codex_model": { "limitId": "codex_model", "primary": {
                "usedPercent": model_used, "windowDurationMins": 300, "resetsAt": RESET } },
        })
    };
    let read_with = |allowed: bool, model_used: i64, codex_used: i64| {
        let mut buckets = by_limit(model_used);
        buckets["codex"]["primary"]["usedPercent"] = json!(codex_used);
        json!({ "accountId": "account-1", "ordinaryUsageAllowed": allowed,
                "rateLimits": five_hour_window(codex_used, RESET), "rateLimitsByLimitId": buckets })
    };
    stop_at_the_limit(&mut harness, read_with(false, 100, 100)).await;
    let scheduled = next_continuation(&mut harness).await;
    assert_eq!(scheduled.waits_on.limit_ids, ["codex", "codex_model"]);
    clock.wait_for_wake_up_at(at(RESET)).await;

    harness
        .control("test/rateLimits", read_with(true, 100, 0))
        .await;
    clock.advance_to(at(RESET));

    let waiting = next_continuation(&mut harness).await;
    assert_eq!(waiting.state, ContinuationState::Scheduled);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(harness.turn_requests().await.len(), 1);
}

#[tokio::test]
async fn a_message_waiting_on_the_user_holds_the_continuation() {
    // Catches a continuation that lifts the pause around a message whose delivery is uncertain,
    // which would leave that message holding a queue that no longer shows why (D11).
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    let mut harness = Harness::start(Options::default()).await;
    harness.start_scheduler(clock.clone()).await;
    harness.running_turn("a", "turn-1").await;
    harness.send("b", "also this").await;
    harness
        .provider
        .steer(harness.thread_id.clone(), crate::harness::message_id("b"))
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
        .wait_for(|event| matches!(event, ThreadEventBody::ThreadPaused { .. }))
        .await;
    harness.control("test/rateLimits", spent()).await;
    harness
        .emit(
            "turn/completed",
            json!({ "threadId": THREAD, "turn": { "id": "turn-1", "items": [], "status": "failed",
                    "error": { "message": "limit", "codexErrorInfo": "usageLimitExceeded" } } }),
        )
        .await;
    next_continuation(&mut harness).await;
    clock.wait_for_wake_up_at(at(RESET)).await;

    harness.control("test/rateLimits", free()).await;
    clock.advance_to(at(RESET));

    let handed = next_continuation(&mut harness).await;
    assert!(
        matches!(handed.state, ContinuationState::AwaitingUser { ref why } if why.contains("Message b")),
        "{handed:?}"
    );
    assert_eq!(
        harness.turn_requests().await.len(),
        2,
        "turn-1's start and b's steer only"
    );
}

#[tokio::test]
async fn a_full_queue_at_the_reset_hands_the_continuation_to_the_user() {
    // Catches a continuation that goes past the ten-message bound (D85), or one left scheduled
    // with no timer after the queue had no room.
    let clock = ManualClock::starting_at(at(RESET - HOUR));
    let mut harness = Harness::start(Options::default()).await;
    let scheduler = harness.start_scheduler(clock.clone()).await;
    stop_at_the_limit(&mut harness, spent()).await;
    next_continuation(&mut harness).await;
    for n in 0..10 {
        harness.send(&format!("m{n}"), "later").await;
    }
    clock.wait_for_wake_up_at(at(RESET)).await;

    harness.control("test/rateLimits", free()).await;
    clock.advance_to(at(RESET));

    let handed = next_continuation(&mut harness).await;
    assert!(
        matches!(handed.state, ContinuationState::AwaitingUser { ref why } if why.contains("full")),
        "{handed:?}"
    );
    harness
        .provider
        .remove(harness.thread_id.clone(), crate::harness::message_id("m9"))
        .await
        .unwrap();
    scheduler.schedule(handed.id).await.unwrap();
    continuation_starts(&mut harness).await;
}
