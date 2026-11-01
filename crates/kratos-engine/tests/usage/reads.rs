//! What Codex says about the account's usage, and a turn that stops at its limit (D38, D39).

use kratos_engine::usage::scheduler::LimitStop;
use kratos_protocol::{Allowance, PauseReason, Provider, ThreadEventBody};
use serde_json::json;

use crate::harness::{Harness, Options, WAIT};

const THREAD: &str = "thread-A";

fn read(allowed: Option<bool>, used_percent: i64) -> serde_json::Value {
    json!({
        "accountId": "account-1", "ordinaryUsageAllowed": allowed,
        "rateLimits": { "limitId": "codex", "planType": "plus",
                        "primary": { "usedPercent": used_percent, "windowDurationMins": 300, "resetsAt": 4_102_444_800_i64 } },
    })
}

#[tokio::test]
async fn a_turn_stopped_at_the_usage_limit_pauses_and_goes_to_the_scheduler() {
    // Catches a limit stop treated as an ordinary failure, which nobody would continue, or one the
    // queue runs past (D39).
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;

    harness
        .emit(
            "turn/completed",
            json!({ "threadId": THREAD, "turn": { "id": "turn-1", "items": [], "status": "failed",
                    "error": { "message": "You've hit your usage limit.", "codexErrorInfo": "usageLimitExceeded" } } }),
        )
        .await;

    harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::ThreadPaused {
                    reason: PauseReason::UsageLimit { .. }
                }
            )
        })
        .await;
    let stops = harness.stops.as_mut().unwrap();
    let stop = tokio::time::timeout(WAIT, stops.recv())
        .await
        .expect("the scheduler hears of the stop")
        .unwrap();
    assert_eq!(
        stop,
        LimitStop {
            thread_id: harness.thread_id.clone(),
            turn_id: kratos_protocol::TurnId::new("turn-1".into()),
            provider: Provider::Codex,
        }
    );
}

#[tokio::test]
async fn a_full_read_sets_the_allowance_and_a_missing_answer_never_allows() {
    // Catches a read without Codex's answer counted as permission to run (D38).
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;

    harness
        .control("test/rateLimits", read(Some(false), 100))
        .await;
    harness.provider.refresh_usage().await.unwrap();
    let blocked = harness.usage.account(Provider::Codex).unwrap();
    assert_eq!(blocked.allowance, Allowance::Blocked);
    assert_eq!(
        blocked.buckets[0].primary.as_ref().unwrap().window_minutes,
        Some(300)
    );

    harness.control("test/rateLimits", read(None, 10)).await;
    harness.provider.refresh_usage().await.unwrap();
    assert!(matches!(
        harness.usage.account(Provider::Codex).unwrap().allowance,
        Allowance::Unavailable { .. }
    ));
}

#[tokio::test]
async fn a_failed_read_leaves_the_allowance_unavailable() {
    // Catches a stale "allowed" kept after Codex stopped answering.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness
        .control("test/rateLimits", read(Some(true), 10))
        .await;
    harness.provider.refresh_usage().await.unwrap();

    harness
        .control(
            "test/rateLimits",
            json!({ "error": "not signed in", "code": -32600 }),
        )
        .await;
    let failed = harness.provider.refresh_usage().await;

    assert!(failed.is_err());
    assert!(matches!(
        harness.usage.account(Provider::Codex).unwrap().allowance,
        Allowance::Unavailable { .. }
    ));
}

#[tokio::test]
async fn a_rolling_update_merges_and_an_account_change_forgets_everything() {
    // Catches an update that drops the buckets it leaves out, and a snapshot of the previous
    // account outliving a sign-in as someone else.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness
        .control("test/rateLimits", read(Some(true), 10))
        .await;
    harness.provider.refresh_usage().await.unwrap();
    let mut changes = harness.usage.watch();
    changes.mark_unchanged();

    harness
        .emit(
            "account/rateLimits/updated",
            json!({ "rateLimits": { "limitId": "codex", "primary": { "usedPercent": 60 } } }),
        )
        .await;
    tokio::time::timeout(WAIT, changes.changed())
        .await
        .unwrap()
        .unwrap();
    let merged = harness.usage.account(Provider::Codex).unwrap();
    assert_eq!(merged.allowance, Allowance::Allowed);
    assert_eq!(merged.buckets[0].primary.as_ref().unwrap().used_percent, 60);

    harness
        .emit("account/updated", json!({ "planType": "pro" }))
        .await;
    tokio::time::timeout(WAIT, changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(harness.usage.account(Provider::Codex), None);
}
