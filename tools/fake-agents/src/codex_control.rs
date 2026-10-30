//! What a test sends `fake-codex` to steer it, and the file where the fake writes its control port.
//!
//! A test sends `test/*` requests as JSON-RPC lines, on the fake's stdin or on the localhost port the
//! fake writes to its `--control-file`. Each builder below returns its method with the params
//! `fake-codex` reads for it, and the fake dispatches on the same method names, so the tests that
//! send them spell neither again.

use std::io;
use std::path::Path;

use serde_json::{Value, json};

pub const SETTLE_TURN: &str = "test/settleTurn";
pub const ASK: &str = "test/ask";
pub const EMIT: &str = "test/emit";

/// One `test/*` request to the fake.
pub struct ControlRequest {
    pub method: &'static str,
    pub params: Value,
}

/// Answers the oldest request of `method` the fake holds with `result`. With `thread_id`, only a
/// request for that Codex thread matches.
pub fn settle(method: &str, result: Value, thread_id: Option<&str>) -> ControlRequest {
    let mut params = json!({ "method": method, "result": result });
    if let Some(thread_id) = thread_id {
        params["threadId"] = json!(thread_id);
    }
    ControlRequest {
        method: SETTLE_TURN,
        params,
    }
}

/// Answers the oldest request of `method` the fake holds with JSON-RPC error `code`.
pub fn refuse(method: &str, code: i64, error: &str) -> ControlRequest {
    ControlRequest {
        method: SETTLE_TURN,
        params: json!({ "method": method, "code": code, "error": error }),
    }
}

/// Answers the oldest held `turn/start` with turn `turn_id` in `status`, such as `inProgress`.
pub fn settle_start(turn_id: &str, status: &str, thread_id: Option<&str>) -> ControlRequest {
    settle(
        "turn/start",
        json!({ "turn": turn(turn_id, status) }),
        thread_id,
    )
}

/// Makes the fake send its client request `method` under Codex's id `request_id`, or `ask-1`
/// without one.
pub fn ask(request_id: Option<Value>, method: &str, params: Value) -> ControlRequest {
    let mut ask = json!({ "method": method, "params": params });
    if let Some(request_id) = request_id {
        ask["requestId"] = request_id;
    }
    ControlRequest {
        method: ASK,
        params: ask,
    }
}

/// Makes the fake send its client notification `method`.
pub fn emit(method: &str, params: Value) -> ControlRequest {
    ControlRequest {
        method: EMIT,
        params: json!({ "method": method, "params": params }),
    }
}

/// Ends turn `turn_id` of Codex thread `thread_id` with `status`. The fake also records the ending
/// in that thread's history.
pub fn complete(thread_id: &str, turn_id: &str, status: &str) -> ControlRequest {
    emit(
        "turn/completed",
        json!({ "threadId": thread_id, "turn": turn(turn_id, status) }),
    )
}

fn turn(id: &str, status: &str) -> Value {
    json!({ "id": id, "items": [], "status": status })
}

/// Writes `port` to `control_file` whole and renamed into place, so a reader that sees the file
/// reads the whole port.
pub fn write_port(control_file: &Path, port: u16) -> io::Result<()> {
    let mut partial = control_file.as_os_str().to_owned();
    partial.push(".partial");
    std::fs::write(&partial, port.to_string())?;
    std::fs::rename(&partial, control_file)
}

/// The port the fake wrote to `control_file`. Fails with `NotFound` until the fake has started.
pub fn read_port(control_file: &Path) -> io::Result<u16> {
    let text = std::fs::read_to_string(control_file)?;
    text.trim().parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} holds no port: {text:?}", control_file.display()),
        )
    })
}
