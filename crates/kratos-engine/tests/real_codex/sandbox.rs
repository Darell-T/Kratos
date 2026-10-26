use fake_agents::responses::Reply;
use kratos_protocol::{Permission, ThreadSettings};

use crate::support::Harness;

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
