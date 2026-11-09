//! Claude's usage limits (D38, D39, D41, L6): what its rate limit events and `get_usage` reads
//! store, and the scheduler continuing a turn that stopped at the limit, on a clock the test moves
//! by hand, over a real store, driving the scripted `fake-claude`.

use std::path::Path;
use std::time::Duration;

use kratos_engine::scheduler::SchedulerHandle;
use kratos_protocol::{
    AccountUsage, Allowance, ContextUsage, Continuation, ContinuationId, ContinuationState,
    LiveUpdate, MessageOrigin, PauseReason, Provider, ThreadEventBody, Timestamp, TurnId,
    UsageWindow,
};
use serde_json::{Value, json};

use crate::clock::{HOUR, MINUTE, ManualClock, RESET, at};
use crate::harness::{A, B, Harness, Launch, Options, message_id, text_input};

/// `RESET + seconds` in the form `get_usage` measured: ISO 8601 with microseconds and an offset.
fn measured(seconds: u64) -> String {
    let stamp = Timestamp::at(at(seconds)).unwrap();
    format!("{}.000000+00:00", &stamp.as_str()[..19])
}

fn options(clock: &std::sync::Arc<ManualClock>) -> Options {
    Options {
        clock: Some(clock.clone()),
        ..Options::default()
    }
}

/// The frame the CLI sends when the server answers a request with its verdict on a limit.
fn rate_limit_event(status: &str, kind: &str, resets_at: u64) -> Value {
    json!({ "type": "rate_limit_event", "uuid": "00000000-0000-4000-8000-0000000000e1",
            "session_id": "00000000-0000-4000-8000-0000000000e2",
            "rate_limit_info": { "status": status, "rateLimitType": kind, "resetsAt": resets_at } })
}

/// The result of a turn that stopped because the account ran out of usage.
fn limit_failure() -> Value {
    json!({ "subtype": "error_during_execution", "is_error": true,
            "errors": ["Claude usage limit reached"] })
}

fn recorded_rows() -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kratos-claude-client/tests/transcripts/s1-launch-and-stream/wire.jsonl");
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["msg"].clone())
        .collect()
}

fn recorded(kind: &str) -> Value {
    recorded_rows()
        .into_iter()
        .find(|message| message["type"] == kind)
        .unwrap_or_else(|| panic!("the recording holds a {kind}"))
}

fn unified_percents(usage: &AccountUsage) -> (Option<i32>, Option<i32>) {
    let unified = usage.buckets.iter().find(|b| b.limit_id == "unified");
    let percent = |window: Option<&UsageWindow>| window.map(|w| w.used_percent);
    (
        percent(unified.and_then(|b| b.primary.as_ref())),
        percent(unified.and_then(|b| b.secondary.as_ref())),
    )
}

/// Waits until the tracker's Claude snapshot is one `wanted` picks, and returns it.
async fn wait_for_usage(harness: &Harness, wanted: impl Fn(&AccountUsage) -> bool) -> AccountUsage {
    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    loop {
        if let Some(usage) = harness.usage.account(Provider::Claude)
            && wanted(&usage)
        {
            return usage;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Claude's usage is {:?}",
            harness.usage.account(Provider::Claude)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Sets what the process answers a `get_usage` with: the five-hour window at `five` percent
/// resetting at `resets_at`, and a weekly window with room.
async fn read_says(harness: &Harness, launch: &Launch, five: u64, resets_at: u64) {
    harness
        .call(
            launch,
            "test/usage",
            json!({ "fiveHour": { "utilization": five, "resetsAt": measured(resets_at) },
                    "sevenDay": { "utilization": 7, "resetsAt": measured(RESET + 100 * HOUR) } }),
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

async fn open_continuations(harness: &Harness) -> Vec<Continuation> {
    harness
        .store
        .call(|store| store.open_continuations())
        .await
        .unwrap()
        .into_iter()
        .map(|(_, continuation)| continuation)
        .collect()
}

async fn continuation(harness: &Harness, id: &ContinuationId) -> Continuation {
    let id = id.clone();
    harness
        .store
        .call(move |store| store.continuation(&id))
        .await
        .unwrap()
        .expect("the continuation is stored")
        .1
}

/// The text of every user message the process was written, oldest first.
async fn written_texts(harness: &Harness, launch: &Launch) -> Vec<String> {
    harness
        .received(launch)
        .await
        .iter()
        .filter(|frame| frame["type"] == "user")
        .map(|frame| {
            frame["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

/// Waits until the process has been written `count` user messages, and returns their texts.
async fn wait_for_written(harness: &Harness, launch: &Launch, count: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    loop {
        let texts = written_texts(harness, launch).await;
        if texts.len() >= count {
            return texts;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the process was written only {texts:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn is_continuation(text: &str) -> bool {
    text.starts_with("Continue the task")
}

/// Starts the turn of message A on the first process, lets the first usage read land, then has the
/// server reject `kind` until `RESET` and the turn fail. The process still reads the lagging
/// usage endpoint: five percent used.
async fn stop_at_the_limit(harness: &mut Harness, kind: &str) -> Launch {
    let launch = harness.begin(A, 1).await;
    wait_for_usage(harness, |usage| usage.allowance == Allowance::Allowed).await;
    read_says(harness, &launch, 5, RESET + 5 * HOUR).await;
    harness
        .emit(&launch, rate_limit_event("rejected", kind, RESET))
        .await;
    harness.finish(&launch, &[A], limit_failure()).await;
    launch
}

fn usage_limit_pause(reason: &PauseReason) -> bool {
    matches!(reason, PauseReason::UsageLimit { .. })
}

#[tokio::test]
async fn the_recorded_rate_limit_event_stores_both_windows_and_allows_the_account() {
    // Catches the event's fractions kept as fractions, its Unix-second resets read as
    // milliseconds, or an allowed event left without an allowance.
    let harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    wait_for_usage(&harness, |usage| {
        unified_percents(usage) == (Some(19), Some(7))
    })
    .await;

    harness.emit(&launch, recorded("rate_limit_event")).await;

    let usage = wait_for_usage(&harness, |usage| {
        unified_percents(usage) == (Some(53), Some(17))
    })
    .await;
    assert_eq!(usage.allowance, Allowance::Allowed);
    assert_eq!(usage.account, None);
    assert_eq!(usage.plan.as_deref(), Some("Claude Pro"));
    let unified = &usage.buckets[0];
    let reset = |window: &Option<UsageWindow>| {
        window.as_ref().and_then(|window| {
            window
                .resets_at
                .as_ref()
                .map(|reset| reset.as_str().to_owned())
        })
    };
    assert_eq!(
        reset(&unified.primary).as_deref(),
        Some("2026-10-20T21:30:00.000Z")
    );
    assert_eq!(
        reset(&unified.secondary).as_deref(),
        Some("2026-10-22T03:00:00.000Z")
    );
    assert_eq!(unified.primary.as_ref().unwrap().window_minutes, Some(300));
    assert_eq!(
        unified.secondary.as_ref().unwrap().window_minutes,
        Some(10_080)
    );
}

#[tokio::test]
async fn a_rejected_turn_pauses_at_the_limit_and_continues_once_after_a_read_that_says_it_may() {
    // Catches a continuation sent before the reset, on a read that still says the window is spent,
    // on a lagging read that contradicts the server's refusal, or sent twice (D39).
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut harness = Harness::start(options(&clock)).await;
    harness.start_scheduler().await;

    let launch = stop_at_the_limit(&mut harness, "five_hour").await;

    let pause = harness.paused_for(usage_limit_pause).await;
    assert_eq!(
        pause,
        PauseReason::UsageLimit {
            turn_id: TurnId::new(A.to_owned()),
            resets_at: Some(Timestamp::at(at(RESET)).unwrap()),
        }
    );
    let scheduled = next_continuation(&mut harness).await;
    assert_eq!(scheduled.state, ContinuationState::Scheduled);
    assert_eq!(scheduled.waits_on.limit_ids, ["unified"]);
    assert_eq!(scheduled.waits_on.provider, Provider::Claude);
    clock.wait_for_wake_up_at(at(RESET)).await;
    harness.assert_written(&launch, &[A]).await;

    read_says(&harness, &launch, 100, RESET + 5 * HOUR).await;
    clock.advance_to(at(RESET));
    let waiting = next_continuation(&mut harness).await;
    assert_eq!(waiting.state, ContinuationState::Scheduled);
    assert_eq!(
        waiting.resets_at.as_ref().map(Timestamp::instant),
        Some(at(RESET + 5 * HOUR)),
        "a read that still says the window is spent waits for its next reset"
    );
    harness.assert_written(&launch, &[A]).await;

    clock.wait_for_wake_up_at(at(RESET + 5 * HOUR)).await;
    read_says(&harness, &launch, 5, RESET + 10 * HOUR).await;
    clock.advance_to(at(RESET + 5 * HOUR));
    let sent = next_continuation(&mut harness).await;
    assert!(
        matches!(sent.state, ContinuationState::Sent { .. }),
        "{sent:?}"
    );
    let texts = wait_for_written(&harness, &launch, 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let texts_later = written_texts(&harness, &launch).await;
    assert_eq!(texts_later, texts, "one continuation message, not two");
    assert!(is_continuation(&texts[1]), "{texts:?}");
    assert_eq!(harness.details().await.paused, None);
}

fn api_retry(attempt: u32) -> Value {
    json!({ "type": "system", "subtype": "api_retry", "attempt": attempt, "max_retries": 10,
            "retry_delay_ms": 1000, "error_status": 429, "error": "rate_limit",
            "uuid": "00000000-0000-4000-8000-0000000000e3",
            "session_id": "00000000-0000-4000-8000-0000000000e2" })
}

/// A turn the CLI waits out: the server rejects, the CLI retries by itself, and the turn goes on
/// to succeed. The thread has no pause and no continuation.
async fn wait_out_the_rejection(harness: &Harness, launch: &Launch) {
    wait_for_usage(harness, |usage| usage.allowance == Allowance::Allowed).await;
    harness
        .emit(launch, rate_limit_event("rejected", "five_hour", RESET))
        .await;
    harness.emit(launch, api_retry(1)).await;
    harness.emit(launch, api_retry(2)).await;
}

#[tokio::test]
async fn a_turn_the_cli_waits_out_and_finishes_sends_nothing_at_the_reset_or_after() {
    // Catches Kratos reading the rejection alone as a stop and sending a second message into a turn
    // the CLI resumed and finished by itself (D39).
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut harness = Harness::start(options(&clock)).await;
    harness.start_scheduler().await;
    let launch = harness.begin(A, 1).await;

    wait_out_the_rejection(&harness, &launch).await;
    harness.finish(&launch, &[A], json!({})).await;

    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(matches!(
        ended,
        ThreadEventBody::TurnCompleted {
            outcome: kratos_protocol::TurnOutcome::Completed,
            ..
        }
    ));
    assert_eq!(harness.details().await.paused, None);
    assert!(open_continuations(&harness).await.is_empty());
    for now in [RESET, RESET + HOUR] {
        clock.advance_to(at(now));
        harness.assert_written(&launch, &[A]).await;
    }
    assert!(open_continuations(&harness).await.is_empty());
}

#[tokio::test]
async fn an_engine_that_restarts_after_a_stop_continues_once_and_after_a_wait_sends_nothing() {
    // Catches a restart that loses the plan (L6) and never continues, one that continues twice,
    // and one that sends into a turn the CLI was still waiting out when the engine died.
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut stopped = Harness::start(options(&clock)).await;
    stopped.start_scheduler().await;
    stop_at_the_limit(&mut stopped, "five_hour").await;
    let scheduled = next_continuation(&mut stopped).await;
    assert_eq!(scheduled.state, ContinuationState::Scheduled);
    let (root, thread_id) = stopped.stop().await;

    let mut restarted = Harness::start_in(root, options(&clock), Some(thread_id)).await;
    restarted.start_scheduler().await;
    clock.advance_to(at(RESET));

    let launch = restarted.launch(2).await;
    let texts = wait_for_written(&restarted, &launch, 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(written_texts(&restarted, &launch).await, texts);
    assert!(is_continuation(&texts[0]), "{texts:?}");
    assert!(matches!(
        continuation(&restarted, &scheduled.id).await.state,
        ContinuationState::Sent { .. }
    ));

    let (root, thread_id) = restarted.stop().await;
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut waiting = Harness::start_in(root, options(&clock), Some(thread_id)).await;
    waiting.start_scheduler().await;
    let launches = waiting.launches().len();
    clock.advance_to(at(RESET + HOUR));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        waiting.launches().len(),
        launches,
        "a sent continuation is not sent again after another restart"
    );
}

#[tokio::test]
async fn a_restart_while_the_cli_waits_out_a_rejection_leaves_no_continuation_to_send() {
    // Catches a continuation made from a rejection event alone, which a restart would then send
    // into a turn the CLI may have finished.
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut running = Harness::start(options(&clock)).await;
    running.start_scheduler().await;
    let launch = running.begin(A, 1).await;
    wait_out_the_rejection(&running, &launch).await;
    let (root, thread_id) = running.stop().await;

    let mut restarted = Harness::start_in(root, options(&clock), Some(thread_id)).await;
    restarted.start_scheduler().await;
    clock.advance_to(at(RESET + HOUR));
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(open_continuations(&restarted).await.is_empty());
    assert_eq!(
        restarted.launches().len(),
        1,
        "nothing starts a CLI on its own"
    );
    assert!(!matches!(
        restarted.details().await.paused,
        Some(PauseReason::UsageLimit { .. })
    ));
}

#[tokio::test]
async fn a_weekly_rejection_waits_for_the_user_and_the_users_go_sends_at_the_reset() {
    // Catches Kratos resuming on its own after a wait longer than the five-hour window (D41), and
    // a go from the user that is ignored at the reset.
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut harness = Harness::start(options(&clock)).await;
    let scheduler: SchedulerHandle = harness.start_scheduler().await;

    let launch = stop_at_the_limit(&mut harness, "seven_day").await;

    let waiting = next_continuation(&mut harness).await;
    assert!(
        matches!(waiting.state, ContinuationState::AwaitingUser { .. }),
        "{waiting:?}"
    );
    assert_eq!(
        waiting.resets_at.as_ref().map(Timestamp::instant),
        Some(at(RESET))
    );
    clock.advance_to(at(RESET - MINUTE));
    harness.assert_written(&launch, &[A]).await;

    let scheduled = scheduler.schedule(waiting.id.clone()).await.unwrap();
    assert_eq!(scheduled.state, ContinuationState::Scheduled);
    clock.wait_for_wake_up_at(at(RESET)).await;
    harness.assert_written(&launch, &[A]).await;
    read_says(&harness, &launch, 5, RESET + 5 * HOUR).await;
    clock.advance_to(at(RESET));

    let texts = wait_for_written(&harness, &launch, 2).await;
    assert!(is_continuation(&texts[1]), "{texts:?}");
}

#[tokio::test]
async fn a_failure_without_a_rejection_is_an_ordinary_failure_and_not_a_limit_stop() {
    // Catches a logged-out account (L6) or any error read as a usage stop, which would schedule a
    // continuation that sends a message into an account that cannot run.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    harness.send(B, "next").await;

    harness
        .finish(
            &launch,
            &[A],
            json!({ "subtype": "error_during_execution", "is_error": true,
                    "errors": ["authentication_failed"] }),
        )
        .await;

    let paused = harness
        .paused_for(|reason| matches!(reason, PauseReason::TurnFailed { .. }))
        .await;
    assert!(
        matches!(paused, PauseReason::TurnFailed { ref error, .. } if error == "authentication_failed")
    );
    assert!(harness.stops.as_mut().unwrap().try_recv().is_err());
    assert!(open_continuations(&harness).await.is_empty());
}

#[tokio::test]
async fn a_rejection_the_server_later_allowed_is_not_a_stop() {
    // Catches the latest event of the turn ignored: a rejection followed by an allowed event means
    // the limit lifted, so a failure after it is not the limit's.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    wait_for_usage(&harness, |usage| usage.allowance == Allowance::Allowed).await;

    harness
        .emit(&launch, rate_limit_event("rejected", "five_hour", RESET))
        .await;
    harness
        .emit(
            &launch,
            rate_limit_event("allowed", "five_hour", RESET + 5 * HOUR),
        )
        .await;
    harness.finish(&launch, &[A], limit_failure()).await;

    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert_eq!(harness.details().await.paused, None);
    assert!(harness.stops.as_mut().unwrap().try_recv().is_err());
}

#[tokio::test]
async fn a_cli_without_get_usage_leaves_the_allowance_unavailable_and_the_decision_to_the_user() {
    // Catches a missing answer counted as permission: with no read the scheduler must ask three
    // times, then hand the continuation to the user and never send.
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut harness = Harness::start(options(&clock)).await;
    harness.start_scheduler().await;
    let launch = harness.begin(A, 1).await;
    wait_for_usage(&harness, |usage| usage.allowance == Allowance::Allowed).await;
    harness
        .call(&launch, "test/usageUnsupported", json!({}))
        .await;
    harness
        .emit(&launch, rate_limit_event("rejected", "five_hour", RESET))
        .await;
    harness.finish(&launch, &[A], limit_failure()).await;

    let scheduled = next_continuation(&mut harness).await;
    assert_eq!(scheduled.state, ContinuationState::Scheduled);
    let usage = wait_for_usage(&harness, |usage| {
        matches!(usage.allowance, Allowance::Unavailable { .. })
    })
    .await;
    assert!(
        matches!(&usage.allowance, Allowance::Unavailable { reason } if reason.contains("get_usage")),
        "{usage:?}"
    );
    clock.wait_for_wake_up_at(at(RESET)).await;
    clock.advance_to(at(RESET));
    for check in 1..=2 {
        let next = RESET + 300 * check;
        clock.wait_for_wake_up_at(at(next)).await;
        clock.advance_to(at(next));
    }

    let handed = next_continuation(&mut harness).await;
    assert!(
        matches!(handed.state, ContinuationState::AwaitingUser { ref why }
            if why == "Claude did not say whether the account may run"),
        "{handed:?}"
    );
    harness.assert_written(&launch, &[A]).await;
}

#[tokio::test]
async fn a_session_that_ends_after_a_limit_stop_keeps_the_pause_and_the_continuation() {
    // Catches the exit path cleaning away the pending resume (L6): the CLI ending while the thread
    // waits for the reset must leave the pause and the plan, and the continuation still sends.
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut harness = Harness::start(options(&clock)).await;
    harness.start_scheduler().await;
    let first = stop_at_the_limit(&mut harness, "five_hour").await;
    let scheduled = next_continuation(&mut harness).await;
    harness.paused_for(usage_limit_pause).await;

    harness.exit(&first, 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(usage_limit_pause(&harness.details().await.paused.unwrap()));
    let open = open_continuations(&harness).await;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, scheduled.id);
    assert_eq!(open[0].state, ContinuationState::Scheduled);
    clock.wait_for_wake_up_at(at(RESET)).await;
    clock.advance_to(at(RESET));
    let second = harness.launch(2).await;
    let texts = wait_for_written(&harness, &second, 1).await;
    assert!(is_continuation(&texts[0]), "{texts:?}");
}

#[tokio::test]
async fn a_different_plan_at_initialize_drops_a_usage_read_still_in_flight() {
    // Catches a read made under the old account landing after a switch, which would let the
    // scheduler send on the strength of another account's quota (D24, D38).
    let harness = Harness::start(Options::default()).await;
    let first = harness.begin(A, 1).await;
    wait_for_usage(&harness, |usage| {
        usage.plan.as_deref() == Some("Claude Pro") && usage.allowance == Allowance::Allowed
    })
    .await;
    read_says(&harness, &first, 100, RESET).await;
    harness.call(&first, "test/usageHold", json!({})).await;
    let refresh = tokio::spawn({
        let (provider, thread_id) = (harness.provider.clone(), harness.thread_id.clone());
        async move { provider.refresh_usage(thread_id).await }
    });
    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    while harness.controls(&first, "get_usage").await < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the CLI never got the read"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    harness.set_plan("Claude Max");
    let second = harness.another_thread("second").await;
    harness
        .provider
        .send(
            second,
            message_id(B),
            text_input("hello"),
            MessageOrigin::User,
        )
        .await
        .unwrap();
    harness.launch(2).await;
    wait_for_usage(&harness, |usage| {
        usage.plan.as_deref() == Some("Claude Max")
    })
    .await;
    harness.call(&first, "test/usageRelease", json!({})).await;

    assert!(
        refresh.await.unwrap().is_err(),
        "a read of the old account is not a fresh read"
    );
    let usage = harness.usage.account(Provider::Claude).unwrap();
    assert_eq!(usage.plan.as_deref(), Some("Claude Max"));
    assert_eq!(
        usage.allowance,
        Allowance::Allowed,
        "the stale read was dropped"
    );
}

#[tokio::test]
async fn a_result_reports_the_last_requests_context_in_the_turn_and_live() {
    // Catches context lost at the turn's end, the cache counted outside the input, or a window
    // taken from the wrong place (D76).
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    let (_, mut live) = harness.hub.watch_live(&harness.thread_id);
    let result = recorded("result");

    harness
        .finish(
            &launch,
            &[A],
            json!({ "usage": result["usage"], "modelUsage": result["modelUsage"] }),
        )
        .await;

    let expected = ContextUsage {
        input_tokens: 22_621,
        cached_input_tokens: 15_768,
        output_tokens: 4,
        reasoning_output_tokens: 0,
        total_tokens: 22_625,
        context_window: Some(1_000_000),
    };
    let update = tokio::time::timeout(crate::harness::WAIT, live.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        update,
        LiveUpdate::Context {
            turn_id: TurnId::new(A.to_owned()),
            usage: expected.clone(),
        }
    );
    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let ThreadEventBody::TurnCompleted { context, .. } = ended else {
        unreachable!("wait_for picked a turn end");
    };
    assert_eq!(context, Some(expected));
}

#[tokio::test]
async fn a_result_without_requests_reports_no_context() {
    // Catches zeros reported for a turn whose result lists no request, which a client would draw
    // as an empty context window.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;

    harness.finish(&launch, &[A], json!({})).await;

    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let ThreadEventBody::TurnCompleted { context, .. } = ended else {
        unreachable!("wait_for picked a turn end");
    };
    assert_eq!(context, None);
}

#[tokio::test]
async fn a_rejection_an_earlier_process_saw_does_not_decide_a_later_turn() {
    // Catches a rejection outliving its process: the CLI died mid-turn, and the next process's
    // first failure, which has nothing to do with the limit, would be read as the limit's.
    let mut harness = Harness::start(Options::default()).await;
    let first = harness.begin(A, 1).await;
    wait_for_usage(&harness, |usage| usage.allowance == Allowance::Allowed).await;
    harness
        .emit(&first, rate_limit_event("rejected", "five_hour", RESET))
        .await;
    harness.exit(&first, 1).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    let second = harness.begin(B, 2).await;
    harness.finish(&second, &[B], limit_failure()).await;

    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(!usage_limit_pause(
        &harness
            .details()
            .await
            .paused
            .unwrap_or(PauseReason::Interrupted)
    ));
    assert!(harness.stops.as_mut().unwrap().try_recv().is_err());
}

#[tokio::test]
async fn a_rejection_while_extra_usage_pays_is_not_a_limit_stop() {
    // Catches a turn that fails for another reason while overage pays, with every event saying
    // rejected, being paused as a usage limit and continued at the reset nobody asked for.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    wait_for_usage(&harness, |usage| usage.allowance == Allowance::Allowed).await;
    let mut event = rate_limit_event("rejected", "five_hour", RESET);
    event["rate_limit_info"]["isUsingOverage"] = json!(true);

    harness.emit(&launch, event).await;
    harness.finish(&launch, &[A], limit_failure()).await;

    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert_eq!(harness.details().await.paused, None);
    assert!(harness.stops.as_mut().unwrap().try_recv().is_err());
    assert!(open_continuations(&harness).await.is_empty());
    assert_eq!(
        harness.usage.account(Provider::Claude).unwrap().allowance,
        Allowance::Blocked,
        "the included quota is what the server refused"
    );
}

#[tokio::test]
async fn an_event_with_a_reset_that_cannot_be_a_time_leaves_the_provider_serving() {
    // Catches a CLI that sends milliseconds, or any huge reset, killing the Claude actor: every
    // thread would then answer "stopped" and the running turn would never end.
    let harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    wait_for_usage(&harness, |usage| usage.allowance == Allowance::Allowed).await;

    let mut event = rate_limit_event("rejected", "five_hour", 1_792_531_800_000);
    event["rate_limit_info"]["unifiedWindows"] = json!({
        "five_hour": { "utilization": 0.5, "resetsAt": u64::MAX },
        "seven_day": { "utilization": 0.1, "resetsAt": 1_792_531_800_000_u64 },
    });
    harness.emit(&launch, event).await;
    harness.finish(&launch, &[A], json!({})).await;

    harness
        .message_becomes(A, |status| {
            matches!(status, kratos_protocol::MessageStatus::Accepted { .. })
        })
        .await;
    harness.send(B, "still served").await;
    harness.wait_written(&launch, &[A, B]).await;
}

#[tokio::test]
async fn a_turn_the_user_stopped_is_never_a_limit_stop_whatever_its_result_says() {
    // Catches the user's Stop becoming an automatic continuation: the CLI answers the interrupt
    // with a failed result after a rejection, and Kratos would send the work again at the reset.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    wait_for_usage(&harness, |usage| usage.allowance == Allowance::Allowed).await;
    harness
        .emit(&launch, rate_limit_event("rejected", "five_hour", RESET))
        .await;

    harness
        .provider
        .stop(harness.thread_id.clone())
        .await
        .unwrap();
    harness.finish(&launch, &[A], limit_failure()).await;

    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(
        !matches!(
            harness.details().await.paused,
            Some(PauseReason::UsageLimit { .. })
        ),
        "{:?}",
        harness.details().await.paused
    );
    assert!(harness.stops.as_mut().unwrap().try_recv().is_err());
    assert!(open_continuations(&harness).await.is_empty());
}

#[tokio::test]
async fn the_context_is_the_last_request_not_the_first_the_sum_or_the_top_level() {
    // Catches the three ways to read the wrong request: the first of several, the sum over the
    // turn, or the cumulative top-level usage. Only the last request fills the window.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;

    harness
        .finish(
            &launch,
            &[A],
            json!({
                "modelUsage": { "model-a": { "contextWindow": 200_000 } },
                "usage": {
                    "input_tokens": 900, "cache_creation_input_tokens": 800,
                    "cache_read_input_tokens": 700, "output_tokens": 600,
                    "iterations": [
                        { "input_tokens": 10, "cache_creation_input_tokens": 20,
                          "cache_read_input_tokens": 30, "output_tokens": 40 },
                        { "input_tokens": 1, "cache_creation_input_tokens": 2,
                          "cache_read_input_tokens": 3, "output_tokens": 4 },
                    ],
                },
            }),
        )
        .await;

    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let ThreadEventBody::TurnCompleted { context, .. } = ended else {
        unreachable!("wait_for picked a turn end");
    };
    assert_eq!(
        context,
        Some(ContextUsage {
            input_tokens: 6,
            cached_input_tokens: 3,
            output_tokens: 4,
            reasoning_output_tokens: 0,
            total_tokens: 10,
            context_window: Some(200_000),
        })
    );
}

#[tokio::test]
async fn a_provider_that_is_not_running_tells_the_scheduler_and_the_user_why() {
    // Catches the scheduler's own list of providers losing the reason a provider is missing: the
    // user would read "not running" where the engine knows "not installed".
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut stopped = Harness::start(options(&clock)).await;
    stopped.start_scheduler().await;
    stop_at_the_limit(&mut stopped, "five_hour").await;
    let scheduled = next_continuation(&mut stopped).await;
    let (root, thread_id) = stopped.stop().await;

    let mut restarted = Harness::start_in(root, options(&clock), Some(thread_id)).await;
    let providers = kratos_engine::providers::Providers {
        codex: Err("Codex is not installed".to_owned()),
        claude: Err("Claude Code is not installed".to_owned()),
    };
    restarted.start_scheduler_with(providers).await;
    clock.advance_to(at(RESET));
    for check in 1..=2 {
        let next = RESET + 300 * check;
        clock.wait_for_wake_up_at(at(next)).await;
        clock.advance_to(at(next));
    }

    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    loop {
        let state = continuation(&restarted, &scheduled.id).await.state;
        if let ContinuationState::AwaitingUser { why } = state {
            assert_eq!(why, "Claude Code is not installed");
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "still {state:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_session_of_an_account_that_was_signed_out_no_longer_writes_usage() {
    // Catches an old session, still open when another subscription signs in, writing its events
    // and reads over the new account's numbers, so the scheduler would decide on the wrong account.
    let harness = Harness::start(Options::default()).await;
    let old = harness.begin(A, 1).await;
    wait_for_usage(&harness, |usage| {
        usage.plan.as_deref() == Some("Claude Pro") && usage.allowance == Allowance::Allowed
    })
    .await;
    harness.set_plan("Claude Max");
    let second = harness.another_thread("second").await;
    harness
        .provider
        .send(
            second,
            message_id(B),
            text_input("hello"),
            MessageOrigin::User,
        )
        .await
        .unwrap();
    harness.launch(2).await;
    wait_for_usage(&harness, |usage| {
        usage.plan.as_deref() == Some("Claude Max") && usage.allowance == Allowance::Allowed
    })
    .await;

    harness
        .emit(&old, rate_limit_event("rejected", "five_hour", RESET))
        .await;
    read_says(&harness, &old, 100, RESET).await;
    let read = harness
        .provider
        .refresh_usage(harness.thread_id.clone())
        .await;

    assert!(
        read.is_err(),
        "a read of the old account is not a fresh read: {read:?}"
    );
    let usage = harness.usage.account(Provider::Claude).unwrap();
    assert_eq!(usage.plan.as_deref(), Some("Claude Max"));
    assert_eq!(usage.allowance, Allowance::Allowed);
    assert_eq!(unified_percents(&usage), (Some(19), Some(7)));
}

/// Plays a second turn on the first process: message B is written, accepted, and in progress.
async fn second_turn(harness: &Harness, launch: &Launch) {
    harness.send(B, B).await;
    harness.wait_written(launch, &[A, B]).await;
    harness.accept(launch, B).await;
}

#[tokio::test]
async fn a_rejection_seen_between_turns_does_not_decide_the_next_turn() {
    // Catches an event that arrived while the thread was idle deciding how the next turn ended:
    // the turn that follows fails for its own reason, and would be continued at a reset.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    harness
        .emit(&launch, rate_limit_event("rejected", "five_hour", RESET))
        .await;

    second_turn(&harness, &launch).await;
    harness.finish(&launch, &[B], limit_failure()).await;

    harness
        .wait_for(|event| {
            matches!(event, ThreadEventBody::TurnCompleted { turn_id, .. } if turn_id.as_str() == B)
        })
        .await;
    assert!(
        !matches!(
            harness.details().await.paused,
            Some(PauseReason::UsageLimit { .. })
        ),
        "{:?}",
        harness.details().await.paused
    );
    assert!(harness.stops.as_mut().unwrap().try_recv().is_err());
}

#[tokio::test]
async fn a_turn_with_no_reply_does_not_take_the_previous_turns_model_for_its_window() {
    // Catches a window read from the model of an earlier turn: a turn that wrote no reply, with
    // several models in its result, has no answering model and so no window.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    harness
        .emit(
            &launch,
            json!({ "type": "assistant", "uuid": "u1", "session_id": "s",
                    "message": { "id": "msg_1", "model": "model-a",
                                 "content": [{ "type": "text", "text": "hi" }] } }),
        )
        .await;
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    second_turn(&harness, &launch).await;
    harness
        .finish(
            &launch,
            &[B],
            json!({
                "modelUsage": { "model-a": { "contextWindow": 200_000 },
                                "model-b": { "contextWindow": 1_000_000 } },
                "usage": { "iterations": [{ "input_tokens": 1, "output_tokens": 1 }] },
            }),
        )
        .await;

    let ended = harness
        .wait_for(|event| {
            matches!(event, ThreadEventBody::TurnCompleted { turn_id, .. } if turn_id.as_str() == B)
        })
        .await;
    let ThreadEventBody::TurnCompleted { context, .. } = ended else {
        unreachable!("wait_for picked a turn end");
    };
    assert_eq!(context.unwrap().context_window, None);
}

#[tokio::test]
async fn a_stop_lost_in_a_restart_waits_for_the_user_with_the_pauses_reset() {
    // Catches the user's go refused after a restart: the recovered continuation waits for the user,
    // as D41 says missing evidence must, but without the reset the pause recorded, so "resume at
    // the reset" had no time to resume at.
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut stopped = Harness::start(options(&clock)).await;
    stop_at_the_limit(&mut stopped, "five_hour").await;
    stopped.paused_for(usage_limit_pause).await;
    let (root, thread_id) = stopped.stop().await;

    let mut restarted = Harness::start_in(root, options(&clock), Some(thread_id)).await;
    let scheduler = restarted.start_scheduler().await;
    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    let waiting = loop {
        if let Some(found) = open_continuations(&restarted).await.pop() {
            break found;
        }
        assert!(tokio::time::Instant::now() < deadline, "no continuation");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    assert!(
        matches!(waiting.state, ContinuationState::AwaitingUser { .. }),
        "{waiting:?}"
    );
    assert_eq!(
        waiting.resets_at.as_ref().map(Timestamp::instant),
        Some(at(RESET))
    );
    let launch = restarted.launch(2).await;
    let scheduled = scheduler.schedule(waiting.id.clone()).await.unwrap();
    assert_eq!(scheduled.state, ContinuationState::Scheduled);
    clock.wait_for_wake_up_at(at(RESET)).await;
    restarted.assert_written(&launch, &[]).await;
    clock.advance_to(at(RESET));
    let texts = wait_for_written(&restarted, &launch, 1).await;
    assert!(is_continuation(&texts[0]), "{texts:?}");
}

#[tokio::test]
async fn a_retry_frame_the_crate_cannot_read_leaves_no_error_on_the_running_turn() {
    // Catches a renamed field in a frame Kratos only watches reaching the log as a provider error
    // on the turn that is running.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;

    harness
        .emit(
            &launch,
            json!({ "type": "system", "subtype": "api_retry", "attempt": 1, "error": "rate_limit" }),
        )
        .await;
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    let errors = harness
        .log()
        .await
        .into_iter()
        .filter(|event| matches!(event, ThreadEventBody::ProviderError { .. }))
        .count();
    assert_eq!(errors, 0);
}
