//! One JSON-RPC connection to one `codex app-server` process, one JSON object per line.
//!
//! Three tasks run the connection. The writer owns the server's stdin, so every message goes out
//! whole and in order. The reader parses stdout: it settles answered requests, answers server
//! requests no one can handle, and forwards the rest as [`Incoming`]. The keeper owns the process:
//! when the process exits it ends the process tree, so a tool the agent started cannot keep the
//! output pipe open, and reports the exit to the reader.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kratos_process::OwnedChild;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch};

use crate::protocol::{Method, Notification, ServerRequest, Unhandled, methods};
use crate::{INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, types};

/// Messages that wait for the consumer. When it falls this far behind, the reader stops reading,
/// and the server waits on its own output: memory stays bounded and nothing is dropped.
const INCOMING_CAPACITY: usize = 256;
/// The last lines the server wrote to stderr, kept for the error when it fails.
const STDERR_LINES_KEPT: usize = 20;

/// How Kratos starts the server and talks to it.
#[derive(Clone, Debug)]
pub struct Launch {
    pub program: PathBuf,
    /// The arguments after the program, starting with `app-server`.
    pub args: Vec<OsString>,
    /// The server's working folder. Under a legacy sandbox policy Codex makes it writable, so it
    /// is a folder no agent should write.
    pub cwd: PathBuf,
    /// Variables removed from the environment the server inherits.
    pub env_remove: Vec<OsString>,
    pub env_set: Vec<(OsString, OsString)>,
    pub client_info: types::ClientInfo,
    /// The deadline of a request that names none.
    pub request_timeout: Duration,
    /// How long [`Codex::close`] waits for the server to exit once its input closes, before it ends
    /// the process tree.
    pub close_timeout: Duration,
}

/// A JSON-RPC id. Kratos numbers its own requests; the server may use strings for its own.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    Text(String),
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestId::Number(number) => write!(formatter, "{number}"),
            RequestId::Text(text) => formatter.write_str(text),
        }
    }
}

impl From<&types::RequestId> for RequestId {
    fn from(id: &types::RequestId) -> RequestId {
        match id {
            types::RequestId::Int64(number) => RequestId::Number(*number),
            types::RequestId::String(text) => RequestId::Text(text.clone()),
        }
    }
}

/// How the server ended. `code` is `None` when no exit code exists, such as a kill by signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
    /// The last lines the server wrote to stderr before it ended.
    pub stderr: Vec<String>,
}

impl fmt::Display for Exit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "Codex exited (code {code})")?,
            None => formatter.write_str("Codex exited (code unknown)")?,
        }
        if let Some(last) = self.stderr.last() {
            write!(formatter, ": {last}")?;
        }
        Ok(())
    }
}

/// What the server sends that the connection does not settle itself.
#[derive(Debug)]
pub enum Incoming {
    Notification(Notification),
    Request(IncomingRequest),
    /// The server ended. Nothing follows it.
    Exited(Exit),
}

/// A server request that waits for an answer. Answer it once with [`IncomingRequest::answer`] or
/// [`IncomingRequest::fail`]. Dropping it unanswered answers the server with an internal error,
/// so the server never waits on a request Kratos forgot.
#[derive(Debug)]
pub struct IncomingRequest {
    pub id: RequestId,
    pub request: ServerRequest,
    responder: Option<Responder>,
}

impl IncomingRequest {
    /// Sends `result` unless the server already withdrew the request with
    /// `serverRequest/resolved`, or the connection ended. Returns whether it was sent.
    pub fn answer<T: Serialize>(mut self, result: &T) -> bool {
        let responder = self.responder.take().expect("answered at most once");
        let result = serde_json::to_value(result).expect("answer types serialize");
        responder.send(&self.id, Ok(result))
    }

    pub fn fail(mut self, code: i64, message: &str) -> bool {
        let responder = self.responder.take().expect("answered at most once");
        responder.send(&self.id, Err((code, message.to_owned())))
    }
}

impl Drop for IncomingRequest {
    fn drop(&mut self) {
        if let Some(responder) = self.responder.take() {
            responder.send(
                &self.id,
                Err((
                    INTERNAL_ERROR,
                    "Kratos dropped the request unanswered".to_owned(),
                )),
            );
        }
    }
}

#[derive(Debug)]
struct Responder {
    shared: Arc<Shared>,
}

impl Responder {
    fn send(&self, id: &RequestId, outcome: Result<Value, (i64, String)>) -> bool {
        if !self.shared.state().unanswered.remove(id) {
            return false;
        }
        let message = match outcome {
            Ok(result) => json!({ "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "id": id, "error": { "code": code, "message": message } })
            }
        };
        self.shared.write(&message)
    }
}

/// A handle to the connection. Clones share it.
#[derive(Clone, Debug)]
pub struct Codex {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    writer: mpsc::UnboundedSender<Write>,
    next_id: AtomicI64,
    state: Mutex<State>,
    request_timeout: Duration,
    close_timeout: Duration,
    close: Mutex<Option<oneshot::Sender<Duration>>>,
    exit: watch::Receiver<Option<Exit>>,
    info: Mutex<Option<types::InitializeResponse>>,
}

#[derive(Debug, Default)]
struct State {
    /// Requests sent and not yet answered, by id.
    pending: HashMap<i64, oneshot::Sender<Result<Value, RequestError>>>,
    /// Server requests forwarded and not yet answered or withdrawn.
    unanswered: HashSet<RequestId>,
    /// Set once the server ended. Every later request fails with it.
    ended: Option<Exit>,
}

#[derive(Debug)]
enum Write {
    Line(String),
    /// Closes the server's stdin, which asks it to exit.
    Close,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        // No code panics while holding this lock, and every update leaves the state consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues one line. False when the writer has stopped, because the server is gone.
    fn write(&self, message: &Value) -> bool {
        let line = serde_json::to_string(message).expect("a JSON value serializes");
        self.writer.send(Write::Line(line)).is_ok()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// The server answered with a JSON-RPC error.
    #[error("{message}")]
    Rpc { code: i64, message: String },
    #[error("Codex {method} timed out after {} ms", .after.as_millis())]
    Timeout { method: String, after: Duration },
    #[error("{0}")]
    Exited(Exit),
    /// The server's answer is not the shape the schema gives it.
    #[error("Codex answered {method} with a result this build cannot read: {error}")]
    Unreadable { method: String, error: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error(
        "Could not find {}. Install Codex with \"npm install -g @openai/codex\" or set CODEX_EXE.",
        .0.display()
    )]
    NotFound(PathBuf),
    #[error("cannot start {}: {source}", .program.display())]
    Spawn {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Codex did not complete the handshake: {0}")]
    Handshake(#[source] RequestError),
}

/// Starts the server, performs the handshake, and returns the handle and the server's messages.
/// The server never runs outside its process tree: a failed handshake ends it before returning.
pub async fn connect(
    launch: Launch,
    capabilities: types::InitializeCapabilities,
) -> Result<(Codex, mpsc::Receiver<Incoming>), ConnectError> {
    let mut child = spawn(&launch)?;
    let stdin = child.child().stdin.take().expect("stdin is piped");
    let stdout = child.child().stdout.take().expect("stdout is piped");
    let stderr = child.child().stderr.take().expect("stderr is piped");

    let (writer, writes) = mpsc::unbounded_channel();
    let (exit_sender, exit) = watch::channel(None);
    let (close_sender, close_requested) = oneshot::channel();
    let (incoming_sender, incoming) = mpsc::channel(INCOMING_CAPACITY);
    let shared = Arc::new(Shared {
        writer,
        next_id: AtomicI64::new(1),
        state: Mutex::new(State::default()),
        request_timeout: launch.request_timeout,
        close_timeout: launch.close_timeout,
        close: Mutex::new(Some(close_sender)),
        exit,
        info: Mutex::new(None),
    });
    let stderr_lines = Arc::new(Mutex::new(VecDeque::new()));

    tokio::spawn(write_lines(stdin, writes));
    tokio::spawn(keep_stderr(stderr, stderr_lines.clone()));
    let (exited_sender, exited) = oneshot::channel();
    tokio::spawn(keep(child, close_requested, exited_sender));
    tokio::spawn(read(
        Reader {
            shared: shared.clone(),
            incoming: incoming_sender,
            exit_sender,
            stderr_lines,
        },
        stdout,
        exited,
    ));

    let codex = Codex { shared };
    let params = types::InitializeParams {
        capabilities: Some(capabilities),
        client_info: launch.client_info.clone(),
    };
    match codex.request::<methods::Initialize>(&params).await {
        Ok(info) => {
            *codex
                .shared
                .info
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(info);
            codex.shared.write(&json!({ "method": "initialized" }));
            Ok((codex, incoming))
        }
        Err(error) => {
            codex.close().await;
            Err(ConnectError::Handshake(error))
        }
    }
}

fn spawn(launch: &Launch) -> Result<OwnedChild, ConnectError> {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .current_dir(&launch.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in &launch.env_remove {
        command.env_remove(name);
    }
    for (name, value) in &launch.env_set {
        command.env(name, value);
    }
    OwnedChild::spawn(command).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => ConnectError::NotFound(launch.program.clone()),
        _ => ConnectError::Spawn {
            program: launch.program.clone(),
            source,
        },
    })
}

impl Codex {
    /// What the server said about itself in the handshake.
    pub fn info(&self) -> types::InitializeResponse {
        self.shared
            .info
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .expect("connect stores the handshake before it returns a handle")
    }

    pub async fn request<M: Method>(
        &self,
        params: &M::Params,
    ) -> Result<M::Response, RequestError> {
        self.request_within::<M>(params, self.shared.request_timeout)
            .await
    }

    pub async fn request_within<M: Method>(
        &self,
        params: &M::Params,
        timeout: Duration,
    ) -> Result<M::Response, RequestError> {
        let params = serde_json::to_value(params).expect("request parameters serialize");
        let result = self.request_value(M::NAME, params, timeout).await?;
        serde_json::from_value(result).map_err(|error| RequestError::Unreadable {
            method: M::NAME.to_owned(),
            error: error.to_string(),
        })
    }

    /// A request by method name, for methods outside [`methods`], such as a fake server's test
    /// controls.
    pub async fn request_value(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (settle, settled) = oneshot::channel();
        {
            // Checked under the same lock the reader holds when it records the exit, so a request
            // either sees the exit or is in `pending` when the reader fails every pending request.
            let mut state = self.shared.state();
            if let Some(exit) = &state.ended {
                return Err(RequestError::Exited(exit.clone()));
            }
            state.pending.insert(id, settle);
        }
        let message = json!({ "id": id, "method": method, "params": params });
        if !self.shared.write(&message) {
            self.shared.state().pending.remove(&id);
            return Err(self.ended());
        }
        match tokio::time::timeout(timeout, settled).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_dropped)) => Err(self.ended()),
            Err(_elapsed) => {
                self.shared.state().pending.remove(&id);
                Err(RequestError::Timeout {
                    method: method.to_owned(),
                    after: timeout,
                })
            }
        }
    }

    /// The error for a request made after the server ended. The exit may still be on its way
    /// from the reader, so an unknown exit reads as one with no code.
    fn ended(&self) -> RequestError {
        let exit = self.shared.state().ended.clone().unwrap_or(Exit {
            code: None,
            stderr: Vec::new(),
        });
        RequestError::Exited(exit)
    }

    /// Asks the server to exit by closing its input, waits up to the close timeout, then ends the
    /// whole process tree. Returns once the server has exited; calling it again only waits.
    pub async fn close(&self) {
        let close = self
            .shared
            .close
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(close) = close {
            let _ = self.shared.writer.send(Write::Close);
            let _ = close.send(self.shared.close_timeout);
        }
        let mut exit = self.shared.exit.clone();
        let _ = exit.wait_for(Option::is_some).await;
    }

    /// The exit, once the server has ended.
    pub fn exit(&self) -> Option<Exit> {
        self.shared.exit.borrow().clone()
    }
}

async fn write_lines(mut stdin: ChildStdin, mut writes: mpsc::UnboundedReceiver<Write>) {
    while let Some(write) = writes.recv().await {
        let Write::Line(mut line) = write else { return };
        line.push('\n');
        // A failed write means the server is gone; the reader reports the exit.
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            return;
        }
    }
}

async fn keep_stderr(stderr: ChildStderr, kept: Arc<Mutex<VecDeque<String>>>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("codex: {line}");
        let mut kept = kept.lock().unwrap_or_else(PoisonError::into_inner);
        if kept.len() == STDERR_LINES_KEPT {
            kept.pop_front();
        }
        kept.push_back(line);
    }
}

/// Owns the process. Ends the tree once the server exits on its own, or after a requested close
/// once the server exits or the close timeout passes.
async fn keep(
    mut child: OwnedChild,
    close_requested: oneshot::Receiver<Duration>,
    exited: oneshot::Sender<Option<i32>>,
) {
    let status = tokio::select! {
        status = child.wait() => status,
        requested = close_requested => {
            match requested {
                Ok(timeout) => match tokio::time::timeout(timeout, child.wait()).await {
                    Ok(status) => status,
                    Err(_elapsed) => {
                        let _ = child.end_tree();
                        child.wait().await
                    }
                },
                // The handle went away without closing: nothing will ask again, so stop now.
                Err(_) => {
                    let _ = child.end_tree();
                    child.wait().await
                }
            }
        }
    };
    // Before the exit is reported, so a caller that waits on close knows no process of the tree
    // still runs, for example before it deletes the folder those processes worked in.
    let _ = child.end_tree();
    let _ = exited.send(status.ok().and_then(|status| status.code()));
}

struct Reader {
    shared: Arc<Shared>,
    incoming: mpsc::Sender<Incoming>,
    exit_sender: watch::Sender<Option<Exit>>,
    stderr_lines: Arc<Mutex<VecDeque<String>>>,
}

async fn read(reader: Reader, stdout: ChildStdout, exited: oneshot::Receiver<Option<i32>>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        // The server logs to stderr, but a stray line on stdout must not end the connection.
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        reader.dispatch(message).await;
    }
    let code = exited.await.unwrap_or(None);
    reader.finish(code).await;
}

impl Reader {
    async fn dispatch(&self, mut message: Value) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message
            .get("id")
            .and_then(|id| serde_json::from_value::<RequestId>(id.clone()).ok());
        let params = message
            .get_mut("params")
            .map(Value::take)
            .unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => self.server_request(id, &method, params).await,
            (Some(method), None) => self.notification(&method, params).await,
            (None, Some(RequestId::Number(id))) => self.settle(id, message),
            // An answer to a string id Kratos never sent, or something that is not JSON-RPC.
            (None, _) => {}
        }
    }

    async fn server_request(&self, id: RequestId, method: &str, params: Value) {
        match ServerRequest::parse(method, params) {
            Ok(request) => {
                self.shared.state().unanswered.insert(id.clone());
                let request = IncomingRequest {
                    id,
                    request,
                    responder: Some(Responder {
                        shared: self.shared.clone(),
                    }),
                };
                // A consumer that stopped listening drops the request, which answers it.
                let _ = self.incoming.send(Incoming::Request(request)).await;
            }
            Err(Unhandled::UnknownMethod) => {
                let message = format!("Method not handled by Kratos: {method}");
                self.shared.write(
                    &json!({ "id": id, "error": { "code": METHOD_NOT_FOUND, "message": message } }),
                );
            }
            Err(Unhandled::BadParams(error)) => {
                eprintln!("codex: cannot read the parameters of {method}: {error}");
                let message = format!("Kratos cannot read the parameters of {method}: {error}");
                self.shared.write(
                    &json!({ "id": id, "error": { "code": INVALID_PARAMS, "message": message } }),
                );
            }
        }
    }

    async fn notification(&self, method: &str, params: Value) {
        let notification = Notification::parse(method, params);
        // Codex no longer needs the answer, so an answer Kratos sends later is dropped.
        if let Notification::ServerRequestResolved(resolved) = &notification {
            self.shared
                .state()
                .unanswered
                .remove(&RequestId::from(&resolved.request_id));
        }
        let _ = self
            .incoming
            .send(Incoming::Notification(notification))
            .await;
    }

    fn settle(&self, id: i64, message: Value) {
        let Some(settle) = self.shared.state().pending.remove(&id) else {
            // Its request already timed out.
            return;
        };
        let outcome = match message.get("error") {
            Some(error) => Err(RequestError::Rpc {
                code: error
                    .get("code")
                    .and_then(Value::as_i64)
                    .unwrap_or(INTERNAL_ERROR),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| error.to_string(), str::to_owned),
            }),
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = settle.send(outcome);
    }

    /// Records the exit, then fails every pending request with it, so a request made after this
    /// point fails at once instead of waiting for its deadline.
    async fn finish(self, code: Option<i32>) {
        let stderr = self
            .stderr_lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect();
        let exit = Exit { code, stderr };
        let pending = {
            let mut state = self.shared.state();
            state.ended = Some(exit.clone());
            state.unanswered.clear();
            std::mem::take(&mut state.pending)
        };
        for settle in pending.into_values() {
            let _ = settle.send(Err(RequestError::Exited(exit.clone())));
        }
        let _ = self.shared.writer.send(Write::Close);
        self.exit_sender.send_replace(Some(exit.clone()));
        let _ = self.incoming.send(Incoming::Exited(exit)).await;
    }
}
