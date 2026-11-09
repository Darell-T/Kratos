//! The fake the engine's tests steer from outside. Unlike the recording player it answers only
//! `initialize` and interrupts, accepts nothing and finishes nothing on its own, and a test
//! decides each frame it emits.
//!
//! The fake binds a localhost port and appends `{pid, port, arguments, cwd}` as one line to the
//! file `FAKE_CLAUDE_LOG` names, so a test finds each process the engine started, however many
//! start. A test sends JSON lines `{id, method, params}` to the port and reads `{id, result}`.
//! `FAKE_CLAUDE_SILENT` makes the fake read its input and answer nothing, not even `initialize`,
//! so a thread stays starting and its log holds only what the engine appends before a process is
//! up.
//! `get_usage` answers a plan read that a test changes with `test/usage` (see `usage_answer`), or
//! refuses it as a CLI without the control does with `test/usageUnsupported`.
//! `FAKE_CLAUDE_FAIL_START` names a file: while it exists, a start exits with the exit code it
//! holds and a "Not logged in" message, so a test chooses which starts fail by writing the file.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

use crate::{fail, initialize_response, reply, send, usage_answer};

/// How the fake answers an `interrupt` control.
#[derive(Clone, Copy, PartialEq)]
enum Interrupts {
    Acknowledge,
    /// Never answers, as a CLI that stopped listening.
    Hang,
    Reject,
}

/// How the fake answers a `get_usage` control.
enum Usage {
    /// The window the test last set with `test/usage`, `{}` until it sets one.
    Answer(Value),
    Unsupported,
}

struct Script {
    initializing: Option<Value>,
    received: Vec<Value>,
    interrupts: Interrupts,
    usage: Usage,
}

type Shared = Arc<Mutex<Script>>;

fn lock(script: &Shared) -> MutexGuard<'_, Script> {
    script.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn run(log: &Path) {
    if let Some(code) = failing_start() {
        eprintln!("Not logged in");
        std::process::exit(code);
    }
    let script: Shared = Arc::new(Mutex::new(Script {
        initializing: None,
        received: Vec::new(),
        interrupts: Interrupts::Acknowledge,
        usage: Usage::Answer(json!({})),
    }));
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a localhost port is free");
    let port = listener.local_addr().expect("a bound port").port();
    let shared = script.clone();
    std::thread::spawn(move || serve_controls(&listener, &shared));
    log_start(log, port);
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.expect("stdin remains readable");
        let message: Value = serde_json::from_str(&line).expect("the engine sends JSON");
        handle(&script, message);
    }
}

fn failing_start() -> Option<i32> {
    let flag = std::env::var_os("FAKE_CLAUDE_FAIL_START")?;
    std::fs::read_to_string(flag).ok()?.trim().parse().ok()
}

fn log_start(log: &Path, port: u16) {
    let line = json!({
        "pid": std::process::id(),
        "port": port,
        "arguments": std::env::args().skip(1).collect::<Vec<_>>(),
        "cwd": std::env::current_dir().expect("a working folder").display().to_string(),
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .expect("the start log is writable");
    // One write, so concurrent starts never interleave a line.
    file.write_all(format!("{line}\n").as_bytes())
        .expect("the start log is writable");
}

fn handle(script: &Shared, message: Value) {
    lock(script).received.push(message.clone());
    if message["type"] == "control_response"
        && message["response"]["request_id"] == "fake-sdk-initialize"
    {
        let initialize = lock(script)
            .initializing
            .take()
            .expect("an SDK initialization is pending");
        let result = &message["response"]["response"]["mcp_response"]["result"];
        if result["serverInfo"]["name"] != "kratos" || result["protocolVersion"] != "2025-11-25" {
            fail(&initialize, "invalid Kratos SDK initialization response");
        } else {
            send(
                &json!({ "type": "control_request", "request_id": "fake-sdk-initialized", "request": {
                    "subtype": "mcp_message", "server_name": "kratos", "message": { "jsonrpc": "2.0", "method": "notifications/initialized" }
                }}),
            );
            reply(&initialize, initialize_response());
        }
        return;
    }
    if message["type"] == "control_request" && std::env::var_os("FAKE_CLAUDE_SILENT").is_none() {
        answer(script, message);
    }
}

fn answer(script: &Shared, message: Value) {
    match message["request"]["subtype"].as_str() {
        Some("initialize") if message["request"]["sdkMcpServers"] == json!(["kratos"]) => {
            lock(script).initializing = Some(message);
            send(
                &json!({ "type": "control_request", "request_id": "fake-sdk-initialize", "request": {
                    "subtype": "mcp_message", "server_name": "kratos", "message": { "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2025-11-25" } }
                }}),
            );
        }
        Some("initialize") => reply(&message, initialize_response()),
        Some("interrupt") => interrupt(script, &message),
        Some("get_usage") => match &lock(script).usage {
            Usage::Answer(window) => reply(&message, usage_answer(window)),
            Usage::Unsupported => fail(&message, "Unsupported control request subtype: get_usage"),
        },
        _ => fail(&message, "unsupported fake control"),
    }
}

fn interrupt(script: &Shared, message: &Value) {
    let mode = lock(script).interrupts;
    match mode {
        Interrupts::Acknowledge => reply(message, json!({"still_queued": []})),
        Interrupts::Hang => {}
        Interrupts::Reject => fail(message, "Interrupt rejected"),
    }
}

fn serve_controls(listener: &TcpListener, script: &Shared) {
    for stream in listener.incoming().flatten() {
        let script = script.clone();
        std::thread::spawn(move || control_connection(stream, &script));
    }
}

fn control_connection(stream: TcpStream, script: &Shared) {
    let mut writer = stream.try_clone().expect("the stream clones");
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        let request: Value = serde_json::from_str(&line).expect("a test sends JSON");
        let result = control(script, &request["method"], &request["params"]);
        let answer = json!({ "id": request["id"], "result": result });
        if writeln!(writer, "{answer}").is_err() {
            return;
        }
    }
}

fn control(script: &Shared, method: &Value, params: &Value) -> Value {
    match method.as_str() {
        Some("test/received") => json!(lock(script).received),
        Some("test/emit") => {
            send(&params["message"]);
            json!({})
        }
        Some("test/ask") => {
            send(
                &json!({"type": "control_request", "request_id": params["requestId"],
                "request": params["request"]}),
            );
            json!({})
        }
        Some("test/interrupts") => {
            lock(script).interrupts = match params["mode"].as_str() {
                Some("hang") => Interrupts::Hang,
                Some("reject") => Interrupts::Reject,
                _ => Interrupts::Acknowledge,
            };
            json!({})
        }
        Some("test/usage") => {
            lock(script).usage = Usage::Answer(params.clone());
            json!({})
        }
        Some("test/usageUnsupported") => {
            lock(script).usage = Usage::Unsupported;
            json!({})
        }
        Some("test/exit") => std::process::exit(params["code"].as_i64().unwrap_or(1) as i32),
        other => json!({ "error": format!("unknown control {other:?}") }),
    }
}
