//! A causal stream-json recording player and controllable Claude process for client tests.
//!
//! `FAKE_CLAUDE_TRANSCRIPT` chooses a scrubbed wire recording. Recorded outgoing rows wait for
//! real stdin; incoming rows substitute the client's request IDs and message UUIDs. `test/*`
//! controls inspect received input, emit frames, hold requests, and inject transport faults.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_RECORDING: &str = include_str!(
    "../../../../crates/kratos-claude-client/tests/transcripts/s1-launch-and-stream/wire.jsonl"
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
        let row = DEFAULT_RECORDING
            .lines()
            .map(|line| serde_json::from_str::<Row>(line).expect("default recording row"))
            .find(|row| row.dir == "in" && row.msg["type"] == "control_response")
            .expect("default initialize reply");
        reply(message, row.msg["response"]["response"].clone());
    }
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
