use fake_agents::responses::Reply;
use kratos_protocol::{AskReason, Permission, ThreadEventBody, ThreadSettings};
use serde_json::json;

use crate::support::Harness;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires pinned Codex 0.159.1 and a provisioned Windows sandbox home; CI runs it"]
async fn a_full_access_thread_runs_a_harmless_command() {
    let mut harness = Harness::start(
        ThreadSettings {
            permission: Permission::FullAccess,
            ..ThreadSettings::default()
        },
        vec![
            Reply::command("Write-Output KRATOS_FULL_CONTROL"),
            Reply::Text("Done.".into()),
        ],
    )
    .await;
    harness.send("full-access-control");
    harness.completed().await;
    assert!(
        harness
            .model
            .tool_outputs()
            .iter()
            .any(|output| output.to_string().contains("KRATOS_FULL_CONTROL"))
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires pinned Codex 0.159.1 and a provisioned Windows sandbox home; CI runs it"]
async fn a_full_access_thread_asks_before_a_dangerous_command() {
    let mut harness = Harness::start(
        ThreadSettings {
            permission: Permission::FullAccess,
            ..ThreadSettings::default()
        },
        vec![
            Reply::command("Remove-Item -LiteralPath delete-fixture -Recurse -Force"),
            Reply::Text("Done.".into()),
        ],
    )
    .await;
    let fixture = harness.folder().join("delete-fixture");
    std::fs::create_dir(&fixture).unwrap();
    std::fs::write(fixture.join("keep.txt"), "keep me").unwrap();
    harness.send("dangerous-delete");
    let approval = loop {
        match harness.next_event().await {
            ThreadEventBody::ApprovalRequested { approval } => break approval,
            ThreadEventBody::ThreadPaused { reason } => panic!("real Codex paused: {reason:?}"),
            ThreadEventBody::TurnCompleted { outcome, .. } => {
                panic!("the delete ran without asking: {outcome:?}")
            }
            _ => {}
        }
    };
    assert_eq!(approval.why, AskReason::Dangerous);
    let declined = harness
        .client
        .post(
            &format!("/api/approvals/{}", approval.id),
            &json!({ "decision": "decline" }),
        )
        .unwrap();
    assert_eq!(declined.status, 200, "{:?}", declined.body);
    harness.completed().await;
    assert!(
        fixture.join("keep.txt").exists(),
        "the declined delete still ran"
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires pinned Codex 0.159.1 and a dedicated provisioned Windows sandbox home; CI runs it"]
async fn plan_mode_real_codex_sandbox_refuses_a_write_after_a_successful_read() {
    let mut harness = Harness::start(
        ThreadSettings {
            permission: Permission::ApproveForMe,
            plan_mode: true,
            ..ThreadSettings::default()
        },
        vec![
            Reply::command("Get-Content -LiteralPath control.txt"),
            Reply::command("Set-Content -LiteralPath sandbox-denied.txt -Value forbidden-write"),
            Reply::Text("Sandbox probe completed.".to_owned()),
        ],
    )
    .await;
    std::fs::write(
        harness.folder().join("control.txt"),
        "KRATOS_READ_CONTROL_4baf2e",
    )
    .unwrap();
    let forbidden = harness.folder().join("sandbox-denied.txt");
    harness.send("sandbox-probe");
    harness.completed().await;

    let outputs = harness.model.tool_outputs();
    assert_eq!(
        outputs.len(),
        2,
        "the real Codex must attempt both tool calls: {outputs:?}"
    );
    let control = outputs
        .iter()
        .find(|output| output["call_id"] == "call-1")
        .unwrap()
        .to_string();
    assert!(
        control.contains("KRATOS_READ_CONTROL_4baf2e"),
        "the positive read control did not execute: {control}"
    );
    let denied = outputs
        .iter()
        .find(|output| output["call_id"] == "call-2")
        .unwrap()
        .to_string();
    let denial = denied.to_lowercase();
    assert!(
        denial.contains("denied")
            || denial.contains("not permitted")
            || denial.contains("read-only"),
        "the write must produce an actual sandbox denial: {denied}"
    );
    assert!(
        !forbidden.exists(),
        "the plan-mode sandbox allowed the forbidden write"
    );
    harness.stop().await;
}
