//! Claude's limit behavior that no recording has shown yet (live lane 10). The fake guesses at
//! it, so each limit response Kratos does not recognize must fail where the user sees it, and
//! never leave a turn looking hung or send continuations without end.

use std::time::Duration;

use kratos_protocol::{ContinuationState, ThreadEventBody, TurnOutcome};
use serde_json::json;

use crate::clock::{MINUTE, ManualClock, RESET, at};
use crate::harness::{A, Harness, Options};
use crate::usage::{
    limit_failure, next_continuation, open_continuations, options, rate_limit_event,
    stop_at_the_limit,
};

/// The provider errors written on the thread, as `(message, will_retry)`.
async fn provider_errors(harness: &Harness) -> Vec<(String, bool)> {
    harness
        .log()
        .await
        .into_iter()
        .filter_map(|event| match event {
            ThreadEventBody::ProviderError {
                message,
                will_retry,
                ..
            } => Some((message, will_retry)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_limit_status_kratos_does_not_know_is_written_on_the_turn_and_never_continued() {
    // Catches an unknown status read as a rejection, which would schedule continuations on a
    // guess, or dropped without a word, which leaves the user no reason for the failed turn.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;

    harness
        .emit(&launch, rate_limit_event("throttled", "five_hour", RESET))
        .await;
    harness.finish(&launch, &[A], limit_failure()).await;

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
    let errors = provider_errors(&harness).await;
    assert!(
        errors
            .iter()
            .any(|(message, will_retry)| message.contains("\"throttled\"") && !will_retry),
        "{errors:?}"
    );
    assert!(
        harness.stops.as_mut().unwrap().try_recv().is_err(),
        "an unknown status is never a limit stop"
    );
    assert!(open_continuations(&harness).await.is_empty());
}

#[tokio::test]
async fn a_rejection_for_a_limit_kratos_does_not_know_waits_for_the_user() {
    // Catches a continuation sent on its own for a limit whose window Kratos cannot read, which
    // could resume before the limit lifts and stop again.
    let clock = ManualClock::starting_at(at(RESET - 2 * MINUTE));
    let mut harness = Harness::start(options(&clock)).await;
    harness.start_scheduler().await;

    let launch = stop_at_the_limit(&mut harness, "mystery_window").await;

    let waiting = next_continuation(&mut harness).await;
    assert!(
        matches!(&waiting.state, ContinuationState::AwaitingUser { why } if why.contains("mystery_window")),
        "{waiting:?}"
    );
    clock.advance_to(at(RESET + MINUTE));
    tokio::time::sleep(Duration::from_millis(300)).await;
    harness.assert_written(&launch, &[A]).await;
}

#[tokio::test]
async fn a_rejection_whose_reset_already_passed_waits_for_the_user_and_sends_nothing() {
    // Catches a continuation sent at once for a limit the server says already reset: it would meet
    // the same refusal, and each stop would plan another, without end.
    let clock = ManualClock::starting_at(at(RESET + MINUTE));
    let mut harness = Harness::start(options(&clock)).await;
    harness.start_scheduler().await;

    let launch = stop_at_the_limit(&mut harness, "five_hour").await;

    let waiting = next_continuation(&mut harness).await;
    assert!(
        matches!(&waiting.state, ContinuationState::AwaitingUser { .. }),
        "{waiting:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    harness.assert_written(&launch, &[A]).await;
}

#[tokio::test]
async fn a_cli_that_retries_a_refused_request_says_so_on_the_turn() {
    // Catches a CLI waiting out a limit by itself looking hung: with no turn deadline, its retries
    // are the only sign the turn is alive.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;

    harness
        .emit(
            &launch,
            json!({ "type": "system", "subtype": "api_retry", "attempt": 2, "max_retries": 10,
                    "retry_delay_ms": 30_000, "error_status": 429, "error": "rate_limit" }),
        )
        .await;
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    let errors = provider_errors(&harness).await;
    assert!(
        errors.iter().any(|(message, will_retry)| *will_retry
            && message.contains("rate_limit")
            && message.contains("429")
            && message.contains("attempt 2 of 10")),
        "{errors:?}"
    );
}
