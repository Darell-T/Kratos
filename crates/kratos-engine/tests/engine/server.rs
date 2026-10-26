use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;

use kratos_client::{ApiReply, ApiRequest, Client, ClientError, Incoming};
use kratos_protocol::{Seq, Thread, Workspace};
use serde_json::{Value, json};

use crate::running_engine::{
    RunningEngine, WAIT, expect_silence, next_seqs, next_text, post, subscribed,
};

fn send(client: &Client, method: &str, path: &str, body: Option<Value>) -> ApiReply {
    let mut request = ApiRequest::new(method, path);
    if let Some(body) = body {
        request = request.body(serde_json::to_vec(&body).unwrap());
    }
    client.send(&request).unwrap()
}

fn error_text(reply: &ApiReply) -> &str {
    reply.body["error"].as_str().unwrap()
}

fn message_body(size: usize) -> Vec<u8> {
    let padding = size - r#"{"text":""}"#.len();
    format!(r#"{{"text":"{}"}}"#, "x".repeat(padding)).into_bytes()
}

/// Posts a body of `size` bytes the way a client does that gives up when a write fails: it writes
/// the whole body, and only then reads the reply. The body is not JSON, because the engine must
/// refuse an oversized one before it parses anything. `token` goes as the client token.
pub(crate) fn post_whole_body_then_read(
    port: u16,
    path: &str,
    size: usize,
    token: Option<&str>,
) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let authorization = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{authorization}Content-Length: {size}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream
        .write_all(&vec![b'x'; size])
        .expect("the engine should read the body to its end before it answers");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply
}

/// An `unsubscribe` message padded with spaces to exactly `size` bytes: valid JSON that only its
/// size can get refused.
fn padded_unsubscribe(size: usize) -> String {
    let message = r#"{"type":"unsubscribe","threadId":"padding"}"#;
    format!("{message}{}", " ".repeat(size - message.len()))
}

#[test]
fn answers_the_health_check() {
    // Catches a missing or renamed route, which the boot recipe and every lane poll first.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());

    let reply = engine.client().get("/api/health").unwrap();

    assert_eq!((reply.status, reply.body), (200, json!({ "ok": true })));
}

#[test]
fn matches_http_api_routes_with_empty_segments_and_preserves_query_and_parameters() {
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();

    for path in [
        "/api/health/",
        "/api//health",
        "/api///health///?ignored=%2F%3F",
    ] {
        let reply = client.get(path).unwrap();
        assert_eq!(
            (reply.status, reply.body),
            (200, json!({ "ok": true })),
            "{path}"
        );
    }
    let created = client
        .post(
            "/api///workspaces//",
            &json!({ "name": "normalized", "path": home.path() }),
        )
        .unwrap();
    assert_eq!(created.status, 201);
    let workspace: Workspace = created.parse().unwrap();
    let created = client
        .post(
            "/api//threads/",
            &json!({ "workspaceId": workspace.id, "title": "normalized", "provider": "claude" }),
        )
        .unwrap();
    assert_eq!(created.status, 201);
    let thread: Thread = created.parse().unwrap();
    let encoded_workspace: String = workspace
        .id
        .as_str()
        .bytes()
        .map(|byte| format!("%{byte:02X}"))
        .collect();
    let listed = client
        .get(&format!(
            "/api//threads///?workspaceId={encoded_workspace}&ignored=%2F%3F"
        ))
        .unwrap();
    assert_eq!((listed.status, listed.body), (200, json!([created.body])));
    let reply = client
        .post(
            &format!("/api///threads//{}///messages//?ignored=%2F", thread.id),
            &json!({ "text": "normalized message" }),
        )
        .unwrap();
    assert_eq!(reply.status, 201);
    assert_eq!(reply.body["threadId"], json!(thread.id));
    assert_eq!(reply.body["seq"], 1);
    assert_eq!(
        reply.body["event"],
        json!({ "kind": "user.message", "text": "normalized message" })
    );

    let malformed = client
        .post("/api//threads/%ff//messages/", &json!({ "text": "bad" }))
        .unwrap();
    assert_eq!(
        (malformed.status, error_text(&malformed)),
        (400, "Malformed URL path")
    );
    let encoded_slash = client
        .post("/api//threads/a%2Fb//messages/", &json!({ "text": "bad" }))
        .unwrap();
    assert_eq!(
        (encoded_slash.status, error_text(&encoded_slash)),
        (404, "Unknown thread a/b")
    );
}

#[test]
fn normalized_routes_keep_the_origin_guards_and_the_exact_socket_upgrade_path() {
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let body = serde_json::to_vec(&json!({ "name": "forbidden", "path": home.path() })).unwrap();
    for (header, value) in [("Host", "evil.example"), ("Origin", "https://evil.example")] {
        let reply = client
            .send(
                &ApiRequest::new("POST", "/api//workspaces/")
                    .header(header, value)
                    .body(body.clone()),
            )
            .unwrap();
        assert_eq!(
            (reply.status, reply.body),
            (403, json!({ "error": "Forbidden" })),
            "{header}"
        );
    }
    assert_eq!(client.get("/api/workspaces").unwrap().body, json!([]));
    for path in ["/api/events/", "/api//events", "/api///events///"] {
        let reply = client
            .send(
                &ApiRequest::new("GET", path)
                    .header("Connection", "upgrade, close")
                    .header("Upgrade", "websocket")
                    .header("Sec-WebSocket-Version", "13")
                    .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            )
            .unwrap();
        assert_eq!(
            (reply.status, error_text(&reply)),
            (404, "Not found"),
            "{path}"
        );
    }
}

#[test]
#[cfg(windows)]
fn shuts_down_with_an_incomplete_upload_still_connected() {
    use std::io::{BufRead, BufReader};

    let home = tempfile::tempdir().unwrap();
    let mut engine = RunningEngine::start_for_shutdown(home.path());
    let port = engine.port();
    let mut stalled = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stalled.set_read_timeout(Some(WAIT)).unwrap();
    write!(stalled, "POST /api/workspaces HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 1024\r\nExpect: 100-continue\r\n\r\n").unwrap();
    let mut stalled = BufReader::new(stalled);
    let mut interim = String::new();
    stalled.read_line(&mut interim).unwrap();
    assert_eq!(interim, "HTTP/1.1 100 Continue\r\n");
    interim.clear();
    stalled.read_line(&mut interim).unwrap();
    assert_eq!(interim, "\r\n");
    // 100 Continue proves the server is reading this body before shutdown is requested.
    stalled.get_mut().write_all(b"{").unwrap();

    engine.shutdown();

    drop(stalled);
    let restarted = RunningEngine::start(home.path());
    assert_eq!(
        restarted.client().get("/api/workspaces").unwrap().body,
        json!([])
    );
}

#[test]
fn creates_a_workspace_a_thread_and_messages() {
    // Catches lost trimming, a wrong status, a list that does not show what was created, or
    // numbering that does not start at 1.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let folder = home.path().to_str().unwrap();

    let created = client
        .post(
            "/api/workspaces",
            &json!({ "name": " repo ", "path": folder }),
        )
        .unwrap();
    assert_eq!(created.status, 201);
    let workspace: Workspace = created.parse().unwrap();
    assert_eq!(
        (workspace.name.as_str(), workspace.path.as_str()),
        ("repo", folder)
    );
    assert_eq!(
        client.get("/api/workspaces").unwrap().body,
        json!([created.body])
    );

    // A Claude thread keeps R2's plain message log until R4 drives Claude.
    let body = json!({ "workspaceId": workspace.id, "title": "first chat", "provider": "claude" });
    let created = client.post("/api/threads", &body).unwrap();
    assert_eq!(created.status, 201);
    let thread: Thread = created.parse().unwrap();
    let listed = client
        .get(&format!("/api/threads?workspaceId={}", workspace.id))
        .unwrap();
    assert_eq!(listed.body, json!([created.body]));

    let message = format!("/api/threads/{}/messages", thread.id);
    let first = client
        .post(&message, &json!({ "text": "  hello  " }))
        .unwrap();
    assert_eq!(first.status, 201);
    assert_eq!(first.body["threadId"], json!(thread.id));
    assert_eq!(first.body["seq"], json!(1));
    assert_eq!(
        first.body["event"],
        json!({ "kind": "user.message", "text": "hello" })
    );
    assert_eq!(post(&client, &thread, "again").seq, Seq::new(2));
}

#[test]
fn rejects_invalid_bodies_with_400() {
    // Catches a validation gap, a rule that does not name its field, and a relative-path text that
    // drifts from the TypeScript engine's (lane 9).
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let folder = home.path().to_str().unwrap();
    let cases = [
        (
            "/api/workspaces",
            json!({ "name": "  ", "path": folder }),
            "name: must not be blank",
        ),
        (
            "/api/workspaces",
            json!({ "name": "x" }),
            "missing field `path`",
        ),
        (
            "/api/workspaces",
            json!({ "name": "x", "path": "." }),
            "Path is not an existing absolute directory: .",
        ),
        (
            "/api/workspaces",
            json!({ "name": "x", "path": "/definitely/not/here" }),
            "Path is not an existing absolute directory: /definitely/not/here",
        ),
        (
            "/api/threads",
            json!({ "workspaceId": "w", "title": "t", "provider": "gemini" }),
            "provider: unknown variant `gemini`, expected `codex` or `claude`",
        ),
        (
            "/api/threads",
            json!({ "workspaceId": "w", "provider": "codex" }),
            "missing field `title`",
        ),
        (
            "/api/threads",
            json!({ "workspaceId": "  ", "title": "t", "provider": "codex" }),
            "workspaceId: must not be blank",
        ),
    ];

    for (path, body, expected) in cases {
        let reply = client.post(path, &body).unwrap();
        assert_eq!(reply.status, 400, "{body}");
        assert_eq!(error_text(&reply), expected, "{body}");
    }
    assert_eq!(client.get("/api/workspaces").unwrap().body, json!([]));
}

#[test]
fn rejects_a_workspace_path_that_is_a_file() {
    // Catches a check that asks whether the path exists instead of whether it is a directory.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let file = home.path().join("file.txt");
    fs::write(&file, "x").unwrap();

    let reply = engine
        .client()
        .post("/api/workspaces", &json!({ "name": "x", "path": file }))
        .unwrap();

    assert_eq!(reply.status, 400);
    assert!(error_text(&reply).starts_with("Path is not an existing absolute directory: "));
}

#[test]
fn rejects_malformed_and_blank_messages_without_storing_them() {
    // Catches an event stored before its body was validated, and a body with something after the
    // JSON value, which a parser that stops at the first value would accept.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let thread = engine.create_thread();
    let path = format!("/api/threads/{}/messages", thread.id);

    for (body, expected) in [
        (r#"{"text":"   "}"#, "text: must not be blank"),
        ("{}", "missing field `text`"),
        ("{nope", "Request body must be valid JSON"),
        (
            r#"{"text":"a"} trailing"#,
            "Request body must be valid JSON",
        ),
        ("", "Request body must be valid JSON"),
    ] {
        let reply = client
            .send(&ApiRequest::new("POST", &path).body(body.into()))
            .unwrap();
        assert_eq!(reply.status, 400, "{body:?}");
        assert_eq!(error_text(&reply), expected, "{body:?}");
    }

    assert_eq!(post(&client, &thread, "first valid").seq, Seq::new(1));
}

#[test]
fn answers_404_for_unknown_threads_workspaces_and_paths() {
    // Catches axum's own empty 404 or 405 leaking through, a path outside /api that reaches a
    // route, an unreadable thread id that is not told apart from an unknown one, and a body read
    // before the thread is looked up.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let new_thread = json!({ "workspaceId": "nope", "title": "t", "provider": "codex" });
    let new_workspace = json!({ "name": "x", "path": home.path() });
    let hi = json!({ "text": "hi" });
    let cases = [
        (
            "POST",
            "/api/threads/nope/messages",
            Some(hi.clone()),
            404,
            "Unknown thread nope",
        ),
        (
            "POST",
            "/api/threads",
            Some(new_thread),
            404,
            "Unknown workspace nope",
        ),
        (
            "GET",
            "/api/threads?workspaceId=nope",
            None,
            404,
            "Unknown workspace nope",
        ),
        ("GET", "/api/nothing", None, 404, "Not found"),
        ("GET", "/", None, 404, "Not found"),
        ("POST", "/", Some(new_workspace.clone()), 404, "Not found"),
        ("GET", "//api/health", None, 404, "Not found"),
        ("GET", "/.//api/health", None, 404, "Not found"),
        (
            "POST",
            "/.//api/workspaces",
            Some(new_workspace),
            404,
            "Not found",
        ),
        ("DELETE", "/api/workspaces", None, 404, "Not found"),
        ("GET", "/api/threads/nope/messages", None, 404, "Not found"),
        ("GET", "/api/events", None, 404, "Not found"),
        (
            "POST",
            "/api/threads/%ff/messages",
            Some(hi),
            400,
            "Malformed URL path",
        ),
    ];

    for (method, path, body, status, expected) in cases {
        let reply = send(&client, method, path, body);
        assert_eq!(
            (reply.status, error_text(&reply)),
            (status, expected),
            "{method} {path}"
        );
    }
    let missing_query = client.get("/api/threads").unwrap();
    assert_eq!(missing_query.status, 400);
    assert!(error_text(&missing_query).contains("workspaceId"));
    assert_eq!(client.get("/api/threads?workspaceId=").unwrap().status, 400);
    assert_eq!(client.get("/api/workspaces").unwrap().body, json!([]));

    let bad_body = ApiRequest::new("POST", "/api/threads/nope/messages").body(b"{nope".to_vec());
    let reply = client.send(&bad_body).unwrap();
    assert_eq!(
        (reply.status, error_text(&reply)),
        (404, "Unknown thread nope")
    );
}

#[test]
fn caps_request_bodies_at_1_mib_and_keeps_serving() {
    // Catches an off-by-one at the cap, a 413 that never reaches a client that is still sending
    // (lane 7: on Windows an engine that answers early resets the connection under a client whose
    // write fails), and a connection or an engine left broken after a refusal.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let thread = engine.create_thread();
    let path = format!("/api/threads/{}/messages", thread.id);
    let post_body = |size: usize| {
        client
            .send(&ApiRequest::new("POST", &path).body(message_body(size)))
            .unwrap()
    };
    const MIB: usize = 1024 * 1024;

    assert_eq!(post_body(MIB).status, 201, "exactly 1 MiB is accepted");
    for size in [MIB + 1, 2 * MIB] {
        let refused = post_body(size);
        assert_eq!(
            (refused.status, error_text(&refused)),
            (413, "Request body is too large"),
            "{size} bytes"
        );
    }

    let reply = post_whole_body_then_read(engine.port(), &path, 8 * MIB, Some(&engine.token()));
    assert!(reply.starts_with("HTTP/1.1 413 "), "{reply}");
    assert_eq!(
        post(&client, &thread, "after the refusals").seq,
        Seq::new(2)
    );
}

#[test]
fn accepts_local_pages_and_clients_that_send_no_origin() {
    // Catches a check so strict that the engine's own clients and local pages are refused.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let port = engine.port();

    for host in [
        format!("localhost:{port}"),
        "LOCALHOST".into(),
        "127.0.0.1".into(),
        "[::1]:5173".into(),
    ] {
        let reply = client
            .send(&ApiRequest::new("GET", "/api/health").header("Host", &host))
            .unwrap();
        assert_eq!(reply.status, 200, "Host {host}");
    }
    let body = serde_json::to_vec(&json!({ "name": "x", "path": home.path() })).unwrap();
    let from_a_local_page = ApiRequest::new("POST", "/api/workspaces")
        .header("Origin", "http://localhost:5173")
        .body(body);
    assert_eq!(client.send(&from_a_local_page).unwrap().status, 201);
    assert!(
        client
            .connect_events(&[("Origin", &format!("http://localhost:{port}"))])
            .is_ok()
    );
    assert!(client.connect_events(&[]).is_ok());
}

#[test]
fn refuses_other_websites_and_host_names_and_stores_nothing() {
    // Catches substring matching, a forgotten socket or fallback route, and work done before the
    // check (lanes 5 and 6).
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let body = serde_json::to_vec(&json!({ "name": "x", "path": home.path() })).unwrap();
    let forbidden = (403, json!({ "error": "Forbidden" }));
    let refused = |request: ApiRequest| {
        let reply = client.send(&request).unwrap();
        (reply.status, reply.body)
    };

    for host in [
        "evil.example",
        "localhost.evil.example",
        "127.0.0.1.evil.example",
    ] {
        let request = ApiRequest::new("POST", "/api/workspaces")
            .header("Host", host)
            .body(body.clone());
        assert_eq!(refused(request), forbidden, "Host {host}");
    }
    for origin in [
        "https://example.com",
        "http://localhost.evil.example",
        "null",
    ] {
        let request = ApiRequest::new("POST", "/api/workspaces")
            .header("Origin", origin)
            .body(body.clone());
        assert_eq!(refused(request), forbidden, "Origin {origin}");
    }
    let unknown_path = ApiRequest::new("GET", "/api/nothing").header("Host", "evil.example");
    assert_eq!(
        refused(unknown_path),
        forbidden,
        "a foreign Host is refused before routing"
    );
    let no_host = client
        .send_raw(b"GET /api/health HTTP/1.1\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert_eq!(
        (no_host.status, no_host.body),
        forbidden,
        "a request with no Host"
    );
    for headers in [
        [("Origin", "https://example.com")],
        [("Host", "evil.example")],
    ] {
        let handshake = client.connect_events(&headers).err().unwrap();
        assert!(
            matches!(handshake, ClientError::Handshake(403)),
            "{headers:?}: {handshake}"
        );
    }

    assert_eq!(client.get("/api/workspaces").unwrap().body, json!([]));
}

#[test]
fn replays_the_backlog_then_streams_live_events_once() {
    // Catches a replay that skips the backlog and an event delivered twice.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let thread = engine.create_thread();
    post(&engine.client(), &thread, "one");
    post(&engine.client(), &thread, "two");

    let mut socket = subscribed(&engine, &thread.id, 0);
    assert_eq!(next_seqs(&mut socket, 2), [1, 2]);
    post(&engine.client(), &thread, "three");

    assert_eq!(next_seqs(&mut socket, 1), [3]);
    expect_silence(&mut socket);
}

#[test]
fn loses_and_repeats_nothing_when_messages_arrive_while_subscribing() {
    // Catches a follower that registers after its first read (an event is missed until some later
    // post) and a replay and a live path that overlap (an event comes twice). Each round races a
    // subscribe against posts, on a fresh thread.
    const ROUNDS: usize = 20;
    const RACING_POSTS: u64 = 25;
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();

    for round in 0..ROUNDS {
        let thread = engine.create_thread();
        for text in ["a", "b", "c"] {
            post(&client, &thread, text);
        }
        thread::scope(|scope| {
            let poster = scope.spawn(|| {
                for number in 0..RACING_POSTS {
                    post(&client, &thread, &format!("racing {number}"));
                }
            });
            let mut socket = subscribed(&engine, &thread.id, 0);
            let expected: Vec<u64> = (1..=3 + RACING_POSTS).collect();
            assert_eq!(
                next_seqs(&mut socket, expected.len()),
                expected,
                "round {round}"
            );
            expect_silence(&mut socket);
            poster.join().unwrap();
        });
    }
}

#[test]
fn resumes_after_a_given_seq_and_stops_after_unsubscribe() {
    // Catches a cursor that is ignored, and an unsubscribe that leaves the follow running.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let thread = engine.create_thread();
    post(&engine.client(), &thread, "one");
    post(&engine.client(), &thread, "two");
    let mut socket = subscribed(&engine, &thread.id, 1);
    assert_eq!(next_seqs(&mut socket, 1), [2]);

    socket.unsubscribe(&thread.id).unwrap();
    // The engine reads a socket's messages in order, so its answer to this one shows that the
    // unsubscribe before it has taken effect.
    socket.send_text("not json".into()).unwrap();
    assert!(next_text(&mut socket).contains(r#""type":"error""#));
    post(&engine.client(), &thread, "three");

    expect_silence(&mut socket);
}

#[test]
fn subscribing_again_replaces_the_earlier_subscription() {
    // Catches two follows of one thread on a socket, which would send every later event twice.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let thread = engine.create_thread();
    post(&engine.client(), &thread, "one");
    post(&engine.client(), &thread, "two");
    let mut socket = subscribed(&engine, &thread.id, 0);
    assert_eq!(next_seqs(&mut socket, 2), [1, 2]);

    socket.subscribe(&thread.id, Seq::new(2)).unwrap();
    socket.send_text("not json".into()).unwrap();
    assert!(next_text(&mut socket).contains(r#""type":"error""#));
    post(&engine.client(), &thread, "three");

    assert_eq!(next_seqs(&mut socket, 1), [3]);
    expect_silence(&mut socket);
}

#[test]
fn reports_unknown_threads_and_invalid_messages_as_errors() {
    // Catches a socket that closes or stays silent on a bad message, and a cursor ahead of the log
    // that waits silently instead of saying so (after a restore it would skip events that reuse
    // their numbers).
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let thread = engine.create_thread();
    post(&engine.client(), &thread, "one");
    post(&engine.client(), &thread, "two");
    let mut socket = engine.client().connect_events(&[]).unwrap();

    socket
        .send_text(r#"{"type":"subscribe","threadId":"nope","after":0}"#.into())
        .unwrap();
    assert_eq!(
        next_text(&mut socket),
        r#"{"type":"error","message":"Unknown thread nope"}"#
    );
    for invalid in [
        "not json",
        r#"{"type":"subscribe","threadId":"x","after":-1}"#,
        r#"{"type":"jump"}"#,
    ] {
        socket.send_text(invalid.into()).unwrap();
        let reply = next_text(&mut socket);
        let message: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(message["type"], "error", "{invalid}: {reply}");
        assert!(
            !message["message"].as_str().unwrap().is_empty(),
            "{invalid}"
        );
    }
    socket.subscribe(&thread.id, Seq::new(5)).unwrap();
    let ahead = format!(
        r#"{{"type":"error","message":"Cursor 5 is ahead of thread {}, which ends at 2"}}"#,
        thread.id
    );
    assert_eq!(next_text(&mut socket), ahead);
    post(&engine.client(), &thread, "three");
    expect_silence(&mut socket);

    socket.subscribe(&thread.id, Seq::BEFORE_FIRST).unwrap();
    assert_eq!(
        next_seqs(&mut socket, 3),
        [1, 2, 3],
        "the socket still works after its errors"
    );
}

#[test]
fn closes_a_socket_whose_message_passes_64_kib_and_serves_the_next() {
    // Catches an off-by-one at the cap, a refusal that leaves the socket open, and one that
    // disturbs other sockets (lane 8).
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let thread = engine.create_thread();
    post(&engine.client(), &thread, "one");
    const CAP: usize = 64 * 1024;

    let mut at_the_cap = engine.client().connect_events(&[]).unwrap();
    at_the_cap.send_text(padded_unsubscribe(CAP)).unwrap();
    expect_silence(&mut at_the_cap);

    let mut over_the_cap = engine.client().connect_events(&[]).unwrap();
    let _ = over_the_cap.send_text(padded_unsubscribe(CAP + 1));
    let closed = over_the_cap.next_incoming(WAIT).unwrap();
    assert_eq!(closed, Some(Incoming::Closed(Some(1009))));

    let mut next = subscribed(&engine, &thread.id, 0);
    assert_eq!(next_seqs(&mut next, 1), [1]);
}
