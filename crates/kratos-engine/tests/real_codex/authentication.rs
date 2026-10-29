use fake_agents::responses::Reply;
use kratos_client::Incoming;
use kratos_protocol::{
    Permission, Seq, ServerMessage, Thread, ThreadEventBody, ThreadSettings, TurnOutcome,
};
use serde_json::json;

use crate::support::{Harness, WAIT};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires pinned Codex 0.159.1 and a provisioned Windows sandbox home; CI runs it"]
async fn an_approve_for_me_agent_and_its_child_cannot_read_the_token_or_decide_an_approval() {
    let mut harness = Harness::start(
        ThreadSettings::default(),
        vec![Reply::command("Write-Output needs-user")],
    )
    .await;
    harness.send("approval-owner");
    let approval = loop {
        match harness.next_event().await {
            ThreadEventBody::ApprovalRequested { approval } => break approval,
            ThreadEventBody::ThreadPaused { reason } => panic!("provider paused: {reason:?}"),
            ThreadEventBody::TurnCompleted { outcome, .. } => {
                panic!("approval was bypassed: {outcome:?}")
            }
            _ => {}
        }
    };
    let attacker: Thread = harness.client.post("/api/threads", &json!({
        "workspaceId": harness.thread.workspace_id,
        "title": "API isolation probe", "provider": "codex",
        "settings": ThreadSettings { permission: Permission::ApproveForMe, ..ThreadSettings::default() }
    })).unwrap().parse().unwrap();
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
try {{ Get-Content -LiteralPath '..\kratos\client-token'; Write-Output 'TOKEN_WAS_READ' }}
catch {{ Write-Output 'TOKEN_BLOCKED' }}
$health = Invoke-WebRequest -UseBasicParsing -Uri '{url}/api/health'
Write-Output "HEALTH=$($health.StatusCode)"
try {{
  $result = Invoke-WebRequest -UseBasicParsing -Uri '{url}/api/approvals/{approval}' -Method Post -ContentType 'application/json' -Body '{{"decision":"accept"}}'
  Write-Output "API=$($result.StatusCode)"
}} catch {{ Write-Output "API=$([int]$_.Exception.Response.StatusCode)" }}
"#,
        url = harness.base_url,
        approval = approval.id
    );
    std::fs::write(harness.folder().join("api-probe.ps1"), script).unwrap();
    harness.model.enqueue([
        Reply::command("Get-Content -LiteralPath '..\\kratos\\client-token' -ErrorAction Stop"),
        Reply::command("powershell.exe -NoLogo -NoProfile -NonInteractive -File .\\api-probe.ps1"),
        Reply::Text("API isolation probe completed.".into()),
    ]);
    let mut events = harness.client.connect_events(&[]).unwrap();
    events.subscribe(&attacker.id, Seq::BEFORE_FIRST).unwrap();
    let sent = harness
        .client
        .post(
            &format!("/api/threads/{}/messages", attacker.id),
            &json!({"messageId": "attacker", "text": "Run the scripted probe."}),
        )
        .unwrap();
    assert_eq!(sent.status, 201, "{:?}", sent.body);
    tokio::task::spawn_blocking(move || {
        loop {
            let Some(Incoming::Text(text)) = events.next_incoming(WAIT).unwrap() else {
                panic!("no attacker event");
            };
            if let ServerMessage::Event { event } = serde_json::from_str(&text).unwrap() {
                match event.event {
                    ThreadEventBody::TurnCompleted { outcome, .. } => {
                        assert_eq!(outcome, TurnOutcome::Completed);
                        break;
                    }
                    ThreadEventBody::ThreadPaused { reason } => {
                        panic!("attacker paused: {reason:?}")
                    }
                    ThreadEventBody::ApprovalRequested { approval } => {
                        panic!("probe unexpectedly needs approval: {approval:?}")
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .unwrap();
    let outputs = harness.model.tool_outputs();
    assert_eq!(outputs.len(), 2, "both attacker tool calls must execute");
    let token = std::fs::read_to_string(harness.home().join("client-token")).unwrap();
    let output = serde_json::to_string(&outputs).unwrap();
    assert!(
        !output.contains(&token),
        "the agent learned the test client token"
    );
    assert!(
        !output.contains("TOKEN_WAS_READ"),
        "the child read the token"
    );
    assert!(
        output.contains("TOKEN_BLOCKED"),
        "child probe did not execute: {output}"
    );
    assert!(
        output.contains("HEALTH=200"),
        "the agent must actually reach the engine: {output}"
    );
    assert!(
        output.contains("API=401"),
        "the API must reject the agent: {output}"
    );
    let pending = harness
        .client
        .get(&format!("/api/threads/{}", harness.thread.id))
        .unwrap();
    assert_eq!(pending.body["approvals"][0]["id"], json!(approval.id));
    assert_eq!(pending.body["approvals"].as_array().unwrap().len(), 1);
    harness.stop().await;
}
