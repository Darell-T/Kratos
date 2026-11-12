//! A causal stream-json recording player and controllable Claude process for client tests.
//!
//! `FAKE_CLAUDE_TRANSCRIPT` chooses a scrubbed wire recording. Recorded outgoing rows wait for
//! real stdin; incoming rows substitute the client's request IDs and message UUIDs. `test/*`
//! controls inspect received input, emit frames, hold requests, and inject transport faults.
//!
//! `FAKE_CLAUDE_LOG` switches to the scripted fake the engine's tests steer (see `scripted`).
//!
//! `--version`, `doctor`, and `update` answer as the install `FAKE_CLAUDE_INSTALL` names does, the
//! native one without it. The installed release is the text of the file `FAKE_CLAUDE_RELEASE`
//! names, or 2.1.303 without one. `update` writes `FAKE_CLAUDE_UPDATE_TO` there, or fails when that
//! is `fail`.

mod scripted;

/// Answers a release command and returns true, or returns false for a session.
fn release(command: &str) -> bool {
    let file = std::env::var_os("FAKE_CLAUDE_RELEASE");
    let installed = file
        .as_ref()
        .and_then(|file| std::fs::read_to_string(file).ok())
        .unwrap_or_else(|| "2.1.303".to_owned());
    let method = std::env::var("FAKE_CLAUDE_INSTALL").unwrap_or_else(|_| "native".to_owned());
    match command {
        "--version" => println!("{} (Claude Code)", installed.trim()),
        "doctor" => println!(
            "Claude Code doctor\n\n  Running: {method} ({})\n  Config install method: {method}\n  \
             Auto-update channel: latest",
            installed.trim()
        ),
        "update" => {
            let target = std::env::var("FAKE_CLAUDE_UPDATE_TO").unwrap_or_default();
            if target == "fail" {
                eprintln!("Error: the download was interrupted");
                std::process::exit(1);
            }
            let file = file.expect("an update test names FAKE_CLAUDE_RELEASE");
            std::fs::write(file, &target).expect("the release file is writable");
            println!(
                "Successfully updated from {} to version {target}",
                installed.trim()
            );
        }
        _ => return false,
    }
    true
}

use std::collections::HashMap;
use std::io::{BufRead, Write};

use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_RECORDING: &str = include_str!(
    "../../../../../crates/kratos-claude-client/tests/transcripts/s1-launch-and-stream/wire.jsonl"
);

#[derive(Deserialize)]
struct Row {
    dir: String,
    msg: Value,
}

#[derive(Default)]
struct Fake {
    received: Vec<Value>,
    bindings: HashMap<String, String>,
    users: HashMap<String, Value>,
}

fn main() {
    if let Some(command) = std::env::args().nth(1)
        && release(&command)
    {
        return;
    }
    if let Some(log) = std::env::var_os("FAKE_CLAUDE_LOG") {
        scripted::run(std::path::Path::new(&log));
        return;
    }
    if let Ok(path) = std::env::var("FAKE_CLAUDE_PID_FILE") {
        std::fs::write(path, std::process::id().to_string())
            .expect("PID observation file is writable");
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut fake = Fake::default();
    if let Ok(path) = std::env::var("FAKE_CLAUDE_TRANSCRIPT") {
        let text = std::fs::read_to_string(path).expect("the recording is readable");
        if !fake.replay(&text, &mut lines) {
            return;
        }
    }
    while let Some(message) = receive(&mut lines) {
        fake.handle(message);
    }
    if std::env::var_os("FAKE_CLAUDE_HOLD_AFTER_EOF").is_some() {
        std::thread::park();
    }
}

impl Fake {
    fn replay(
        &mut self,
        text: &str,
        lines: &mut impl Iterator<Item = std::io::Result<String>>,
    ) -> bool {
        for line in text.lines() {
            let row: Row = serde_json::from_str(line).expect("recordings contain JSON rows");
            if row.dir == "in" {
                let mut message = substitute(row.msg, &self.bindings);
                if message["type"] == "user"
                    && message["isReplay"] == true
                    && let Some(user) = message["uuid"].as_str().and_then(|id| self.users.get(id))
                {
                    message["message"] = user["message"].clone();
                }
                send(&message);
            } else if !self.match_input(&row.msg, lines) {
                return false;
            }
        }
        true
    }

    fn match_input(
        &mut self,
        expected: &Value,
        lines: &mut impl Iterator<Item = std::io::Result<String>>,
    ) -> bool {
        loop {
            let Some(actual) = receive(lines) else {
                return false;
            };
            if actual["request"]["subtype"]
                .as_str()
                .is_some_and(|subtype| subtype.starts_with("test/"))
            {
                self.handle(actual);
                continue;
            }
            assert_eq!(actual["type"], expected["type"], "recorded input kind");
            match expected["type"].as_str() {
                Some("control_request") => {
                    assert_eq!(
                        actual["request"]["subtype"], expected["request"]["subtype"],
                        "recorded control subtype"
                    );
                    self.bind(expected, &actual, "request_id");
                }
                Some("user") => {
                    self.bind(expected, &actual, "uuid");
                    self.users.insert(
                        actual["uuid"].as_str().expect("a user UUID").to_owned(),
                        actual.clone(),
                    );
                }
                Some("control_response") => assert_eq!(
                    actual["response"],
                    substitute(expected["response"].clone(), &self.bindings),
                    "reverse control response envelope and body"
                ),
                _ => {}
            }
            self.received.push(actual);
            return true;
        }
    }

    fn bind(&mut self, expected: &Value, actual: &Value, key: &str) {
        self.bindings.insert(
            expected[key].as_str().expect("recorded id").to_owned(),
            actual[key].as_str().expect("live id").to_owned(),
        );
    }

    fn handle(&mut self, message: Value) {
        self.received.push(message.clone());
        match message["type"].as_str() {
            Some("control_request") => self.control(&message),
            Some("control_response") => send(&json!({"type":"test_answer", "answer":message})),
            Some("user") => {
                let mut replay = message;
                replay["isReplay"] = json!(true);
                replay["session_id"] = json!("00000000-0000-4000-8000-000000000001");
                send(&replay);
            }
            _ => {}
        }
    }

    fn control(&self, message: &Value) {
        let request = &message["request"];
        match request["subtype"].as_str() {
            Some("initialize") => self.initialize(message),
            Some("test/emit") => {
                send(&request["message"]);
                reply(message, json!({}));
            }
            Some("test/inspect") => reply(
                message,
                json!({"received":self.received, "arguments":std::env::args().skip(1).collect::<Vec<_>>(), "environment":std::env::vars().collect::<HashMap<_,_>>(), "pid":std::process::id()}),
            ),
            Some("test/echo") => reply(message, request["value"].clone()),
            Some("interrupt") => reply(message, json!({"still_queued":[]})),
            Some("get_usage") => reply(message, usage_answer(&json!({}))),
            _ => self.fault(message),
        }
    }

    fn fault(&self, message: &Value) {
        let request = &message["request"];
        match request["subtype"].as_str() {
            Some("test/hang") => {
                send(&json!({"type":"test_pending", "request_id":message["request_id"]}))
            }
            Some("test/flood_then_exit") => {
                for _ in 0..512 {
                    send(&json!({"type":"noise"}));
                }
                eprintln!("final fake diagnostic");
                std::process::exit(19);
            }
            Some("test/exit") => {
                let count = request["stderr_lines"].as_u64().unwrap_or(1);
                for line in 0..count {
                    eprintln!("fake stderr {line}");
                }
                eprintln!("final fake diagnostic");
                std::process::exit(request["code"].as_i64().unwrap_or(17) as i32);
            }
            Some("test/flood_controls") => {
                for number in 0..request["count"].as_u64().unwrap_or(300) {
                    send(
                        &json!({"type":"control_request", "request_id":format!("fake-{number}"),
                        "request":{"subtype":"can_use_tool", "tool_name":"Read", "input":{},
                            "tool_use_id":format!("tool-{number}")}}),
                    );
                }
                reply(message, json!({}));
            }
            Some("test/close_stdout_then_exit") => close_stdout_then_exit(request),
            Some("test/oversized") => {
                let bytes = usize::try_from(request["bytes"].as_u64().unwrap_or(150_000))
                    .expect("bounded test length");
                write_line(&"x".repeat(bytes));
                send(&json!({"type":"user", "isReplay":true, "uuid":"after-oversize"}));
                reply(message, json!({}));
            }
            _ => fail(message, "unsupported fake control"),
        }
    }

    fn initialize(&self, message: &Value) {
        match std::env::var("FAKE_CLAUDE_INITIALIZE").as_deref() {
            Ok("hang") => return,
            Ok("malformed") => {
                reply(message, json!({}));
                return;
            }
            _ => {}
        }
        reply(message, initialize_response());
    }
}

/// The initialize answer the default recording holds: a Claude Pro login, no email. The command
/// list is the fake's own, so a test can tell a built-in from a defined command and see aliases.
fn initialize_response() -> Value {
    let row = DEFAULT_RECORDING
        .lines()
        .map(|line| serde_json::from_str::<Row>(line).expect("default recording row"))
        .find(|row| row.dir == "in" && row.msg["type"] == "control_response")
        .expect("default initialize reply");
    let mut response = row.msg["response"]["response"].clone();
    if let Some(plan) =
        std::env::var_os("FAKE_CLAUDE_PLAN").and_then(|file| std::fs::read_to_string(file).ok())
    {
        response["account"]["subscriptionType"] = json!(plan.trim());
    }
    response["commands"] = json!([
        { "name": "synthetic-command", "description": "Synthetic command", "argumentHint": "",
          "aliases": ["synthetic-alias"], "builtin": true },
        { "name": "synthetic-plugin-command", "description": "Defined outside the CLI",
          "argumentHint": "<file>" },
    ]);
    response
}

/// A `get_usage` answer in the shape the CLI measured: a whole percent and an ISO 8601 reset with
/// microseconds and an offset, beside null keys the crate ignores. `window` fields `fiveHour` and
/// `sevenDay` hold `{ utilization, resetsAt }` or null; `subscriptionType` and `available` override
/// the plan and whether plan limits apply.
fn usage_answer(window: &Value) -> Value {
    let pick = |key: &str, default: Value| {
        window.get(key).map_or(default.clone(), |given| {
            if given.is_null() {
                Value::Null
            } else {
                json!({ "utilization": given["utilization"], "resets_at": given["resetsAt"],
                            "extra": null })
            }
        })
    };
    let available = window["available"].as_bool().unwrap_or(true);
    let limits = json!({
        "five_hour": pick("fiveHour", json!({ "utilization": 19, "resets_at": "2030-01-01T05:00:00.123456+00:00" })),
        "seven_day": pick("sevenDay", json!({ "utilization": 7, "resets_at": "2030-01-06T12:00:00.000000+00:00" })),
        "seven_day_opus": null,
        "synthetic_codename_a": null,
        "synthetic_codename_b": null,
    });
    json!({
        "subscription_type": window.get("subscriptionType").cloned().unwrap_or(json!("pro")),
        "rate_limits_available": available,
        "session": { "total_cost_usd": 0 },
        "behaviors": null,
        "rate_limits": if available { limits } else { Value::Null },
    })
}

/// Models a CLI that finishes its own shutdown after stdout closes: it flushes a marker, then exits.
fn close_stdout_then_exit(request: &Value) -> ! {
    close_stdout();
    std::thread::sleep(std::time::Duration::from_millis(
        request["pause_ms"].as_u64().unwrap_or(300),
    ));
    if let Some(marker) = request["marker"].as_str() {
        std::fs::write(marker, "finished").expect("marker file is writable");
    }
    std::process::exit(request["code"].as_i64().unwrap_or(3) as i32);
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "std has no safe way to close the inherited stdout handle"
)]
fn close_stdout() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    let handle = std::io::stdout().as_raw_handle();
    // SAFETY: this process owns its inherited stdout handle and never writes to it afterwards.
    drop(unsafe { OwnedHandle::from_raw_handle(handle) });
}

#[cfg(unix)]
#[expect(
    unsafe_code,
    reason = "std has no safe way to close the inherited stdout descriptor"
)]
fn close_stdout() {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: descriptor 1 is this process's inherited stdout, and nothing writes to it afterwards.
    drop(unsafe { OwnedFd::from_raw_fd(1) });
}

fn substitute(value: Value, bindings: &HashMap<String, String>) -> Value {
    match value {
        Value::String(text) => Value::String(bindings.get(&text).cloned().unwrap_or(text)),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| substitute(value, bindings))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, substitute(value, bindings)))
                .collect(),
        ),
        other => other,
    }
}

fn receive(lines: &mut impl Iterator<Item = std::io::Result<String>>) -> Option<Value> {
    let line = lines.next()?.expect("stdin remains readable");
    Some(serde_json::from_str(&line).expect("client sends JSON"))
}

fn reply(message: &Value, body: Value) {
    send(
        &json!({"type":"control_response", "response":{"subtype":"success", "request_id":message["request_id"], "response":body}}),
    );
}
fn fail(message: &Value, error: &str) {
    send(
        &json!({"type":"control_response", "response":{"subtype":"error", "request_id":message["request_id"], "error":error}}),
    );
}
fn send(message: &Value) {
    write_line(&message.to_string());
}

fn write_line(line: &str) {
    let mut stdout = std::io::stdout().lock();
    if writeln!(stdout, "{line}")
        .and_then(|_| stdout.flush())
        .is_err()
    {
        std::process::exit(0);
    }
}
