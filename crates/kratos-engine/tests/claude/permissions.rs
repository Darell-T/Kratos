//! Until Kratos's permission rules exist (slice 3), every request Claude makes is refused, and a
//! thread never launches in a mode that would skip asking.

use kratos_protocol::{Permission, ThreadSettings};
use serde_json::{Value, json};

use crate::harness::{A, Harness, Launch, Options};

/// Makes the fake send the provider `request` under `id`, and returns the answer.
async fn ask(harness: &Harness, launch: &Launch, id: &str, request: Value) -> Value {
    harness
        .call(
            launch,
            "test/ask",
            json!({ "requestId": id, "request": request }),
        )
        .await;
    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    loop {
        let answer = harness.received(launch).await.into_iter().find(|frame| {
            frame["type"] == "control_response" && frame["response"]["request_id"] == id
        });
        if let Some(answer) = answer {
            return answer["response"].clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{id} was never answered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

async fn running(options: Options) -> (Harness, Launch) {
    let harness = Harness::start(options).await;
    let launch = harness.begin(A, 1).await;
    (harness, launch)
}

#[tokio::test]
async fn every_tool_request_is_denied_with_the_reason() {
    // Catches a tool allowed, or a request left unanswered with Claude waiting, before Kratos's
    // permission rules exist.
    let (harness, launch) = running(Options::default()).await;

    let answer = ask(
        &harness,
        &launch,
        "tool-1",
        json!({ "subtype": "can_use_tool", "tool_name": "Write",
                "input": { "file_path": "outside.txt" }, "tool_use_id": "t1" }),
    )
    .await;

    assert_eq!(answer["subtype"], "success");
    assert_eq!(answer["response"]["behavior"], "deny");
    assert!(
        answer["response"]["message"]
            .as_str()
            .unwrap()
            .contains("not built yet")
    );
}

#[tokio::test]
async fn an_elicitation_is_declined_and_every_other_request_is_an_error() {
    // Catches an MCP server's question that is answered or ignored, a hook or MCP request Kratos
    // never registered that is accepted, and a request Kratos does not know that hangs Claude.
    let (harness, launch) = running(Options::default()).await;

    let elicitation = ask(
        &harness,
        &launch,
        "elicit-1",
        json!({ "subtype": "elicitation", "mcp_server_name": "files", "message": "Pick one" }),
    )
    .await;
    let hook = ask(
        &harness,
        &launch,
        "hook-1",
        json!({ "subtype": "hook_callback", "callback_id": "h1", "input": {} }),
    )
    .await;
    let mcp = ask(
        &harness,
        &launch,
        "mcp-1",
        json!({ "subtype": "mcp_message", "server_name": "kratos", "message": { "id": 1 } }),
    )
    .await;
    let unknown = ask(
        &harness,
        &launch,
        "new-1",
        json!({ "subtype": "something_new" }),
    )
    .await;

    assert_eq!(elicitation["response"], json!({ "action": "decline" }));
    for answer in [&hook, &mcp, &unknown] {
        assert_eq!(answer["subtype"], "error", "{answer}");
    }
}

#[tokio::test]
async fn the_session_registers_no_hooks_and_no_tool_servers() {
    // Catches hooks or SDK MCP servers declared in the handshake before Kratos can answer them.
    let (harness, launch) = running(Options::default()).await;

    let received = harness.received(&launch).await;

    let initialize = &received[0]["request"];
    assert_eq!(initialize, &json!({ "subtype": "initialize" }));
}

#[tokio::test]
async fn full_access_does_not_launch_a_mode_that_skips_the_ask() {
    // Catches a thread with full access launched in bypass mode, which would let Claude write
    // anywhere with no rule between it and the disk.
    let (harness, launch) = running(Options {
        settings: ThreadSettings {
            permission: Permission::FullAccess,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;

    assert_eq!(launch.value("--permission-mode"), Some("default"));
    assert!(!launch.has("--allow-dangerously-skip-permissions"));
    let answer = ask(
        &harness,
        &launch,
        "tool-1",
        json!({ "subtype": "can_use_tool", "tool_name": "Write", "input": {}, "tool_use_id": "t1" }),
    )
    .await;
    assert_eq!(answer["response"]["behavior"], "deny");
}
