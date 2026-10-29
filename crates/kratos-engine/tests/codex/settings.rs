//! Changing a thread's settings: when a change takes effect, and which context limits Codex can
//! honor (D76, D87).

use std::time::Duration;

use kratos_engine::providers::codex::ProviderError;
use kratos_protocol::{AskReason, Permission, ThreadEventBody, ThreadSettings};
use serde_json::json;

use crate::harness::{Harness, Options};

const THREAD: &str = "thread-A";

#[tokio::test]
async fn workspace_permissions_refresh_each_turn_and_shrink_when_switching_to_ask_me() {
    let full = ThreadSettings {
        permission: Permission::FullAccess,
        ..ThreadSettings::default()
    };
    let mut harness = Harness::start(Options {
        settings: full,
        ..Options::default()
    })
    .await;
    harness.running_turn("a", "turn-1").await;
    harness.another_thread("registered-later").await;
    harness.complete(THREAD, "turn-1", "completed").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    harness.running_turn("b", "turn-2").await;
    let requests = harness.turn_requests().await;
    let roots = requests[1]["params"]["runtimeWorkspaceRoots"]
        .as_array()
        .unwrap();
    assert_eq!(roots.len(), 2);
    assert!(roots.contains(&json!(harness.root.path().join("registered-later"))));

    harness
        .provider
        .set_settings(harness.thread_id.clone(), ThreadSettings::default())
        .await
        .unwrap();
    harness.complete(THREAD, "turn-2", "completed").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    harness.running_turn("c", "turn-3").await;
    let requests = harness.turn_requests().await;
    assert_eq!(
        requests[2]["params"]["runtimeWorkspaceRoots"],
        json!([harness.folder()])
    );
    harness.stop().await;
}

fn with_limit(limit: u64) -> ThreadSettings {
    ThreadSettings {
        context_limit_tokens: Some(limit),
        ..ThreadSettings::default()
    }
}

/// Runs turn-1 with Codex reporting a 200,000-token window, and ends it.
async fn learn_the_window(harness: &mut Harness) {
    harness.running_turn("a", "turn-1").await;
    let breakdown = json!({ "cachedInputTokens": 0, "inputTokens": 10, "outputTokens": 1,
                            "reasoningOutputTokens": 0, "totalTokens": 11 });
    harness
        .emit(
            "thread/tokenUsage/updated",
            json!({ "threadId": THREAD, "turnId": "turn-1",
                    "tokenUsage": { "last": breakdown, "total": breakdown, "modelContextWindow": 200000 } }),
        )
        .await;
    harness.complete(THREAD, "turn-1", "completed").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
}

#[tokio::test]
async fn a_context_limit_waits_until_codex_has_reported_the_model_s_window() {
    // Catches a limit accepted with nothing to check it against, which could claim a window larger
    // than the model's (D76).
    let harness = Harness::start(Options::default()).await;

    let refused = harness
        .provider
        .set_settings(harness.thread_id.clone(), with_limit(50_000))
        .await;

    assert!(
        matches!(refused, Err(ProviderError::Unsupported(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_context_limit_larger_than_the_model_s_window_is_refused() {
    // Catches Kratos inventing a larger window than the model has (D76).
    let mut harness = Harness::start(Options::default()).await;
    learn_the_window(&mut harness).await;

    let refused = harness
        .provider
        .set_settings(harness.thread_id.clone(), with_limit(200_001))
        .await;

    assert!(
        matches!(refused, Err(ProviderError::Unsupported(ref text)) if text.contains("200000")),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_new_context_limit_reloads_the_thread_with_it_before_the_next_turn() {
    // Catches a limit stored but never sent, since Codex applies one only when it loads a thread.
    let mut harness = Harness::start(Options::default()).await;
    learn_the_window(&mut harness).await;

    harness
        .provider
        .set_settings(harness.thread_id.clone(), with_limit(50_000))
        .await
        .unwrap();
    harness.send("b", "second").await;
    harness.held("turn/start", 2).await;

    let opened = harness.control("test/threadRequests", json!({})).await;
    let methods: Vec<&str> = opened
        .as_array()
        .unwrap()
        .iter()
        .map(|request| request["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        ["thread/start", "thread/unsubscribe", "thread/resume"]
    );
    assert_eq!(
        opened[2]["params"]["config"]["model_context_window"],
        50_000
    );
}

#[tokio::test]
async fn a_limit_changed_while_a_turn_runs_reaches_codex_before_the_next_turn() {
    // Catches a change made mid-turn that is stored and then never applied.
    let mut harness = Harness::start(Options::default()).await;
    learn_the_window(&mut harness).await;
    harness.running_turn("b", "turn-2").await;

    harness
        .provider
        .set_settings(harness.thread_id.clone(), with_limit(50_000))
        .await
        .unwrap();
    harness.complete(THREAD, "turn-2", "completed").await;
    harness.send("c", "third").await;
    harness.held("turn/start", 3).await;

    let opened = harness.control("test/threadRequests", json!({})).await;
    assert_eq!(opened.as_array().unwrap().len(), 3);
    assert_eq!(
        opened[2]["params"]["config"]["model_context_window"],
        50_000
    );
}

#[tokio::test]
async fn a_permission_changed_mid_turn_applies_from_the_next_turn() {
    // Catches a looser permission that reaches the turn already running, whose sandbox was chosen
    // under the stricter one (D87).
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    let full_access = ThreadSettings {
        permission: Permission::FullAccess,
        ..ThreadSettings::default()
    };

    harness
        .provider
        .set_settings(harness.thread_id.clone(), full_access)
        .await
        .unwrap();
    harness
        .ask(
            "ask-1",
            "item/commandExecution/requestApproval",
            json!({ "threadId": THREAD, "turnId": "turn-1", "itemId": "item-1", "startedAtMs": 0,
                    "command": "ls" }),
        )
        .await;

    let requested = harness
        .wait_for(|event| matches!(event, ThreadEventBody::ApprovalRequested { .. }))
        .await;
    assert!(matches!(
        requested,
        ThreadEventBody::ApprovalRequested { ref approval } if approval.why == AskReason::AskEveryAction
    ));
    harness.complete(THREAD, "turn-1", "completed").await;
    harness.send("b", "second").await;
    let started = harness.held("turn/start", 2).await;
    assert_eq!(started[1]["params"]["permissions"], ":danger-full-access");
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn a_blank_model_is_refused() {
    // Catches a setting Codex would reject at the next turn, after the user has moved on.
    let harness = Harness::start(Options::default()).await;
    let blank = ThreadSettings {
        model: Some("  ".into()),
        ..ThreadSettings::default()
    };

    let refused = harness
        .provider
        .set_settings(harness.thread_id.clone(), blank)
        .await;

    assert!(
        matches!(refused, Err(ProviderError::Unsupported(_))),
        "{refused:?}"
    );
}
