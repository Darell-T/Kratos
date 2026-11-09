//! Changing a thread's settings: when a change takes effect, and which context limits Codex can
//! honor (D76, D87).

use std::time::Duration;

use kratos_engine::providers::ProviderError;
use kratos_protocol::{
    AccessLimit, AskReason, ContextLimit, MessageOrigin, Permission, ThreadEventBody,
    ThreadSettings,
};
use serde_json::json;

use crate::harness::{Harness, Options};

const THREAD: &str = "thread-A";

#[tokio::test]
async fn a_permission_reduction_replaces_the_native_session_and_keeps_history() {
    // Catches session approvals surviving a narrower permission or plan mode on the next turn.
    for (permission, plan_mode, next_permission, next_plans) in [
        (Permission::FullAccess, false, Permission::AskMe, false),
        (Permission::FullAccess, true, Permission::AskMe, true),
        (Permission::AskMe, false, Permission::AskMe, true),
    ] {
        let initial = ThreadSettings {
            permission,
            plan_mode,
            ..ThreadSettings::default()
        };
        let mut harness = Harness::start(Options {
            settings: initial,
            ..Options::default()
        })
        .await;
        harness.running_turn("a", "turn-1").await;
        harness.complete(THREAD, "turn-1", "completed").await;
        harness
            .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
            .await;
        let next = ThreadSettings {
            permission: next_permission,
            plan_mode: next_plans,
            ..ThreadSettings::default()
        };
        harness
            .provider
            .set_settings(harness.thread_id.clone(), next)
            .await
            .unwrap();
        harness.send("b", "second").await;
        let starts = harness.held("turn/start", 2).await;
        let native = starts[1]["params"]["threadId"].as_str().unwrap();
        assert_ne!(
            native, THREAD,
            "the old native session retains its approval grants"
        );
        let native = native.to_owned();
        let history = harness
            .control("thread/read", json!({ "threadId": native }))
            .await;
        assert_eq!(
            history["thread"]["turns"][0]["id"], "turn-1",
            "the conversation remains available"
        );
        let stored = harness
            .store
            .call({
                let id = harness.thread_id.clone();
                move |store| store.provider_thread(&id)
            })
            .await
            .unwrap();
        assert_eq!(stored.native_id.as_deref(), Some(native.as_str()));
        assert_eq!(stored.settings.permission, next_permission);
        assert_eq!(stored.settings.plan_mode, next_plans);
        let opened = harness.control("test/threadRequests", json!({})).await;
        assert_eq!(opened[1]["method"], "thread/fork");
        assert_eq!(opened[1]["params"]["threadId"], THREAD);
        assert_eq!(opened[1]["params"]["deferGoalContinuation"], true);
        assert_eq!(opened[1]["params"]["approvalPolicy"], "untrusted");
        assert_eq!(opened[1]["params"]["approvalsReviewer"], "user");
        assert_eq!(
            opened[1]["params"]["runtimeWorkspaceRoots"],
            json!([harness.folder()])
        );
        harness.settle_start("turn-2", "inProgress").await;
        harness.complete(&native, "turn-2", "completed").await;
        harness
            .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
            .await;
        harness.send("c", "same permission").await;
        let starts = harness.held("turn/start", 3).await;
        assert_eq!(
            starts[2]["params"]["threadId"], native,
            "unchanged permissions keep their session"
        );
        harness.stop().await;
    }
}

#[tokio::test]
async fn a_sender_limit_replaces_the_receivers_old_native_approval_session() {
    // Catches checking the receiver's desired settings before applying the sender's lower limit.
    let mut harness = Harness::start(Options {
        settings: ThreadSettings {
            permission: Permission::FullAccess,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    harness.running_turn("a", "turn-1").await;
    harness.complete(THREAD, "turn-1", "completed").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let sender = harness.another_thread("sender").await;
    harness
        .send_as(
            "b",
            "review",
            MessageOrigin::Thread {
                thread_id: sender,
                limit: AccessLimit {
                    permission: Permission::AskMe,
                    plan_mode: false,
                },
            },
        )
        .await;
    let starts = harness.held("turn/start", 2).await;
    assert_ne!(starts[1]["params"]["threadId"], THREAD);
    assert_eq!(starts[1]["params"]["permissions"], "kratos-ask");
    assert_eq!(
        harness.details().await.settings.permission,
        Permission::AskMe
    );
    harness.stop().await;
}

#[tokio::test]
async fn a_failed_permission_fork_cannot_resume_the_old_approval_grants() {
    // Catches forgetting the old native session's access when a fork fails before the downgrade.
    let mut harness = Harness::start(Options {
        settings: ThreadSettings {
            permission: Permission::FullAccess,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    harness.running_turn("a", "turn-1").await;
    harness.complete(THREAD, "turn-1", "completed").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    harness
        .provider
        .set_settings(harness.thread_id.clone(), ThreadSettings::default())
        .await
        .unwrap();
    harness.control("test/failNextFork", json!({})).await;
    harness.send("b", "narrower").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::ThreadPaused { .. }))
        .await;
    assert_eq!(
        harness.turn_requests().await.len(),
        1,
        "the failed fork starts no turn"
    );

    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    let starts = harness.held("turn/start", 2).await;
    assert_ne!(
        starts[1]["params"]["threadId"], THREAD,
        "retry must not rejoin the old grant-bearing session"
    );
    let requests = harness.control("test/threadRequests", json!({})).await;
    assert_eq!(
        requests
            .as_array()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "thread/fork")
            .count(),
        2
    );
    harness.stop().await;
}

#[tokio::test]
async fn every_permission_writes_only_the_thread_s_own_workspace() {
    // Catches a turn that lets Codex write another registered workspace, which on Windows makes
    // Codex change the permissions of every file in that project too.
    let approve = ThreadSettings {
        permission: Permission::ApproveForMe,
        ..ThreadSettings::default()
    };
    let mut harness = Harness::start(Options {
        settings: approve,
        ..Options::default()
    })
    .await;
    harness.another_thread("registered-later").await;
    harness.running_turn("a", "turn-1").await;
    harness.complete(THREAD, "turn-1", "completed").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let full = ThreadSettings {
        permission: Permission::FullAccess,
        ..ThreadSettings::default()
    };
    harness
        .provider
        .set_settings(harness.thread_id.clone(), full)
        .await
        .unwrap();
    harness.running_turn("b", "turn-2").await;

    let requests = harness.turn_requests().await;
    for request in &requests {
        assert_eq!(
            request["params"]["runtimeWorkspaceRoots"],
            json!([harness.folder()])
        );
    }
    assert_eq!(requests.len(), 2);
    harness.stop().await;
}

fn with_limit(limit: u64) -> ThreadSettings {
    ThreadSettings {
        context_limit_tokens: Some(
            ContextLimit::new(limit).expect("a limit within the protocol's range"),
        ),
        ..ThreadSettings::default()
    }
}

async fn learn_the_window(harness: &mut Harness, window: i64) {
    harness.running_turn("a", "turn-1").await;
    let breakdown = json!({ "cachedInputTokens": 0, "inputTokens": 10, "outputTokens": 1,
                            "reasoningOutputTokens": 0, "totalTokens": 11 });
    harness
        .emit(
            "thread/tokenUsage/updated",
            json!({ "threadId": THREAD, "turnId": "turn-1",
                    "tokenUsage": { "last": breakdown, "total": breakdown, "modelContextWindow": window } }),
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
async fn a_negative_window_from_codex_is_not_taken_as_the_model_s_window() {
    // Catches a negative window read as a huge one, which would accept any context limit (D76).
    let mut harness = Harness::start(Options::default()).await;
    learn_the_window(&mut harness, -1).await;

    let refused = harness
        .provider
        .set_settings(harness.thread_id.clone(), with_limit(50_000))
        .await;

    assert!(
        matches!(refused, Err(ProviderError::Unsupported(ref text)) if text.contains("has not reported")),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_context_limit_larger_than_the_model_s_window_is_refused() {
    // Catches Kratos inventing a larger window than the model has (D76).
    let mut harness = Harness::start(Options::default()).await;
    learn_the_window(&mut harness, 200_000).await;

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
    learn_the_window(&mut harness, 200_000).await;

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
    learn_the_window(&mut harness, 200_000).await;
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
