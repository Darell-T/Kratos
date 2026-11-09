//! D99 through fake Claude, the production MCP path, and the pinned real Codex sandbox.

use std::time::Duration;

use kratos_protocol::CommandResult;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::support::{Harness, WAIT};

async fn fake(port: u16, method: &str, params: Value) -> Value {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(
            format!(
                "{}\n",
                json!({ "id": 1, "method": method, "params": params })
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(WAIT, BufReader::new(reader).read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str::<Value>(&line).unwrap()["result"].clone()
}

async fn port(harness: &Harness) -> u16 {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Ok(log) = std::fs::read_to_string(harness.root.path().join("fake-claude.log"))
            && let Some(row) = log.lines().next()
        {
            return serde_json::from_str::<Value>(row).unwrap()["port"]
                .as_u64()
                .unwrap()
                .try_into()
                .unwrap();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fake Claude never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn run(
    harness: &Harness,
    port: u16,
    thread_id: &str,
    id: &str,
    command: &str,
) -> CommandResult {
    fake(port, "test/ask", json!({ "requestId": id, "request": {
        "subtype": "mcp_message", "server_name": "kratos", "message": {
            "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
                "name": "run_command", "arguments": { "commandLine": command, "timeoutMs": 60000 }
            }
        }
    }})).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    let approval = loop {
        let details = harness
            .client
            .get(&format!("/api/threads/{thread_id}"))
            .unwrap();
        let approvals = details.body["approvals"]
            .as_array()
            .expect("thread details contain approvals");
        if let Some(approval) = approvals.first() {
            break approval["id"].as_str().unwrap().to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "command never reached the user for approval"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let approved = harness
        .client
        .post(
            &format!("/api/approvals/{approval}"),
            &json!({ "decision": "accept" }),
        )
        .unwrap();
    assert_eq!(approved.status, 200, "{}", approved.body);
    loop {
        let received = fake(port, "test/received", json!({})).await;
        if let Some(frame) = received.as_array().unwrap().iter().find(|frame| {
            frame["type"] == "control_response" && frame["response"]["request_id"] == id
        }) {
            let body = &frame["response"]["response"]["mcp_response"]["result"];
            let text = body["content"][0]["text"]
                .as_str()
                .expect("MCP result contains command output");
            return serde_json::from_str(text).unwrap();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "real command never returned to Claude"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires pinned Codex 0.159.1 and a provisioned Windows sandbox home; CI runs it"]
async fn a_claude_mcp_command_runs_in_the_real_sandbox_and_cannot_write_outside_its_root() {
    // Catches a fake-only permission check, an unsandboxed Claude command, and a refusal caused only by setup failure.
    let harness = Harness::start(kratos_protocol::ThreadSettings::default(), vec![]).await;
    std::fs::write(
        harness.folder().join("powershell.exe"),
        "KRATOS_UNTRUSTED_SHELL",
    )
    .unwrap();
    let created = harness.client.post("/api/threads", &json!({ "workspaceId": harness.thread.workspace_id,
        "title": "Claude sandbox proof", "provider": "claude", "settings": kratos_protocol::ThreadSettings::default() })).unwrap();
    assert_eq!(created.status, 201, "{}", created.body);
    let thread_id = created.body["id"].as_str().unwrap();
    let message_id = "00000000-0000-4000-8000-000000000001";
    let sent = harness
        .client
        .post(
            &format!("/api/threads/{thread_id}/messages"),
            &json!({ "messageId": message_id, "text": "Run the sandbox proof" }),
        )
        .unwrap();
    assert_eq!(sent.status, 201);
    let port = port(&harness).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let received = fake(port, "test/received", json!({})).await;
        if received
            .as_array()
            .unwrap()
            .iter()
            .any(|frame| frame["uuid"] == message_id)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "message was not written to Claude"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    fake(
        port,
        "test/emit",
        json!({ "message": { "type": "user", "isReplay": true, "uuid": message_id } }),
    )
    .await;
    let control = run(
        &harness,
        port,
        thread_id,
        "control",
        "Write-Output KRATOS_CLAUDE_SANDBOX_CONTROL",
    )
    .await;
    assert!(
        matches!(control, CommandResult::Exited { exit_code: 0, ref stdout, .. } if stdout.contains("KRATOS_CLAUDE_SANDBOX_CONTROL")),
        "harmless process must run successfully: {control:?}"
    );
    let target = harness.root.path().join("outside-claude-root.txt");
    assert!(!target.exists());
    let command = format!(
        "Set-Content -LiteralPath '{}' -Value blocked -ErrorAction Stop",
        target.display().to_string().replace('\'', "''")
    );
    let denied = run(&harness, port, thread_id, "denied", &command).await;
    assert!(
        matches!(
            denied,
            CommandResult::Refused { .. } | CommandResult::Exited { exit_code: 1.., .. }
        ),
        "sandbox must refuse the write: {denied:?}"
    );
    assert!(
        !target.exists(),
        "a prohibited file appeared despite the sandbox"
    );
    assert!(
        harness.model.requests().is_empty(),
        "no paid model may run in this test"
    );
    harness.stop().await;
}
