//! `kratos-engine mcp` is the Model Context Protocol server Codex starts for each Kratos thread.
//! It turns each tool call into requests to the running engine's API with the client token.
//!
//! Codex starts it outside the agent's sandbox, so it can read the token that sandboxed commands
//! cannot. Each request names the agent's thread, and the engine refuses what that thread may not
//! do (see `server::acting`).

mod tools;
mod transcript;

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use kratos_client::Client;
use kratos_protocol::ThreadId;
use serde_json::{Value, json};

pub const MAX_WAIT_SECONDS: u64 = 1800;
/// The variable that names the thread whose agent the server serves.
pub const THREAD_ID_VAR: &str = "KRATOS_THREAD_ID";

const PROTOCOL_VERSION: &str = "2025-06-18";
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;

const INSTRUCTIONS: &str = "Tools for working with the other threads in Kratos: list and read \
threads, start new ones, send them messages, wait for their turns, and answer what they ask. The \
receiver of a message answers in its own thread, so read the answer with wait_for_turn or \
read_thread.";

pub struct Caller {
    client: Client,
    thread_id: ThreadId,
}

impl Caller {
    pub fn new(client: Client, thread_id: ThreadId) -> Caller {
        Caller { client, thread_id }
    }
}

/// One request being answered, and whether Codex has cancelled it.
struct Call<'a> {
    caller: &'a Caller,
    cancelled: &'a AtomicBool,
}

/// Serves until `input` ends. Each request is answered on its own thread, so a long
/// `wait_for_turn` holds up no other call, and `notifications/cancelled` ends the wait it names.
pub fn serve(caller: &Caller, input: impl BufRead, output: impl Write + Send) -> io::Result<()> {
    let output = Mutex::new(output);
    let running = Running::default();
    thread::scope(|scope| {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let request: Value = match serde_json::from_str(&line) {
                Ok(request) => request,
                Err(error) => {
                    let failed = failure(Value::Null, PARSE_ERROR, &error.to_string());
                    write(&output, &failed)?;
                    continue;
                }
            };
            if request["method"] == "notifications/cancelled" {
                running.cancel(&request["params"]["requestId"]);
                continue;
            }
            let Some(id) = request.get("id").cloned() else {
                continue;
            };
            let cancelled = running.start(&id);
            let (output, running) = (&output, &running);
            scope.spawn(move || {
                let call = Call {
                    caller,
                    cancelled: &cancelled,
                };
                let answer = answer(&call, id.clone(), &request);
                running.finish(&id);
                // A closed stdout means Codex has gone, and the loop above ends at its EOF.
                let _ = write(output, &answer);
            });
        }
        Ok(())
    })
}

/// The requests being answered, by their JSON-RPC id.
#[derive(Default)]
struct Running(Mutex<HashMap<String, Arc<AtomicBool>>>);

impl Running {
    fn start(&self, id: &Value) -> Arc<AtomicBool> {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.lock().insert(id.to_string(), cancelled.clone());
        cancelled
    }

    fn cancel(&self, id: &Value) {
        if let Some(cancelled) = self.lock().get(&id.to_string()) {
            cancelled.store(true, Ordering::Relaxed);
        }
    }

    fn finish(&self, id: &Value) {
        self.lock().remove(&id.to_string());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<AtomicBool>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn write(output: &Mutex<impl Write>, message: &Value) -> io::Result<()> {
    let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);
    writeln!(output, "{message}")?;
    output.flush()
}

fn answer(call: &Call, id: Value, request: &Value) -> Value {
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match request["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "kratos", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools::list() }),
        "tools/call" => tools::call(call, &params),
        method => return failure(id, METHOD_NOT_FOUND, &format!("unknown method {method}")),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
