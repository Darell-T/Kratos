//! `kratos-engine mcp` is the Model Context Protocol server Codex starts for each Kratos thread.
//! It turns each tool call into requests to the running engine's API with the client token.
//!
//! Codex starts it outside the agent's sandbox, so it can read the token that sandboxed commands
//! cannot. Each request names the agent's thread, and the engine refuses what that thread may not
//! do (see `server::acting`).

mod tools;
mod transcript;
mod wait;

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use kratos_client::Client;
use kratos_codex_client::RequestId;
use kratos_protocol::ThreadId;
use serde::Deserialize;
use serde_json::{Value, json};

pub const MAX_WAIT_SECONDS: u64 = 1800;
/// The variable that names the thread whose agent the server serves.
pub const THREAD_ID_VAR: &str = "KRATOS_THREAD_ID";

const PROTOCOL_VERSION: &str = "2025-06-18";
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

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

/// A JSON-RPC message from Codex. One without an id is a notification, which gets no answer.
#[derive(Deserialize)]
struct RpcRequest {
    id: Option<RequestId>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cancelled {
    request_id: RequestId,
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
            let request: RpcRequest = match serde_json::from_str(&line) {
                Ok(request) => request,
                Err(error) => {
                    let code = if error.is_data() {
                        INVALID_REQUEST
                    } else {
                        PARSE_ERROR
                    };
                    write(&output, &failure(None, code, &error.to_string()))?;
                    continue;
                }
            };
            if request.method == "notifications/cancelled" {
                // A notification gets no answer, so one naming no request is dropped.
                if let Ok(Cancelled { request_id }) = serde_json::from_value(request.params) {
                    running.cancel(&request_id);
                }
                continue;
            }
            let Some(id) = request.id.clone() else {
                continue;
            };
            let cancelled = running.start(&id);
            let (output, running) = (&output, &running);
            scope.spawn(move || {
                let call = Call {
                    caller,
                    cancelled: &cancelled,
                };
                let answer = answer(&call, request);
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
struct Running(Mutex<HashMap<RequestId, Arc<AtomicBool>>>);

impl Running {
    fn start(&self, id: &RequestId) -> Arc<AtomicBool> {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.lock().insert(id.clone(), cancelled.clone());
        cancelled
    }

    fn cancel(&self, id: &RequestId) {
        if let Some(cancelled) = self.lock().get(id) {
            cancelled.store(true, Ordering::Relaxed);
        }
    }

    fn finish(&self, id: &RequestId) {
        self.lock().remove(id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RequestId, Arc<AtomicBool>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn write(output: &Mutex<impl Write>, message: &Value) -> io::Result<()> {
    let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);
    writeln!(output, "{message}")?;
    output.flush()
}

fn answer(call: &Call, request: RpcRequest) -> Value {
    let RpcRequest { id, method, params } = request;
    let result = match method.as_str() {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "kratos", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools::list() }),
        "tools/call" => match serde_json::from_value(params) {
            Ok(tool_call) => tools::call(call, tool_call),
            Err(error) => return failure(id, INVALID_PARAMS, &error.to_string()),
        },
        method => return failure(id, METHOD_NOT_FOUND, &format!("unknown method {method}")),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: Option<RequestId>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_of_the_wrong_shape_answers_a_json_rpc_error() {
        // Catches a tools/call without a tool name run as a call to the tool named "", and a
        // message with no method answered as a call to an unknown method.
        let client = Client::new("http://127.0.0.1:9").unwrap();
        let caller = Caller::new(client, ThreadId::new("t".to_owned()));
        let input = [
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":2}"#,
            "not json",
        ]
        .join("\n");
        let mut output = Vec::new();
        serve(&caller, input.as_bytes(), &mut output).unwrap();

        let mut answers: Vec<(Value, i64)> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .map(|answer| {
                (
                    answer["id"].clone(),
                    answer["error"]["code"].as_i64().unwrap(),
                )
            })
            .collect();
        answers.sort_by_key(|(_, code)| *code);
        assert_eq!(
            answers,
            [
                (Value::Null, PARSE_ERROR),
                (json!(1), INVALID_PARAMS),
                (Value::Null, INVALID_REQUEST),
            ]
        );
    }
}
