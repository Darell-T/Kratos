//! What a test sends `fake-codex` to steer it, and the file where the fake writes its control port.
//!
//! A test sends `test/*` requests as JSON-RPC lines, on the fake's stdin or on the localhost port the
//! fake writes to its `--control-file`. The params below are the shapes `fake-codex` reads, so the
//! tests that send them do not spell them again.

use std::io;
use std::path::Path;

use serde_json::{Value, json};

/// `test/settleTurn` params that answer the oldest held `turn/start` with turn `turn_id` in
/// `status`, such as `inProgress`. With `thread_id`, only a request for that Codex thread matches.
pub fn settle_start(turn_id: &str, status: &str, thread_id: Option<&str>) -> Value {
    let turn = json!({ "id": turn_id, "items": [], "status": status });
    let mut params = json!({ "method": "turn/start", "result": { "turn": turn } });
    if let Some(thread_id) = thread_id {
        params["threadId"] = json!(thread_id);
    }
    params
}

/// `test/settleTurn` params that answer the oldest held `turn/start` with JSON-RPC error `code`.
pub fn fail_start(code: i64, error: &str) -> Value {
    json!({ "method": "turn/start", "code": code, "error": error })
}

/// `test/ask` params that make the fake send its client request `method` under Codex's id
/// `request_id`.
pub fn ask(request_id: &str, method: &str, params: Value) -> Value {
    json!({ "requestId": request_id, "method": method, "params": params })
}

/// `test/emit` params that make the fake send its client notification `method`.
pub fn emit(method: &str, params: Value) -> Value {
    json!({ "method": method, "params": params })
}

/// `test/emit` params that end turn `turn_id` of Codex thread `thread_id` with `status`. The fake
/// also records the ending in that thread's history.
pub fn complete(thread_id: &str, turn_id: &str, status: &str) -> Value {
    let turn = json!({ "id": turn_id, "items": [], "status": status });
    emit(
        "turn/completed",
        json!({ "threadId": thread_id, "turn": turn }),
    )
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
