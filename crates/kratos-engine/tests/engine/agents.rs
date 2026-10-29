//! The engine binary driving a Codex thread over its API: the client token (D19), a message whose
//! reply streams live, an approval decided over HTTP, and the account's usage.

use kratos_client::{ApiRequest, Client, ClientError};
use kratos_protocol::{LiveUpdate, ServerMessage, Thread, ThreadEventBody};
use serde_json::{Value, json};

use crate::running_engine::{RunningEngine, next_text, subscribed};
use crate::server::post_whole_body_then_read;

fn messages_path(thread: &Thread) -> String {
    format!("/api/threads/{}/messages", thread.id)
}

/// The next socket message about the thread that `wanted` picks, skipping the rest.
fn next_message(
    socket: &mut kratos_client::EventSocket,
    mut wanted: impl FnMut(&ServerMessage) -> bool,
) -> ServerMessage {
    loop {
        let message: ServerMessage = serde_json::from_str(&next_text(socket)).unwrap();
        if wanted(&message) {
            return message;
        }
    }
}

/// Settles the turn the fake holds for the message just sent as `turn-1`, now running.
fn start_turn(engine: &RunningEngine) {
    engine.fake_codex_held("turn/start", 1);
    engine.fake_codex(
        "test/settleTurn",
        json!({ "method": "turn/start",
                "result": { "turn": { "id": "turn-1", "items": [], "status": "inProgress" } } }),
    );
}

fn emit(engine: &RunningEngine, method: &str, params: Value) {
    engine.fake_codex("test/emit", json!({ "method": method, "params": params }));
}

#[test]
fn every_route_but_the_health_check_refuses_a_request_without_this_start_s_token() {
    // Catches an agent with network access reaching the API (D19): no token, a wrong one, or the
    // token of an earlier engine start all fail, and so does the socket handshake.
    let home = tempfile::tempdir().unwrap();
    let earlier = RunningEngine::start(home.path()).token();
    let engine = RunningEngine::start(home.path());
    let stale = ApiRequest::new("GET", "/api/workspaces")
        .header("Authorization", &format!("Bearer {earlier}"));
    let bare = Client::new(&format!("http://127.0.0.1:{}", engine.port())).unwrap();

    assert_eq!(bare.get("/api/health").unwrap().status, 200);
    for request in [
        ApiRequest::new("GET", "/api/workspaces"),
        ApiRequest::new("GET", "/api/usage"),
        ApiRequest::new("POST", "/api/approvals/any").body(br#"{"decision":"accept"}"#.to_vec()),
        ApiRequest::new("GET", "/api/workspaces").header("Authorization", "Bearer wrong"),
        stale,
    ] {
        let reply = bare.send(&request).unwrap();
        assert_eq!(
            (reply.status, reply.body),
            (401, json!({ "error": "Unauthorized" })),
            "{request:?}"
        );
    }
    assert!(matches!(
        bare.connect_events(&[]),
        Err(ClientError::Handshake(401))
    ));
    assert_eq!(engine.client().get("/api/workspaces").unwrap().status, 200);
    assert_ne!(engine.token(), earlier, "each start issues a new token");
}

#[test]
fn a_refused_upload_is_read_to_its_end_so_the_client_sees_the_401() {
    // Catches a refusal sent while the client still writes, which on Windows resets the
    // connection and hides the status.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());

    let reply = post_whole_body_then_read(engine.port(), "/api/workspaces", 8 * 1024 * 1024, None);

    assert!(reply.starts_with("HTTP/1.1 401 "), "{reply}");
}

#[test]
fn a_codex_message_streams_its_reply_live_and_its_end_reaches_the_log() {
    // Catches the reply missing from the socket while it is written (D88), and a client's retry
    // after a lost reply queuing the message twice (D85).
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let thread = engine.create_thread_of("codex");
    let mut socket = subscribed(&engine, &thread.id, 0);

    let sent = client
        .post(
            &messages_path(&thread),
            &json!({ "text": "hello", "messageId": "m1" }),
        )
        .unwrap();
    assert_eq!((sent.status, &sent.body["status"]), (201, &json!("queued")));
    start_turn(&engine);
    for delta in ["Hel", "lo"] {
        emit(
            &engine,
            "item/agentMessage/delta",
            json!({ "threadId": "thread-A", "turnId": "turn-1", "itemId": "r1", "delta": delta }),
        );
    }
    emit(
        &engine,
        "item/completed",
        json!({ "threadId": "thread-A", "turnId": "turn-1", "completedAtMs": 0,
                "item": { "type": "agentMessage", "id": "r1", "text": "Hello" } }),
    );
    emit(
        &engine,
        "turn/completed",
        json!({ "threadId": "thread-A", "turn": { "id": "turn-1", "items": [], "status": "completed" } }),
    );

    let mut streamed = String::new();
    while streamed != "Hello" {
        match next_message(&mut socket, |message| {
            matches!(message, ServerMessage::Live { .. })
        }) {
            ServerMessage::Live {
                update: LiveUpdate::ReplyDelta { delta, .. },
                ..
            } => streamed.push_str(&delta),
            other => panic!("expected a reply delta, got {other:?}"),
        }
    }
    let ended = next_message(
        &mut socket,
        |message| matches!(message, ServerMessage::Event { event } if matches!(event.event, ThreadEventBody::TurnCompleted { .. })),
    );
    assert!(matches!(ended, ServerMessage::Event { .. }));
    let again = client
        .post(
            &messages_path(&thread),
            &json!({ "text": "hello", "messageId": "m1" }),
        )
        .unwrap();
    assert_eq!(
        (again.status, &again.body["status"]),
        (200, &json!("accepted"))
    );
    let details = client.get(&format!("/api/threads/{}", thread.id)).unwrap();
    assert_eq!(details.body["queue"], json!([]));
}

#[test]
fn an_approval_reaches_the_socket_and_the_decision_sent_over_http_reaches_codex() {
    // Catches the D83 path broken between the API and Codex: the request never shown, or the
    // decision recorded but never answered.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let thread = engine.create_thread_of("codex");
    let mut socket = subscribed(&engine, &thread.id, 0);
    client
        .post(&messages_path(&thread), &json!({ "text": "list files" }))
        .unwrap();
    start_turn(&engine);

    engine.fake_codex(
        "test/ask",
        json!({ "requestId": "ask-1", "method": "item/commandExecution/requestApproval",
                "params": { "threadId": "thread-A", "turnId": "turn-1", "itemId": "c1",
                            "startedAtMs": 0, "command": "ls" } }),
    );
    let requested = next_message(
        &mut socket,
        |message| matches!(message, ServerMessage::Event { event } if matches!(event.event, ThreadEventBody::ApprovalRequested { .. })),
    );
    let ServerMessage::Event { event } = requested else {
        unreachable!("picked an event");
    };
    let ThreadEventBody::ApprovalRequested { approval } = event.event else {
        unreachable!("picked an approval request");
    };
    let decided = client
        .post(
            &format!("/api/approvals/{}", approval.id),
            &json!({ "decision": "accept" }),
        )
        .unwrap();

    assert_eq!(
        (decided.status, decided.body),
        (200, json!({ "state": "decided", "decision": "accept" }))
    );
    let answers = engine.fake_codex_answers(1);
    assert_eq!(answers[0]["result"], json!({ "decision": "accept" }));
    let unknown = client
        .post("/api/approvals/nope", &json!({ "decision": "accept" }))
        .unwrap();
    assert_eq!(unknown.status, 404);
}

#[test]
fn usage_reports_the_account_s_windows_with_their_resets() {
    // Catches a usage route that serves nothing, or drops the secondary window (D38).
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    client.get("/api/usage").unwrap();
    engine.fake_codex(
        "test/rateLimits",
        json!({ "accountId": "account-1", "ordinaryUsageAllowed": true,
                "rateLimits": { "limitId": "codex", "planType": "plus",
                    "primary": { "usedPercent": 20, "windowDurationMins": 300, "resetsAt": 4_102_444_800_i64 },
                    "secondary": { "usedPercent": 5, "windowDurationMins": 10080, "resetsAt": 4_102_963_200_i64 } } }),
    );

    let usage = client.get("/api/usage").unwrap();

    assert_eq!(usage.status, 200);
    let bucket = &usage.body["accounts"][0]["buckets"][0];
    assert_eq!(
        usage.body["accounts"][0]["allowance"],
        json!({ "state": "allowed" })
    );
    assert_eq!(bucket["primary"]["windowMinutes"], 300);
    assert_eq!(bucket["secondary"]["windowMinutes"], 10080);
    assert!(
        bucket["primary"]["resetsAt"]
            .as_str()
            .unwrap()
            .starts_with("2100-01-01")
    );
}

#[test]
fn skills_come_from_codex_and_the_missing_slash_commands_say_why() {
    // Catches skills read from Codex's private files, or a silent gap where slash commands
    // should be (D97).
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let thread = engine.create_thread_of("codex");

    let skills = engine
        .client()
        .get(&format!("/api/threads/{}/skills", thread.id))
        .unwrap();

    assert_eq!(skills.status, 200, "{}", skills.body);
    assert_eq!(skills.body["skills"][0]["name"], "project");
    assert!(
        skills.body["slashCommandsGap"]
            .as_str()
            .unwrap()
            .contains("slash commands")
    );
}

#[test]
fn without_codex_a_codex_thread_answers_503_and_other_threads_still_work() {
    // Catches an engine that refuses to start, or hides why, when Codex is not installed.
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("no-codex-here");
    let engine = RunningEngine::start_with_codex(home.path(), &missing);
    let client = engine.client();
    let codex = engine.create_thread_of("codex");
    let claude = engine.create_thread_of("claude");

    let refused = client
        .post(&messages_path(&codex), &json!({ "text": "hello" }))
        .unwrap();
    let logged = client
        .post(&messages_path(&claude), &json!({ "text": "hello" }))
        .unwrap();

    assert_eq!(refused.status, 503);
    assert!(
        refused.body["error"]
            .as_str()
            .unwrap()
            .contains("CODEX_EXE"),
        "{}",
        refused.body
    );
    assert_eq!(logged.status, 201);
}

#[test]
fn a_new_codex_thread_s_settings_meet_the_checks_a_settings_change_meets() {
    // Catches creation storing settings the settings route refuses, such as a one-token context
    // limit that Codex would then load the thread with.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let workspace = client
        .post(
            "/api/workspaces",
            &json!({ "name": "repo", "path": home.path() }),
        )
        .unwrap()
        .body["id"]
        .clone();
    let create = |settings: Value| {
        client
            .post(
                "/api/threads",
                &json!({ "workspaceId": workspace, "title": "chat", "provider": "codex",
                         "settings": settings }),
            )
            .unwrap()
    };
    let settings = |model: Value, limit: Value| {
        json!({ "model": model, "permission": "askMe", "planMode": false,
                "contextLimitTokens": limit })
    };

    let tiny_limit = create(settings(Value::Null, json!(1)));
    let blank_model = create(settings(json!(" "), Value::Null));
    let plain = create(settings(json!("gpt-5"), Value::Null));

    assert_eq!(tiny_limit.status, 400, "{}", tiny_limit.body);
    assert_eq!(blank_model.status, 400, "{}", blank_model.body);
    assert_eq!(plain.status, 201, "{}", plain.body);
    let threads = client
        .get(&format!(
            "/api/threads?workspaceId={}",
            workspace.as_str().unwrap()
        ))
        .unwrap();
    assert_eq!(
        threads.body.as_array().unwrap().len(),
        1,
        "only the valid thread is stored"
    );
}

#[test]
fn a_message_id_sent_again_with_another_message_answers_409() {
    // Catches a client that reuses an id for new text or files getting success for the earlier
    // message, while the new instruction is dropped.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let thread = engine.create_thread_of("codex");
    let send = |body: Value| client.post(&messages_path(&thread), &body).unwrap();

    let first = send(json!({ "messageId": "same", "text": "first instruction" }));
    let retried = send(json!({ "messageId": "same", "text": "first instruction" }));
    let changed_text = send(json!({ "messageId": "same", "text": "different instruction" }));
    let added_file = send(json!({ "messageId": "same", "input": [
        { "type": "text", "text": "first instruction" },
        { "type": "file", "path": "notes.md" },
    ] }));

    assert_eq!(first.status, 201);
    assert_eq!(
        (retried.status, &retried.body["input"]),
        (200, &first.body["input"])
    );
    assert_eq!(changed_text.status, 409, "{}", changed_text.body);
    assert_eq!(added_file.status, 409, "{}", added_file.body);
    let details = client.get(&format!("/api/threads/{}", thread.id)).unwrap();
    assert_eq!(details.body["queue"][0]["input"], first.body["input"]);
}

#[test]
fn a_full_queue_answers_429_and_stores_nothing() {
    // Catches an eleventh message stored and then refused, which a retry would duplicate (D85).
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let thread = engine.create_thread_of("codex");
    for n in 0..10 {
        let sent = client
            .post(
                &messages_path(&thread),
                &json!({ "text": "work", "messageId": format!("m{n}") }),
            )
            .unwrap();
        assert_eq!(sent.status, 201);
    }

    let full = client
        .post(
            &messages_path(&thread),
            &json!({ "text": "more", "messageId": "m10" }),
        )
        .unwrap();

    assert_eq!(full.status, 429);
    assert_eq!(
        (&full.body["queued"], &full.body["limit"]),
        (&json!(10), &json!(10))
    );
    let details = client.get(&format!("/api/threads/{}", thread.id)).unwrap();
    assert_eq!(details.body["queue"].as_array().unwrap().len(), 10);
}
