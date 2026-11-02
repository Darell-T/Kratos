//! The `kratos-engine mcp` server Codex starts for each thread: the tools it offers, and what it
//! refuses an agent so that no tool gives it more than its own permission.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use fake_agents::codex_control;
use kratos_api_client::{ApiRequest, ClientError};
use kratos_protocol::{
    ACTING_THREAD_HEADER, CreateThread, Permission, Provider, RequiredText, Thread, ThreadSettings,
    WorkspaceId,
};
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
    engine.create_workspace().id
}

fn thread(
    engine: &RunningEngine,
    workspace: &WorkspaceId,
    provider: Provider,
    permission: Permission,
) -> Thread {
    let request = CreateThread {
        workspace_id: workspace.clone(),
        title: RequiredText::new("chat").unwrap(),
        provider,
        settings: Some(ThreadSettings {
            permission,
            ..ThreadSettings::default()
        }),
    };
    engine.client().create_thread(&request).unwrap()
}

#[test]
fn the_server_lists_kratos_tools_and_speaks_for_its_thread() {
    // Catches a server Codex cannot list tools from, one that does not know which thread's agent
    // it serves, which every label and refusal depends on, and arguments of the wrong shape that
    // fall back to defaults instead of being refused.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let me = thread(&engine, &workspace, Provider::Claude, Permission::AskMe);
    let other = thread(&engine, &workspace, Provider::Claude, Permission::AskMe);
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
    let me = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
    let busy = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
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
    // Catches a label whose wording reaches Codex mangled, a message the receiving agent cannot
    // trace to its sender, one that records the receiver's permission as its limit instead of the
    // sender's, a label stored as if the sender had written it, and an agent that queues work for
    // itself, which would loop.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let sender = thread(
        &engine,
        &workspace,
        Provider::Codex,
        Permission::ApproveForMe,
    );
    let receiver = thread(&engine, &workspace, Provider::Codex, Permission::FullAccess);
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
    assert_eq!(
        started[0]["text"],
        format!(
            "[Message from Kratos thread {}. From this turn on, this thread runs with no more than \
             that thread's permission, Approve for me, until the user raises it. If that stops you \
             from doing something, say so in your reply instead of working around it. Answer in \
             your reply, which that thread reads. Message it back only if it asks you to.]",
            sender.id
        ),
        "the receiving agent learns who sent the message and why it may be able to do less"
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
    let sender = thread(
        &engine,
        &workspace,
        Provider::Codex,
        Permission::ApproveForMe,
    );
    let receiver = thread(&engine, &workspace, Provider::Codex, Permission::FullAccess);

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
    let receiver = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
    let log_only = thread(&engine, &workspace, Provider::Claude, Permission::AskMe);
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
fn a_request_naming_a_thread_reaches_only_the_routes_kratos_s_tools_call() {
    // Catches a route that runs an agent's request with the user's power because its handler never
    // reads Kratos-Thread, such as a settings change that raises a thread to Full access, and a
    // refusal that comes only after the route has acted.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let workspace = workspace(&engine);
    let agent = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
    let target = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
    let target_path = format!("/api/threads/{}", target.id);
    let send = |body: Value| client.post(&format!("{target_path}/messages"), &body);
    send(json!({ "text": "work" })).unwrap();
    engine.fake_codex_held("turn/start", 1);
    send(json!({ "messageId": "queued", "text": "later" })).unwrap();
    let other_folder = home.path().join("other");
    std::fs::create_dir(&other_folder).unwrap();
    let state = || {
        let target = client.get(&target_path).unwrap().body;
        (target, client.get("/api/workspaces").unwrap().body)
    };
    let before = state();
    let queued = format!("{target_path}/messages/queued");
    let full_access = ThreadSettings {
        permission: Permission::FullAccess,
        ..ThreadSettings::default()
    };

    let reached: Vec<String> = [
        ("GET", "/api/health".to_owned(), json!(null)),
        (
            "POST",
            "/api/workspaces".to_owned(),
            json!({ "name": "other", "path": other_folder }),
        ),
        ("DELETE", queued.clone(), json!(null)),
        ("POST", format!("{queued}/steer"), json!(null)),
        ("POST", format!("{queued}/retry"), json!(null)),
        ("POST", format!("{queued}/discard"), json!(null)),
        ("POST", format!("{target_path}/resume"), json!(null)),
        ("PUT", format!("{target_path}/settings"), json!(full_access)),
        ("GET", format!("{target_path}/skills"), json!(null)),
        (
            "POST",
            "/api/continuations/any/schedule".to_owned(),
            json!(null),
        ),
        ("GET", "/api/usage".to_owned(), json!(null)),
    ]
    .into_iter()
    .filter_map(|(method, path, body)| {
        let mut request =
            ApiRequest::new(method, &path).header(ACTING_THREAD_HEADER, agent.id.as_str());
        if !body.is_null() {
            request = request
                .header("Content-Type", "application/json")
                .body(body.to_string().into_bytes());
        }
        let reply = client.send(&request).unwrap();
        (reply.status != 403).then(|| format!("{method} {path}: {} {}", reply.status, reply.body))
    })
    .collect();
    let socket = client.connect_events(&[(ACTING_THREAD_HEADER, agent.id.as_str())]);

    assert!(
        reached.is_empty(),
        "an agent's request reached {reached:#?}"
    );
    assert!(
        matches!(socket, Err(ClientError::Handshake(403))),
        "the event socket"
    );
    assert_eq!(
        state(),
        before,
        "a refused request changed a thread or the workspaces"
    );
}

#[test]
fn a_new_thread_never_gets_more_permission_than_the_agent_that_creates_it() {
    // Catches an Ask me agent that escapes its sandbox by creating a Full access thread and
    // messaging it.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let creator = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
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
    let worker = thread(&engine, &workspace, Provider::Codex, Permission::FullAccess);
    let asker = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
    let boss = thread(&engine, &workspace, Provider::Codex, Permission::FullAccess);
    engine
        .client()
        .post(
            &format!("/api/threads/{}/messages", worker.id),
            &json!({ "text": "delete the build folder" }),
        )
        .unwrap();
    engine.start_turn();
    let asked = codex_control::ask(
        Some("ask-1".into()),
        "item/commandExecution/requestApproval",
        json!({ "threadId": "thread-A", "turnId": "turn-1", "itemId": "c1", "startedAtMs": 0,
                "command": "rm -rf build" }),
    );
    engine.fake_codex_request(asked);
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

#[test]
fn a_next_turn_upgrade_does_not_widen_mcp_child_permissions() {
    // Catches next-turn settings granting tools more access during a starting or completed turn.
    for (permission, plan_mode) in [(Permission::AskMe, false), (Permission::FullAccess, true)] {
        let home = tempfile::tempdir().unwrap();
        let engine = RunningEngine::start(home.path());
        let workspace = workspace(&engine);
        let me = thread(&engine, &workspace, Provider::Codex, permission);
        let path = format!("/api/threads/{}", me.id);
        let initial = ThreadSettings {
            permission,
            plan_mode,
            ..ThreadSettings::default()
        };
        engine
            .client()
            .put(&format!("{path}/settings"), &initial)
            .unwrap()
            .parse::<Value>()
            .unwrap();
        engine
            .client()
            .post(&format!("{path}/messages"), &json!({ "text": "first" }))
            .unwrap();
        engine.fake_codex_held("turn/start", 1);
        let full = ThreadSettings {
            permission: Permission::FullAccess,
            ..ThreadSettings::default()
        };
        engine
            .client()
            .put(&format!("{path}/settings"), &full)
            .unwrap()
            .parse::<Value>()
            .unwrap();
        let mut mcp = Mcp::start(&engine, &me);

        let (child, refused) = mcp.tool(
            "create_thread",
            json!({ "title": "worker", "permission": "fullAccess", "planMode": false }),
        );
        assert!(
            refused,
            "starting {permission:?}, plan {plan_mode}: {child}"
        );
        let (child, refused) = mcp.tool(
            "create_thread",
            json!({ "title": "worker", "permission": permission, "planMode": plan_mode }),
        );
        assert!(
            !refused,
            "a child at the current permission is allowed: {child}"
        );
        let (child, refused) = mcp.tool("create_thread", json!({ "title": "default worker" }));
        assert!(
            !refused,
            "default child settings use the current turn: {child}"
        );

        engine.start_turn();
        let deadline = Instant::now() + WAIT;
        while engine.client().get(&path).unwrap().body["runningTurn"].is_null() {
            assert!(Instant::now() < deadline, "the turn never started");
            std::thread::yield_now();
        }
        engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
        while !engine.client().get(&path).unwrap().body["runningTurn"].is_null() {
            assert!(Instant::now() < deadline, "the turn never ended");
            std::thread::yield_now();
        }
        let (child, refused) = mcp.tool(
            "create_thread",
            json!({ "title": "worker", "permission": "fullAccess", "planMode": false }),
        );
        assert!(
            refused,
            "a late tool keeps the completed turn's permission: {child}"
        );

        engine
            .client()
            .post(&format!("{path}/messages"), &json!({ "text": "second" }))
            .unwrap();
        engine.fake_codex_held("turn/start", 2);
        let (child, refused) = mcp.tool("create_thread", json!({ "title": "worker" }));
        assert!(!refused, "the next turn gets the saved permission: {child}");
    }
}

#[test]
fn a_next_turn_upgrade_does_not_widen_a_running_senders_message_limit() {
    // Catches an Ask me turn labelling its delegated message with a future Full access limit.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let me = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
    let receiver = thread(&engine, &workspace, Provider::Codex, Permission::FullAccess);
    let path = format!("/api/threads/{}", me.id);
    engine
        .client()
        .post(&format!("{path}/messages"), &json!({ "text": "first" }))
        .unwrap();
    engine.start_turn();
    let full = ThreadSettings {
        permission: Permission::FullAccess,
        ..ThreadSettings::default()
    };
    engine
        .client()
        .put(&format!("{path}/settings"), &full)
        .unwrap()
        .parse::<Value>()
        .unwrap();

    let (sent, refused) = Mcp::start(&engine, &me).tool(
        "send_message",
        json!({ "threadId": receiver.id, "text": "review" }),
    );
    assert!(!refused, "{sent}");
    assert_eq!(
        sent["limit"],
        json!({ "permission": "askMe", "planMode": false })
    );
    let starts = engine.fake_codex_held("turn/start", 2);
    assert_eq!(starts[1]["params"]["permissions"], "kratos-ask");
    let details = engine
        .client()
        .get(&format!("/api/threads/{}", receiver.id))
        .unwrap();
    assert_eq!(details.body["settings"]["permission"], "askMe");
}

#[test]
fn a_next_turn_upgrade_does_not_authorize_peer_approval_decisions() {
    // Catches an active restricted turn approving a peer's request using saved future settings.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let worker = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
    let me = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
    engine
        .client()
        .post(
            &format!("/api/threads/{}/messages", worker.id),
            &json!({ "text": "work" }),
        )
        .unwrap();
    engine.start_turn();
    engine.fake_codex_request(codex_control::ask(
        Some("peer-approval".into()),
        "item/commandExecution/requestApproval",
        json!({ "threadId": "thread-A", "turnId": "turn-1", "itemId": "c1",
                "startedAtMs": 0, "command": "cargo test" }),
    ));
    let deadline = Instant::now() + WAIT;
    let approval = loop {
        let details = engine
            .client()
            .get(&format!("/api/threads/{}", worker.id))
            .unwrap();
        if let Some(id) = details.body["approvals"][0]["id"].as_str() {
            break id.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "the peer did not ask for approval"
        );
        std::thread::yield_now();
    };
    let path = format!("/api/threads/{}", me.id);
    engine
        .client()
        .post(&format!("{path}/messages"), &json!({ "text": "first" }))
        .unwrap();
    engine.fake_codex_held("turn/start", 2);
    let full = ThreadSettings {
        permission: Permission::FullAccess,
        ..ThreadSettings::default()
    };
    engine
        .client()
        .put(&format!("{path}/settings"), &full)
        .unwrap()
        .parse::<Value>()
        .unwrap();
    let mut mcp = Mcp::start(&engine, &me);

    let (answer, refused) = mcp.tool(
        "decide_approval",
        json!({ "approvalId": approval, "decision": "accept" }),
    );
    assert!(refused, "the current turn must stay restricted: {answer}");
    let details = engine
        .client()
        .get(&format!("/api/threads/{}", worker.id))
        .unwrap();
    assert_eq!(
        details.body["approvals"][0]["id"], approval,
        "a refused decision leaves the approval pending"
    );
}

#[test]
fn mcp_permissions_follow_idle_settings_until_the_first_turn_then_keep_the_turns_settings() {
    // Catches freezing an unused thread's settings, or applying a queued downgrade to active tools.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let me = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
    let path = format!("/api/threads/{}", me.id);
    let full = ThreadSettings {
        permission: Permission::FullAccess,
        ..ThreadSettings::default()
    };
    engine
        .client()
        .put(&format!("{path}/settings"), &full)
        .unwrap()
        .parse::<Value>()
        .unwrap();
    let mut mcp = Mcp::start(&engine, &me);
    let child = json!({ "title": "worker", "permission": "fullAccess", "planMode": false });
    let (created, refused) = mcp.tool("create_thread", child.clone());
    assert!(!refused, "settings apply before any turn: {created}");

    engine
        .client()
        .post(&format!("{path}/messages"), &json!({ "text": "work" }))
        .unwrap();
    engine.fake_codex_held("turn/start", 1);
    engine
        .client()
        .put(&format!("{path}/settings"), &ThreadSettings::default())
        .unwrap()
        .parse::<Value>()
        .unwrap();
    let (created, refused) = mcp.tool("create_thread", child);
    assert!(
        !refused,
        "the running Full access turn keeps its permission: {created}"
    );
    assert_eq!(
        engine.client().get(&path).unwrap().body["settings"]["permission"],
        "askMe",
        "the user still sees next-turn settings"
    );
}

#[test]
fn an_old_mcp_connection_cannot_gain_future_permission_after_an_engine_restart() {
    // Catches an old agent reconnecting with a new token before a turn grants its saved upgrade.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let workspace = workspace(&engine);
    let me = thread(&engine, &workspace, Provider::Codex, Permission::AskMe);
    let path = format!("/api/threads/{}", me.id);
    engine
        .client()
        .post(&format!("{path}/messages"), &json!({ "text": "work" }))
        .unwrap();
    engine.start_turn();
    let deadline = Instant::now() + WAIT;
    while engine.client().get(&path).unwrap().body["runningTurn"].is_null() {
        assert!(Instant::now() < deadline, "the turn never started");
        std::thread::yield_now();
    }
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
    while !engine.client().get(&path).unwrap().body["runningTurn"].is_null() {
        assert!(Instant::now() < deadline, "the turn never ended");
        std::thread::yield_now();
    }
    let full = ThreadSettings {
        permission: Permission::FullAccess,
        ..ThreadSettings::default()
    };
    engine
        .client()
        .put(&format!("{path}/settings"), &full)
        .unwrap()
        .parse::<Value>()
        .unwrap();
    let mut old_mcp = Mcp::start(&engine, &me);
    let port = engine.port();
    engine.kill();
    let engine = RunningEngine::start_on(home.path(), port);
    let (created, refused) = old_mcp.tool(
        "create_thread",
        json!({ "title": "worker", "permission": "fullAccess" }),
    );
    assert!(
        refused,
        "the old MCP connection must be restarted: {created}"
    );
    let (whoami, refused) = Mcp::start(&engine, &me).tool("whoami", json!({}));
    assert!(
        !refused,
        "a new MCP connection can read the thread: {whoami}"
    );
}

#[test]
fn a_question_answer_cannot_exceed_the_senders_or_the_receivers_turn_permission() {
    // Catches question answers bypassing the sender limit, or using the receiver's future settings.
    for (sender_permission, sender_plans, receiver_permission, next_permission, refused) in [
        (
            Permission::AskMe,
            false,
            Permission::FullAccess,
            Permission::AskMe,
            true,
        ),
        (
            Permission::FullAccess,
            true,
            Permission::FullAccess,
            Permission::FullAccess,
            true,
        ),
        (
            Permission::AskMe,
            false,
            Permission::AskMe,
            Permission::FullAccess,
            false,
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        let engine = RunningEngine::start(home.path());
        let workspace = workspace(&engine);
        let receiver = thread(&engine, &workspace, Provider::Codex, receiver_permission);
        let sender = thread(&engine, &workspace, Provider::Codex, sender_permission);
        let sender_settings = ThreadSettings {
            permission: sender_permission,
            plan_mode: sender_plans,
            ..ThreadSettings::default()
        };
        engine
            .client()
            .put(
                &format!("/api/threads/{}/settings", sender.id),
                &sender_settings,
            )
            .unwrap()
            .parse::<Value>()
            .unwrap();
        let path = format!("/api/threads/{}", receiver.id);
        engine
            .client()
            .post(&format!("{path}/messages"), &json!({ "text": "work" }))
            .unwrap();
        engine.start_turn();
        let next = ThreadSettings {
            permission: next_permission,
            ..ThreadSettings::default()
        };
        engine
            .client()
            .put(&format!("{path}/settings"), &next)
            .unwrap()
            .parse::<Value>()
            .unwrap();
        engine.fake_codex_request(codex_control::ask(
            Some("question-1".into()),
            "item/tool/requestUserInput",
            json!({ "threadId": "thread-A", "turnId": "turn-1", "itemId": "q1", "isBlocking": false,
                    "questions": [{ "id": "color", "header": "Color", "question": "Which color?",
                                    "options": [{ "label": "Red", "description": "Red" }] }] }),
        ));
        let deadline = Instant::now() + WAIT;
        let question = loop {
            let details = engine.client().get(&path).unwrap();
            if let Some(id) = details.body["questions"][0]["id"].as_str() {
                break id.to_owned();
            }
            assert!(Instant::now() < deadline, "the question did not arrive");
            std::thread::yield_now();
        };

        let (answer, was_refused) = Mcp::start(&engine, &sender).tool(
            "answer_question",
            json!({ "questionId": question, "answers": { "color": ["Red"] } }),
        );
        assert_eq!(
            was_refused, refused,
            "sender {:?}, plans {}, receiver {:?}: {}",
            sender_permission, sender_plans, receiver_permission, answer
        );
        if refused {
            let details = engine.client().get(&path).unwrap();
            assert_eq!(
                details.body["questions"][0]["id"], question,
                "a refused answer leaves the question pending"
            );
            let answered = engine
                .client()
                .post(
                    &format!("/api/questions/{question}"),
                    &json!({ "answers": { "color": ["Blue"] } }),
                )
                .unwrap();
            assert_eq!(answered.status, 200, "the user can still answer");
        }
        let answers = engine.fake_codex_answers(1);
        let expected = if refused { "Blue" } else { "Red" };
        assert_eq!(
            answers[0]["result"]["answers"]["color"]["answers"],
            json!([expected])
        );
    }
}
