use std::process::{Command, Output};

/// Nothing listens on port 9, so a command that reached the network would exit 1, not 2.
const NOWHERE: &str = "http://127.0.0.1:9";

fn kratos_client(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kratos-client"))
        .args(["--url", NOWHERE])
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn a_mistyped_command_line_is_a_usage_error_and_sends_nothing() {
    // Catches a lane that silently runs a different request after a typo: an unknown command or
    // option, a missing argument, or a number that is not one.
    let mistakes: [&[&str]; 8] = [
        &[],
        &["nope"],
        &["health", "extra"],
        &["post", "only-a-thread-id"],
        &["subscribe", "thread", "--afte", "1"],
        &["subscribe", "thread", "--count", "many"],
        &["request", "GET", "/api/health", "--body-bytes", "3"],
        &["request", "POST", "/api/threads", "--header", "no-colon"],
    ];
    for mistake in mistakes {
        let output = kratos_client(mistake);
        assert_eq!(output.status.code(), Some(2), "{mistake:?}");
        assert!(output.stdout.is_empty(), "{mistake:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage: kratos-client"),
            "{mistake:?}"
        );
    }
}

#[test]
fn create_thread_settings_must_be_json_before_anything_is_sent() {
    // Catches a Claude thread created without the model or permission a lane asked for because
    // its settings were not read, and settings that are valid JSON never reaching the engine.
    let create = |settings: &str| {
        kratos_client(&[
            "create-thread",
            "workspace",
            "chat",
            "claude",
            "--settings",
            settings,
        ])
    };

    let mistyped = create("{model: haiku");
    let valid = create(r#"{"model":"claude-haiku-5-5","permission":"askMe","planMode":false}"#);

    assert_eq!(mistyped.status.code(), Some(2));
    assert!(mistyped.stdout.is_empty());
    assert_eq!(
        valid.status.code(),
        Some(1),
        "the request reached for the engine"
    );
}

#[test]
fn an_unreachable_engine_exits_1_with_a_json_error() {
    // Catches a client that prints nothing, or exits 0, when the engine is down, so a lane would
    // read a dead engine as a pass.
    let output = kratos_client(&["health"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .starts_with("cannot reach the engine at 127.0.0.1:9")
    );
}

#[test]
fn bench_turns_runs_against_the_session_url_like_every_other_command() {
    // Catches bench-turns taking the engine URL as its own argument and ignoring --url, so a lane
    // that sets --url or KRATOS_URL benchmarks a different engine than the rest of its commands.
    let output = kratos_client(&["bench-turns", "no-control-file"]);

    assert_eq!(output.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .starts_with("cannot reach the engine at 127.0.0.1:9")
    );
}

/// Serves `body` once to whoever connects, as the engine's JSON reply, and returns the URL.
fn serve_once(body: serde_json::Value) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).unwrap();
            request.extend_from_slice(&chunk[..read]);
        }
        let body = body.to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    (url, server)
}

#[test]
fn usage_prints_the_claude_accounts_two_windows_next_to_codexs() {
    // Catches the lane that reads usage seeing one provider only: the report must carry the Claude
    // account's five-hour and weekly windows in the same line as Codex's.
    let window = |minutes: i64, used: i32| {
        serde_json::json!({ "usedPercent": used, "windowMinutes": minutes,
                            "resetsAt": "2100-01-01T05:00:00.000Z" })
    };
    let account = |provider: &str, plan: &str| {
        serde_json::json!({ "provider": provider, "account": null, "plan": plan,
            "allowance": { "state": "allowed" }, "readAt": "2100-01-01T00:00:00.000Z",
            "buckets": [{ "limitId": "unified", "name": null, "reached": null,
                          "primary": window(300, 19), "secondary": window(10080, 7) }] })
    };
    let (url, server) = serve_once(
        serde_json::json!({ "accounts": [account("claude", "Claude Pro"), account("codex", "plus")] }),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_kratos-client"))
        .args(["--url", &url, "usage"])
        .output()
        .unwrap();
    server.join().unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let accounts = line["body"]["accounts"].as_array().unwrap();
    let windows: Vec<(&str, i64, i64)> = accounts
        .iter()
        .map(|account| {
            let bucket = &account["buckets"][0];
            (
                account["provider"].as_str().unwrap(),
                bucket["primary"]["windowMinutes"].as_i64().unwrap(),
                bucket["secondary"]["windowMinutes"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(windows, [("claude", 300, 10080), ("codex", 300, 10080)]);
}
