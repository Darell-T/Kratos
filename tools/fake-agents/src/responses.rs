//! A scripted Responses API for real-Codex integration tests. Only model responses are fake:
//! Codex still chooses its tools, asks Kratos for approval, and executes through its sandbox.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

/// One response in a deterministic model script.
pub enum Reply {
    Command(String),
    Text(String),
}

impl Reply {
    pub fn command(command: &str) -> Self {
        Self::Command(command.to_owned())
    }
}

#[derive(Default)]
struct Script {
    replies: VecDeque<Reply>,
    requests: Vec<Value>,
}

/// Owns a localhost server. Dropping it stops accepting requests.
pub struct Server {
    address: SocketAddr,
    script: Arc<Mutex<Script>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    pub async fn start(replies: impl IntoIterator<Item = Reply>) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let script = Arc::new(Mutex::new(Script {
            replies: replies.into_iter().collect(),
            requests: Vec::new(),
        }));
        let router = Router::new()
            .route("/v1/responses", post(respond))
            .with_state(script.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("the test model server runs");
        });
        Ok(Self {
            address,
            script,
            task,
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    pub fn enqueue(&self, replies: impl IntoIterator<Item = Reply>) {
        self.script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replies
            .extend(replies);
    }

    /// Full request bodies, including the tools Codex advertised and subsequent tool results.
    pub fn requests(&self) -> Vec<Value> {
        self.script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .requests
            .clone()
    }

    /// Tool results returned to the model, deduplicated by call id across repeated input history.
    pub fn tool_outputs(&self) -> Vec<Value> {
        let mut outputs = std::collections::BTreeMap::new();
        for request in self.requests() {
            for item in request["input"].as_array().into_iter().flatten() {
                if item["type"] == "custom_tool_call_output" {
                    outputs.insert(
                        item["call_id"].as_str().unwrap_or_default().to_owned(),
                        item.clone(),
                    );
                }
            }
        }
        outputs.into_values().collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn respond(State(script): State<Arc<Mutex<Script>>>, body: Bytes) -> Response {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let (reply, number) = {
        let mut script = script.lock().unwrap_or_else(PoisonError::into_inner);
        script.requests.push(request);
        (script.replies.pop_front(), script.requests.len())
    };
    let Some(reply) = reply else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "the scripted model has no response left",
        )
            .into_response();
    };
    let response_id = format!("resp-{number}");
    let item_id = format!("item-{number}");
    let item = match reply {
        Reply::Command(command) => {
            let arguments =
                json!({ "cmd": command, "yield_time_ms": 10000, "max_output_tokens": 2000 });
            json!({
                "type": "custom_tool_call", "id": item_id, "call_id": format!("call-{number}"),
                "namespace": "functions", "name": "exec",
                "input": format!("text(await tools.exec_command({arguments}));"),
            })
        }
        Reply::Text(text) => json!({
            "type": "message", "id": item_id, "role": "assistant", "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        }),
    };
    let events = [
        json!({ "type": "response.created", "response": { "id": response_id, "status": "in_progress", "output": [] } }),
        json!({ "type": "response.output_item.added", "output_index": 0, "item": item }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": item }),
        json!({ "type": "response.completed", "response": {
            "id": response_id, "status": "completed", "output": [item],
            "usage": { "input_tokens": 10, "output_tokens": 1, "total_tokens": 11 },
        } }),
    ];
    let stream: String = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    ([(header::CONTENT_TYPE, "text/event-stream")], stream).into_response()
}
