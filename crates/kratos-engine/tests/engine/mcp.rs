//! The `kratos-engine mcp` server Codex starts for each thread: the tools it offers, and what it
//! refuses an agent so that no tool gives it more than its own permission.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use kratos_client::ApiRequest;
use kratos_engine::server::ACTING_THREAD_HEADER;
use kratos_protocol::{Permission, Thread, ThreadSettings, Workspace, WorkspaceId};
use serde_json::{Value, json};

use crate::running_engine::{RunningEngine, WAIT};

struct Mcp {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(engine: &RunningEngine, thread: &Thread) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kratos-engine"))
            .arg("mcp")
            .env("KRATOS_HOME", engine.home())
            .env("KRATOS_PORT", engine.port().to_string())
            .env("KRATOS_THREAD_ID", thread.id.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Mcp {
            child,
            input,
            output,
            next_id: 0,
        };
        let started = mcp.request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" } }),
        );
        assert_eq!(started["serverInfo"]["name"], "kratos");
        assert_eq!(started["protocolVersion"], "2025-06-18");
        writeln!(
            mcp.input,
            "{}",
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .unwrap();
        mcp
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = json!(self.next_id);
        self.write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.answer_to(&id)["result"].clone()
    }

    fn write(&mut self, message: Value) {
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
    }

    /// The server's answer to request `id`, skipping answers to other requests.
    fn answer_to(&mut self, id: &Value) -> Value {
        let mut line = String::new();
        loop {
            line.clear();
            assert!(
                self.output.read_line(&mut line).unwrap() > 0,
                "the server closed"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == *id {
                return message;
            }
        }
    }

    fn tool(&mut self, name: &str, arguments: Value) -> (Value, bool) {
        let result = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let text = result["content"][0]["text"].as_str().unwrap();
        let answer = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
        (answer, result["isError"] == true)
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn workspace(engine: &RunningEngine) -> WorkspaceId {
    let workspace: Workspace = engine
        .client()
        .post(
            "/api/workspaces",
            &json!({ "name": "repo", "path": engine.home() }),
        )
        .unwrap()
        .parse()
        .unwrap();
    workspace.id
}

fn thread(
    engine: &RunningEngine,
    workspace: &WorkspaceId,
    provider: &str,
    permission: Permission,
) -> Thread {
    let settings = ThreadSettings {
        permission,
        ..ThreadSettings::default()
    };
    engine
        .client()
        .post(
            "/api/threads",
            &json!({ "workspaceId": workspace, "title": "chat", "provider": provider,
                     "settings": settings }),
        )
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn the_server_lists_kratos_tools_and_speaks_for_its_thread() {
    // Catches a server Codex cannot list tools from, one that does not know which thread's agent
    // it serves, which every label and refusal depends on, and arguments of the wrong shape that
    // fall back to defaults instead of being refused.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let me = thread(&engine, &workspace, "claude", Permission::AskMe);
    let other = thread(&engine, &workspace, "claude", Permission::AskMe);
    let mut mcp = Mcp::start(&engine, &me);

    let tools = mcp.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "whoami",
            "list_workspaces",
            "list_threads",
            "get_thread",
            "read_thread",
            "create_thread",
            "send_message",
            "wait_for_turn",
            "stop_thread",
            "answer_question",
            "decide_approval",
        ]
    );
    let (whoami, refused) = mcp.tool("whoami", json!({}));
    assert!(!refused, "{whoami}");
    assert_eq!(whoami["id"], json!(me.id));
    let (listed, _) = mcp.tool("list_threads", json!({}));
    assert_eq!(listed.as_array().unwrap().len(), 2, "{listed}");
    let (waited, _) = mcp.tool("wait_for_turn", json!({ "threadId": other.id }));
    assert_eq!(waited["state"], "idle", "{waited}");
    for (name, arguments) in [
        (
            "wait_for_turn",
            json!({ "threadId": other.id, "timeoutSeconds": 0 }),
        ),
        ("read_thread", json!({ "threadId": other.id, "after": "5" })),
        ("create_thread", json!({ "title": "x", "planMode": "yes" })),
    ] {
        let (refusal, refused) = mcp.tool(name, arguments);
        assert!(
            refused && refusal.to_string().contains("Invalid arguments"),
            "{name}: {refusal}"
        );
    }
}

#[test]
fn a_long_wait_holds_up_no_other_call_and_ends_when_codex_cancels_it() {
    // Catches a server that answers one call at a time, so a wait of up to 30 minutes blocks
    // every Kratos tool the agent calls next, and one that keeps waiting after Codex cancels it.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let me = thread(&engine, &workspace, "codex", Permission::AskMe);
    let busy = thread(&engine, &workspace, "codex", Permission::AskMe);
    engine
        .client()
        .post(
            &format!("/api/threads/{}/messages", busy.id),
            &json!({ "text": "work" }),
        )
        .unwrap();
    engine.fake_codex_held("turn/start", 1);
    let mut mcp = Mcp::start(&engine, &me);
    let wait = json!("wait");

    mcp.write(
        json!({ "jsonrpc": "2.0", "id": wait, "method": "tools/call",
                      "params": { "name": "wait_for_turn",
                                  "arguments": { "threadId": busy.id, "timeoutSeconds": 600 } } }),
    );
    let asked = Instant::now();
    let (whoami, refused) = mcp.tool("whoami", json!({}));
    assert!(
        !refused && asked.elapsed() < Duration::from_secs(10),
        "{whoami}"
    );
    mcp.write(
        json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
                      "params": { "requestId": wait } }),
    );

    let cancelled = mcp.answer_to(&wait);
    assert_eq!(cancelled["result"]["isError"], true, "{cancelled}");
    assert_eq!(cancelled["result"]["content"][0]["text"], "Cancelled");
    assert!(asked.elapsed() < Duration::from_secs(20));
}

#[test]
fn an_agent_messages_another_thread_under_its_own_name_but_never_itself() {
    // Catches a message the receiving agent cannot trace to its sender, one that records the
    // receiver's permission as its limit instead of the sender's, a label stored as if the sender
    // had written it, and an agent that queues work for itself, which would loop.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let sender = thread(&engine, &workspace, "codex", Permission::ApproveForMe);
    let receiver = thread(&engine, &workspace, "codex", Permission::FullAccess);
    let mut mcp = Mcp::start(&engine, &sender);

    let (sent, refused) = mcp.tool(
        "send_message",
        json!({ "threadId": receiver.id, "text": "Please review src/lib.rs." }),
    );
    assert!(!refused, "{sent}");
    assert_eq!(
        (&sent["by"], &sent["threadId"], &sent["limit"]),
        (
            &json!("thread"),
            &json!(sender.id),
            &json!({ "permission": "approveForMe", "planMode": false })
        ),
        "{sent}"
    );
    assert_eq!(
        sent["input"],
        json!([{ "type": "text", "text": "Please review src/lib.rs." }])
    );
    let started = &engine.fake_codex_held("turn/start", 1)[0]["params"]["input"];
    let label = started[0]["text"].as_str().unwrap();
    assert!(
        label.starts_with(&format!("[Message from Kratos thread {}.", sender.id)),
        "{label}"
    );
    assert!(
        label.contains("no more than") && label.contains("Approve for me"),
        "the receiving agent learns why it may be able to do less: {label}"
    );
    assert_eq!(started[1]["text"], "Please review src/lib.rs.");
    let (read, _) = mcp.tool("read_thread", json!({ "threadId": receiver.id }));
    let transcript = read["transcript"].as_str().unwrap();
    assert!(
        transcript.contains(&format!("thread {}: Please review src/lib.rs.", sender.id)),
        "{transcript}"
    );

    let (to_self, refused) = mcp.tool(
        "send_message",
        json!({ "threadId": sender.id, "text": "loop" }),
    );
    assert!(
        refused && to_self.to_string().contains("your own thread"),
        "{to_self}"
    );
    let (own, _) = mcp.tool("read_thread", json!({ "threadId": sender.id }));
    assert!(!own["transcript"].as_str().unwrap().contains("loop"));
}

#[test]
fn a_thread_another_thread_lowered_cannot_create_its_way_back_to_full_access() {
    // Catches a Full access thread lowered by an Approve for me sender's message that creates a
    // Full access thread, which would hand the sender's request to an agent with no sandbox.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let sender = thread(&engine, &workspace, "codex", Permission::ApproveForMe);
    let receiver = thread(&engine, &workspace, "codex", Permission::FullAccess);

    let (sent, refused) = Mcp::start(&engine, &sender).tool(
        "send_message",
        json!({ "threadId": receiver.id, "text": "Run the tests." }),
    );
    assert!(!refused, "{sent}");
    engine.fake_codex_held("turn/start", 1);

    let details = engine
        .client()
        .get(&format!("/api/threads/{}", receiver.id))
        .unwrap();
    assert_eq!(details.body["settings"]["permission"], "approveForMe");
    let (wider, refused) = Mcp::start(&engine, &receiver).tool(
        "create_thread",
        json!({ "title": "escape", "permission": "fullAccess" }),
    );
    assert!(
        refused && wider.to_string().contains("more permission"),
        "{wider}"
    );
}

#[test]
fn the_engine_acts_for_a_thread_only_on_another_thread_that_exists() {
    // Catches a label naming a thread that does not exist, a client that makes a thread queue work
    // for itself or stop its own turn, and a message to a thread with no agent that drops its
    // sender and logs it anyway.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let receiver = thread(&engine, &workspace, "codex", Permission::AskMe);
    let log_only = thread(&engine, &workspace, "claude", Permission::AskMe);
    let messages = |thread: &Thread| format!("/api/threads/{}/messages", thread.id);

    for (path, acting, status, reason) in [
        (
            messages(&receiver),
            receiver.id.as_str(),
            403,
            "your own thread",
        ),
        (messages(&receiver), "nope", 400, "no such thread"),
        (messages(&log_only), receiver.id.as_str(), 400, "no agent"),
        (
            format!("/api/threads/{}/stop", receiver.id),
            receiver.id.as_str(),
            403,
            "your own thread",
        ),
    ] {
        let request = ApiRequest::new("POST", &path)
            .header(ACTING_THREAD_HEADER, acting)
            .header("Content-Type", "application/json")
            .body(br#"{"text":"hi"}"#.to_vec());
        let reply = engine.client().send(&request).unwrap();
        let error = reply.body["error"].as_str().unwrap_or_default();
        assert!(
            reply.status == status && error.contains(reason),
            "{path} as {acting}: {} {}",
            reply.status,
            reply.body
        );
    }
    for thread in [&receiver, &log_only] {
        let events = engine
            .client()
            .get(&format!("/api/threads/{}/events", thread.id))
            .unwrap();
        assert_eq!(events.body, json!([]), "nothing joined {}", thread.id);
    }
}

#[test]
fn a_new_thread_never_gets_more_permission_than_the_agent_that_creates_it() {
    // Catches an Ask me agent that escapes its sandbox by creating a Full access thread and
    // messaging it.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let creator = thread(&engine, &workspace, "codex", Permission::AskMe);
    let mut mcp = Mcp::start(&engine, &creator);

    let (wider, refused) = mcp.tool(
        "create_thread",
        json!({ "title": "worker", "permission": "fullAccess" }),
    );
    assert!(
        refused && wider.to_string().contains("more permission"),
        "{wider}"
    );
    let (created, refused) = mcp.tool("create_thread", json!({ "title": "worker" }));
    assert!(!refused, "{created}");
    assert_eq!(created["workspaceId"], json!(workspace));
    let details = engine
        .client()
        .get(&format!("/api/threads/{}", created["id"].as_str().unwrap()))
        .unwrap();
    assert_eq!(details.body["settings"]["permission"], "askMe");
}

#[test]
fn only_a_full_access_agent_decides_another_thread_s_approval() {
    // Catches a sandboxed agent that approves what its own permission would have to ask about,
    // and an agent that approves its own request.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let worker = thread(&engine, &workspace, "codex", Permission::FullAccess);
    let asker = thread(&engine, &workspace, "codex", Permission::AskMe);
    let boss = thread(&engine, &workspace, "codex", Permission::FullAccess);
    engine
        .client()
        .post(
            &format!("/api/threads/{}/messages", worker.id),
            &json!({ "text": "delete the build folder" }),
        )
        .unwrap();
    engine.fake_codex_held("turn/start", 1);
    engine.fake_codex(
        "test/settleTurn",
        json!({ "method": "turn/start",
                "result": { "turn": { "id": "turn-1", "items": [], "status": "inProgress" } } }),
    );
    engine.fake_codex(
        "test/ask",
        json!({ "requestId": "ask-1", "method": "item/commandExecution/requestApproval",
                "params": { "threadId": "thread-A", "turnId": "turn-1", "itemId": "c1",
                            "startedAtMs": 0, "command": "rm -rf build" } }),
    );
    let asked_by = std::time::Instant::now() + WAIT;
    let approval = loop {
        let details = engine
            .client()
            .get(&format!("/api/threads/{}", worker.id))
            .unwrap();
        if let Some(approval) = details.body["approvals"][0]["id"].as_str() {
            break approval.to_owned();
        }
        assert!(
            std::time::Instant::now() < asked_by,
            "no approval: {}",
            details.body
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let decide = |mcp: &mut Mcp| {
        mcp.tool(
            "decide_approval",
            json!({ "approvalId": approval, "decision": "accept" }),
        )
    };

    let (answer, refused) = decide(&mut Mcp::start(&engine, &asker));
    assert!(
        refused && answer.to_string().contains("Only a Full access"),
        "{answer}"
    );
    let (answer, refused) = decide(&mut Mcp::start(&engine, &worker));
    assert!(
        refused && answer.to_string().contains("your own thread"),
        "{answer}"
    );
    let (answer, refused) = decide(&mut Mcp::start(&engine, &boss));
    assert!(!refused, "{answer}");
    let answers = engine.fake_codex_answers(1);
    assert_eq!(answers[0]["result"], json!({ "decision": "accept" }));
}
