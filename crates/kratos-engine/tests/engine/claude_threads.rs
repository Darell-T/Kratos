//! The engine binary driving a Claude thread over its API, through the scripted `fake-claude`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use kratos_protocol::Provider;
use serde_json::{Value, json};

use crate::running_engine::{RunningEngine, WAIT};

/// The frames the engine wrote to the first fake-claude it started, once it has started.
fn first_process_received(log: &Path) -> Vec<Value> {
    let deadline = Instant::now() + WAIT;
    let port = loop {
        let line = std::fs::read_to_string(log)
            .ok()
            .and_then(|text| text.lines().next().map(str::to_owned));
        if let Some(line) = line {
            break serde_json::from_str::<Value>(&line).unwrap()["port"]
                .as_u64()
                .unwrap();
        }
        assert!(Instant::now() < deadline, "the engine never started claude");
        thread::sleep(Duration::from_millis(20));
    };
    loop {
        let mut stream = TcpStream::connect(("127.0.0.1", u16::try_from(port).unwrap())).unwrap();
        writeln!(
            stream,
            "{}",
            json!({ "id": 1, "method": "test/received", "params": {} })
        )
        .unwrap();
        let mut answer = String::new();
        BufReader::new(stream).read_line(&mut answer).unwrap();
        let received: Vec<Value> = serde_json::from_str::<Value>(&answer).unwrap()["result"]
            .as_array()
            .unwrap()
            .clone();
        if received.iter().any(|frame| frame["type"] == "user") || Instant::now() > deadline {
            return received;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_claude_thread_takes_messages_through_the_agent_routes_and_a_stop_reaches_its_process() {
    // Catches the routes still answering a Claude thread from Codex or from R2's plain log once
    // Claude can run: the message must reach the CLI as a queued message (201 with a message, not
    // an event), a stop must reach the CLI, and a setting the CLI cannot honor must be refused.
    let home = tempfile::tempdir().unwrap();
    let (engine, log) = RunningEngine::start_with_fake_claude(home.path());
    let client = engine.client();
    let claude = engine.create_thread_of(Provider::Claude);

    let sent = client
        .post(
            &format!("/api/threads/{}/messages", claude.id),
            &json!({ "text": "hello", "messageId": "00000000-0000-4000-8000-00000000000a" }),
        )
        .unwrap();
    let received = first_process_received(&log);
    let stopped = client
        .post(&format!("/api/threads/{}/stop", claude.id), &json!({}))
        .unwrap();
    let refused = client
        .put(
            &format!("/api/threads/{}/settings", claude.id),
            &json!({ "model": null, "permission": "askMe", "planMode": false,
                     "contextLimitTokens": 100_000 }),
        )
        .unwrap();

    assert_eq!(sent.status, 201, "{}", sent.body);
    assert_eq!(
        sent.body["status"], "queued",
        "a message, not a logged event"
    );
    let user = received
        .iter()
        .find(|frame| frame["type"] == "user")
        .expect("the CLI got the message");
    assert_eq!(user["uuid"], "00000000-0000-4000-8000-00000000000a");
    assert_eq!(
        user["message"]["content"],
        json!([{ "type": "text", "text": "hello" }])
    );
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    assert_eq!(refused.status, 400, "{}", refused.body);
}
