//! Windows startup must settle sandbox setup before even attaching a thread.

use kratos_protocol::{MessageStatus, PauseReason, ThreadEventBody};
use serde_json::{Value, json};

use crate::harness::{Harness, Options, WAIT};

async fn start(setup: Value) -> Harness {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state.json");
    std::fs::write(
        &state,
        json!({ "sandboxReadiness": "notConfigured", "sandboxSetup": setup }).to_string(),
    )
    .unwrap();
    Harness::start_in(
        root,
        Options {
            fake_args: vec!["--state-file".into(), state.into()],
            ..Options::default()
        },
        None,
    )
    .await
}

#[tokio::test]
async fn windows_sandbox_setup_holds_all_turns_until_ready() {
    let mut harness = start(json!({ "hold": true })).await;
    harness.send("a", "work").await;
    tokio::time::timeout(WAIT, async {
        loop {
            let requests = harness.control("test/sandboxRequests", json!({})).await;
            if requests
                .as_array()
                .unwrap()
                .iter()
                .any(|request| request["method"] == "windowsSandbox/setupStart")
            {
                assert_eq!(requests[1]["params"]["mode"], "elevated");
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("setup starts");

    assert_eq!(harness.message("a").await.status, MessageStatus::Queued);
    assert_eq!(
        harness.control("test/threadRequests", json!({})).await,
        json!([])
    );
    assert!(harness.turn_requests().await.is_empty());

    harness.control("test/releaseSandboxSetup", json!({})).await;
    harness.held("turn/start", 1).await;
    let requests = harness.control("test/sandboxRequests", json!({})).await;
    assert_eq!(requests[2]["method"], "windowsSandbox/readiness");
    harness.stop().await;
}

#[tokio::test]
async fn windows_sandbox_setup_failure_keeps_the_message_queued_and_reports_why() {
    let mut harness =
        start(json!({ "success": false, "error": "sandbox provisioning denied" })).await;
    harness.send("a", "work").await;
    let paused = harness
        .wait_for(|event| matches!(event, ThreadEventBody::ThreadPaused { .. }))
        .await;
    assert!(
        matches!(&paused, ThreadEventBody::ThreadPaused { reason: PauseReason::Unavailable { error } } if error.contains("sandbox provisioning denied")),
        "{paused:?}"
    );
    assert_eq!(harness.message("a").await.status, MessageStatus::Queued);
    harness.stop().await;
}

#[tokio::test]
async fn windows_sandbox_setup_success_without_readiness_still_blocks_work() {
    let mut harness = start(json!({ "success": true, "readinessAfter": "updateRequired" })).await;
    harness.send("a", "work").await;
    let paused = harness
        .wait_for(|event| matches!(event, ThreadEventBody::ThreadPaused { .. }))
        .await;
    assert!(
        matches!(&paused, ThreadEventBody::ThreadPaused { reason: PauseReason::Unavailable { error } } if error.contains("readiness is not ready")),
        "{paused:?}"
    );
    assert_eq!(harness.message("a").await.status, MessageStatus::Queued);
    harness.stop().await;
}
