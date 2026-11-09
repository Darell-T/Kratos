//! One process and its ordered stdin, bounded stdout, reverse controls, and terminal state.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use kratos_process::OwnedChild;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::{ControlRequest, Initialize, Initialized, ParseError, SessionStart, StreamMessage};

/// Largest of 1,089 messages in 34 spike recordings was 50,630 UTF-8 bytes without newline,
/// s8-settings-isolation/b-user-project-local/wire.jsonl:6. 128 KiB gives 2.59 times headroom.
pub const MAX_LINE_BYTES: usize = 128 * 1024;
const QUEUE_CAPACITY: usize = 256;
const STDERR_BYTES_KEPT: usize = 16 * 1024;
const STDERR_LINES_KEPT: usize = 20;

/// The CLI's permission setting. Approval policy itself belongs to the engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionMode {
    #[default]
    Default,
    Auto,
    Plan,
    BypassPermissions,
}

impl PermissionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

/// Settings sources are explicit so project settings are never loaded accidentally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingSource {
    User,
    Project,
    Local,
}

impl SettingSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

/// The complete native CLI launch, with intentional overrides applied after inherited filtering.
#[derive(Clone, Debug)]
pub struct Launch {
    pub program: PathBuf,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub permission_mode: PermissionMode,
    pub setting_sources: Vec<SettingSource>,
    pub session: SessionStart,
    pub disallowed_tools: Vec<String>,
    pub env_set: Vec<(OsString, OsString)>,
    pub control_timeout: Duration,
    pub close_timeout: Duration,
}

impl Launch {
    /// Starts with no settings sources and the CLI's default model and permissions.
    #[must_use]
    pub fn new(program: PathBuf, cwd: PathBuf, session: SessionStart) -> Self {
        Self {
            program,
            cwd,
            session,
            model: None,
            permission_mode: PermissionMode::Default,
            setting_sources: Vec::new(),
            disallowed_tools: Vec::new(),
            env_set: Vec::new(),
            control_timeout: Duration::from_secs(30),
            close_timeout: Duration::from_secs(5),
        }
    }
}

/// Why Kratos ended a session that the CLI did not end itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExitCause {
    /// Unanswered reverse controls filled the outgoing queue, so no reply could be reserved.
    ReplyQueueFull,
}

impl std::fmt::Display for ExitCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReplyQueueFull => {
                formatter.write_str("too many reverse controls were waiting for an answer")
            }
        }
    }
}

/// The final child status and bounded stderr tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
    pub stderr: Vec<String>,
    /// Set when Kratos ended the session for a reason of its own, rather than the CLI exiting.
    pub cause: Option<ExitCause>,
}

impl std::fmt::Display for Exit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "Claude exited (code {code})")?,
            None => formatter.write_str("Claude exited (code unknown)")?,
        }
        if let Some(cause) = self.cause {
            write!(formatter, " after Kratos ended it, because {cause}")?;
        }
        if let Some(last) = self.stderr.last() {
            write!(formatter, ": {last}")?;
        }
        Ok(())
    }
}

/// Protocol events that are not a matching answer to a request sent by this client.
#[derive(Debug)]
pub enum Incoming {
    Message(StreamMessage),
    Control(IncomingControl),
    Oversized {
        bytes: usize,
    },
    /// Malformed known messages are visible without ending an otherwise usable stream.
    Unreadable(ParseError),
    /// No message follows the final exit.
    Exited(Exit),
}

/// A reverse control that consumes its single answer or answers an error when dropped.
#[derive(Debug)]
pub struct IncomingControl {
    pub id: String,
    pub request: ControlRequest,
    responder: Option<Responder>,
}

impl IncomingControl {
    /// Queues a successful response body unless the CLI withdrew the request or exited.
    #[must_use]
    pub fn answer(mut self, body: Value) -> bool {
        self.respond(Ok(body))
    }

    /// Queues a protocol error unless the CLI withdrew the request or exited.
    #[must_use]
    pub fn fail(mut self, error: &str) -> bool {
        self.respond(Err(error.to_owned()))
    }

    fn respond(&mut self, outcome: Result<Value, String>) -> bool {
        let responder = self
            .responder
            .take()
            .expect("each incoming control is consumed once");
        responder.send(&self.id, outcome)
    }
}

impl Drop for IncomingControl {
    fn drop(&mut self) {
        if let Some(responder) = self.responder.take() {
            responder.send(
                &self.id,
                Err("Kratos dropped the control unanswered".to_owned()),
            );
        }
    }
}

#[derive(Debug)]
struct Responder {
    shared: Arc<Shared>,
    permit: mpsc::OwnedPermit<String>,
}

impl Responder {
    fn send(self, id: &str, outcome: Result<Value, String>) -> bool {
        if !self.shared.state().awaiting_answer.remove(id) {
            return false;
        }
        let response = match outcome {
            Ok(body) => json!({"subtype":"success", "request_id":id, "response":body}),
            Err(error) => json!({"subtype":"error", "request_id":id, "error":error}),
        };
        self.permit
            .send(json!({"type":"control_response", "response":response}).to_string());
        true
    }
}

/// A cheap shared handle. Only public handles own last-handle process cleanup.
/// Drain incoming events concurrently with requests. A full event queue pauses stdout, including
/// control replies, while deadlines keep running. Close and exit settlement do not need a drain.
#[derive(Clone, Debug)]
pub struct Claude {
    shared: Arc<Shared>,
    _lease: Arc<Lease>,
}

#[derive(Debug)]
struct Lease {
    close: watch::Sender<Option<Duration>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.close.send_if_modified(|value| {
            if value.is_none() {
                *value = Some(Duration::ZERO);
                true
            } else {
                false
            }
        });
    }
}

#[derive(Debug)]
struct Shared {
    writer: mpsc::Sender<String>,
    next_id: AtomicU64,
    state: Mutex<State>,
    control_slots: Arc<Semaphore>,
    close: watch::Sender<Option<Duration>>,
    exited: watch::Receiver<Option<Exit>>,
    control_timeout: Duration,
    close_timeout: Duration,
}

#[derive(Debug, Default)]
enum Lifecycle {
    #[default]
    Running,
    Closing,
    Exited(Exit),
}

#[derive(Debug, Default)]
struct State {
    lifecycle: Lifecycle,
    pending: HashMap<String, oneshot::Sender<Result<Value, RequestError>>>,
    awaiting_answer: HashSet<String>,
    startup: Option<mpsc::UnboundedSender<IncomingControl>>,
    cause: Option<ExitCause>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("Claude connection state lock poisoned")
    }

    fn running(state: &State) -> Result<(), RequestError> {
        match &state.lifecycle {
            Lifecycle::Running => Ok(()),
            Lifecycle::Closing => Err(RequestError::Closing),
            Lifecycle::Exited(exit) => Err(RequestError::Exited(exit.clone())),
        }
    }

    fn shutdown(&self, timeout: Duration) {
        let mut state = self.state();
        if matches!(state.lifecycle, Lifecycle::Running) {
            state.lifecycle = Lifecycle::Closing;
        }
        self.close.send_if_modified(|value| {
            if value.is_none() {
                *value = Some(timeout);
                true
            } else {
                false
            }
        });
    }

    fn end_because(&self, cause: ExitCause) {
        self.state().cause.get_or_insert(cause);
        self.shutdown(Duration::ZERO);
    }

    fn finish(&self, exit: &Exit) {
        self.control_slots.close();
        let pending = {
            let mut state = self.state();
            state.lifecycle = Lifecycle::Exited(exit.clone());
            state.awaiting_answer.clear();
            state.startup.take();
            std::mem::take(&mut state.pending)
        };
        for sender in pending.into_values() {
            let _ = sender.send(Err(RequestError::Exited(exit.clone())));
        }
    }
}

struct PendingGuard {
    shared: Arc<Shared>,
    id: String,
    _slot: OwnedSemaphorePermit,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.shared.state().pending.remove(&self.id);
    }
}

/// A request's timeout means its remote outcome is unknown, rather than a safe retry.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error("Claude {subtype} timed out after {} ms; outcome is unknown", .after.as_millis())]
    Timeout { subtype: String, after: Duration },
    #[error("Claude rejected the control: {0}")]
    Rejected(String),
    #[error("Claude connection is closing")]
    Closing,
    #[error("{0}")]
    Exited(Exit),
    #[error("control request requires a string subtype")]
    MissingSubtype,
    #[error("invalid user message uuid: {0}")]
    InvalidUuid(#[source] uuid::Error),
    #[error("cannot read Claude response: {0}")]
    Unreadable(#[from] ParseError),
}

/// Launch and initialization failures retain the program or protocol cause.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("cannot start Claude executable {}: {source}", .program.display())]
    Spawn {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Claude did not complete initialization: {0}")]
    Initialize(#[source] RequestError),
}

/// Starts the CLI and sends initialize first. Startup controls use the supplied prompt callback.
/// The callback may hand the owned control to an application task for asynchronous work.
///
/// # Errors
/// A failed handshake closes the process tree before returning its error.
pub async fn connect(
    launch: Launch,
    initialize: Initialize,
    mut on_startup_control: impl FnMut(IncomingControl),
) -> Result<(Claude, mpsc::Receiver<Incoming>, Initialized), ConnectError> {
    let mut child = spawn(&launch)?;
    let stdin = child.take_stdin().expect("stdin was piped");
    let stdout = child.take_stdout().expect("stdout was piped");
    let stderr = child.take_stderr().expect("stderr was piped");
    let (writer, writes) = mpsc::channel(QUEUE_CAPACITY);
    let (incoming_sender, incoming) = mpsc::channel(QUEUE_CAPACITY);
    // Each startup control holds a reply permit, so the writer queue already bounds this channel.
    let (startup_sender, mut startup) = mpsc::unbounded_channel();
    let (close, close_requested) = watch::channel(None);
    let (exit_sender, exited) = watch::channel(None);
    let shared = Arc::new(Shared {
        writer,
        next_id: AtomicU64::new(1),
        state: Mutex::new(State {
            startup: Some(startup_sender),
            ..State::default()
        }),
        control_slots: Arc::new(Semaphore::new(QUEUE_CAPACITY)),
        close: close.clone(),
        exited,
        control_timeout: launch.control_timeout,
        close_timeout: launch.close_timeout,
    });
    let claude = Claude {
        shared: shared.clone(),
        _lease: Arc::new(Lease { close }),
    };
    let stderr = tokio::spawn(keep_stderr(stderr));
    tokio::spawn(write_lines(
        shared.clone(),
        stdin,
        writes,
        close_requested.clone(),
    ));
    tokio::spawn(keep(
        shared.clone(),
        child,
        stderr,
        close_requested,
        exit_sender,
    ));
    tokio::spawn(read(shared.clone(), stdout, incoming_sender));
    let mut request = serde_json::to_value(initialize).expect("initialize fields serialize");
    request["subtype"] = json!("initialize");
    let initialized = {
        let reply = claude.control(request);
        tokio::pin!(reply);
        loop {
            tokio::select! {
                reply = &mut reply => break reply.and_then(|raw| Initialized::parse(raw).map_err(RequestError::from)),
                Some(control) = startup.recv() => on_startup_control(control),
            }
        }
    };
    shared.state().startup.take();
    while let Ok(control) = startup.try_recv() {
        on_startup_control(control);
    }
    match initialized {
        Ok(initialized) => Ok((claude, incoming, initialized)),
        Err(error) => {
            claude.close().await;
            Err(ConnectError::Initialize(error))
        }
    }
}

fn spawn(launch: &Launch) -> Result<OwnedChild, ConnectError> {
    let mut command = Command::new(&launch.program);
    command
        .args([
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--permission-prompt-tool=stdio",
            "--include-partial-messages",
            "--replay-user-messages",
        ])
        .arg(launch.session.flag())
        .arg(format!(
            "--permission-mode={}",
            launch.permission_mode.as_str()
        ))
        .arg(format!(
            "--setting-sources={}",
            launch
                .setting_sources
                .iter()
                .map(|source| source.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ))
        .current_dir(&launch.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(model) = &launch.model {
        command.arg(format!("--model={model}"));
    }
    if launch.permission_mode == PermissionMode::BypassPermissions {
        command.arg("--allow-dangerously-skip-permissions");
    }
    if !launch.disallowed_tools.is_empty() {
        command
            .arg("--disallowedTools")
            .arg(launch.disallowed_tools.join(","));
    }
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("CLAUDE")
            || upper.starts_with("ANTHROPIC")
            || upper.starts_with("KRATOS")
        {
            command.env_remove(name);
        }
    }
    for (name, value) in &launch.env_set {
        command.env(name, value);
    }
    OwnedChild::spawn(command).map_err(|source| ConnectError::Spawn {
        program: launch.program.clone(),
        source,
    })
}

impl Claude {
    /// Queues a user message. Only a later Replay frame proves native acceptance.
    ///
    /// # Errors
    /// Rejects an invalid UUID or a closing or exited connection.
    pub async fn send(&self, uuid: &str, content: Value) -> Result<(), RequestError> {
        uuid::Uuid::parse_str(uuid).map_err(RequestError::InvalidUuid)?;
        let permit = self
            .shared
            .writer
            .reserve()
            .await
            .map_err(|_| self.ended())?;
        Shared::running(&self.shared.state())?;
        permit.send(json!({"type":"user", "uuid":uuid, "message":{"role":"user", "content":content}, "parent_tool_use_id":null}).to_string());
        Ok(())
    }

    /// Sends a control using the launch deadline, including time spent waiting for queue capacity.
    ///
    /// # Errors
    /// Returns a CLI error, timeout with unknown outcome, or connection exit.
    pub async fn control(&self, request: Value) -> Result<Value, RequestError> {
        self.control_within(request, self.shared.control_timeout)
            .await
    }

    /// Sends one control with an explicit deadline. Cancelling the future removes its pending slot.
    ///
    /// # Errors
    /// Returns a CLI error, timeout with unknown outcome, or connection exit.
    pub async fn control_within(
        &self,
        request: Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        let subtype = request
            .get("subtype")
            .and_then(Value::as_str)
            .ok_or(RequestError::MissingSubtype)?
            .to_owned();
        match tokio::time::timeout_at(Instant::now() + timeout, self.request(request)).await {
            Ok(reply) => reply,
            Err(_) => Err(RequestError::Timeout {
                subtype,
                after: timeout,
            }),
        }
    }

    async fn request(&self, request: Value) -> Result<Value, RequestError> {
        let slot = self
            .shared
            .control_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| self.ended())?;
        let permit = self
            .shared
            .writer
            .reserve()
            .await
            .map_err(|_| self.ended())?;
        let id = format!(
            "kratos-{}",
            self.shared.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, reply) = oneshot::channel();
        {
            let mut state = self.shared.state();
            Shared::running(&state)?;
            state.pending.insert(id.clone(), sender);
        }
        let _guard = PendingGuard {
            shared: self.shared.clone(),
            id: id.clone(),
            _slot: slot,
        };
        permit.send(
            json!({"type":"control_request", "request_id":id, "request":request}).to_string(),
        );
        reply.await.unwrap_or_else(|_| Err(self.ended()))
    }

    /// Acknowledges the interrupt request. The terminal turn result still arrives on the stream.
    ///
    /// # Errors
    /// Returns a CLI error, unknown-outcome timeout, or connection exit.
    pub async fn interrupt(&self) -> Result<Value, RequestError> {
        self.control(json!({"subtype":"interrupt"})).await
    }

    fn ended(&self) -> RequestError {
        match &self.shared.state().lifecycle {
            Lifecycle::Exited(exit) => RequestError::Exited(exit.clone()),
            _ => RequestError::Closing,
        }
    }

    /// Closes stdin and waits for process-tree cleanup, regardless of incoming queue consumption.
    pub async fn close(&self) {
        self.shared.shutdown(self.shared.close_timeout);
        let mut exited = self.shared.exited.clone();
        let _ = exited.wait_for(Option::is_some).await;
    }
}

async fn write_lines(
    shared: Arc<Shared>,
    mut stdin: ChildStdin,
    mut writes: mpsc::Receiver<String>,
    mut close: watch::Receiver<Option<Duration>>,
) {
    loop {
        let line = tokio::select! {
            biased;
            _ = close.wait_for(Option::is_some) => return,
            line = writes.recv() => match line { Some(line) => line, None => return },
        };
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        tokio::select! {
            biased;
            _ = close.wait_for(Option::is_some) => return,
            result = write => if result.is_err() { shared.shutdown(Duration::ZERO); return; },
        }
    }
}

async fn keep_stderr(mut stderr: ChildStderr) -> Vec<String> {
    let mut kept = VecDeque::with_capacity(STDERR_BYTES_KEPT);
    let mut buffer = [0; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => {
                kept.extend(&buffer[..count]);
                let excess = kept.len().saturating_sub(STDERR_BYTES_KEPT);
                kept.drain(..excess);
            }
            Err(error) => {
                let message = format!("cannot read Claude stderr: {error}");
                kept.extend(message.bytes());
                break;
            }
        }
    }
    let bytes: Vec<u8> = kept.into_iter().collect();
    let mut lines: Vec<String> = String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .take(STDERR_LINES_KEPT)
        .map(str::to_owned)
        .collect();
    lines.reverse();
    lines
}

async fn keep(
    shared: Arc<Shared>,
    mut child: OwnedChild,
    stderr: tokio::task::JoinHandle<Vec<String>>,
    mut close: watch::Receiver<Option<Duration>>,
    exited: watch::Sender<Option<Exit>>,
) {
    let status = tokio::select! {
        status = child.wait() => status,
        _ = async { let _ = close.wait_for(Option::is_some).await; } => {
            let timeout = (*close.borrow()).unwrap_or(Duration::ZERO);
            match tokio::time::timeout(timeout, child.wait()).await {
                Ok(status) => status,
                Err(_) => { let _ = child.end_tree(); child.wait().await }
            }
        }
    };
    let _ = child.end_tree();
    let mut stderr = stderr
        .await
        .unwrap_or_else(|error| vec![format!("Claude stderr task failed: {error}")]);
    let code = match status {
        Ok(status) => status.code(),
        Err(error) => {
            stderr.push(format!("cannot wait for Claude: {error}"));
            None
        }
    };
    let exit = Exit {
        code,
        stderr,
        cause: shared.state().cause,
    };
    shared.finish(&exit);
    shared.close.send_replace(Some(Duration::ZERO));
    exited.send_replace(Some(exit));
}

enum Line {
    Data(Vec<u8>),
    Oversized(usize),
    Eof,
}

async fn bounded_line(reader: &mut (impl AsyncBufRead + Unpin)) -> io::Result<Line> {
    let mut kept = Vec::new();
    let mut bytes = 0usize;
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            break;
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(buffer.len());
        bytes = bytes.saturating_add(count);
        if bytes <= MAX_LINE_BYTES {
            kept.extend_from_slice(&buffer[..count]);
        }
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            return if bytes > MAX_LINE_BYTES {
                Ok(Line::Oversized(bytes))
            } else {
                Ok(Line::Data(kept))
            };
        }
    }
    if bytes > MAX_LINE_BYTES {
        Ok(Line::Oversized(bytes))
    } else if bytes == 0 {
        Ok(Line::Eof)
    } else {
        Ok(Line::Data(kept))
    }
}

async fn read(shared: Arc<Shared>, stdout: ChildStdout, incoming: mpsc::Sender<Incoming>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let event = match bounded_line(&mut reader).await {
            Ok(Line::Eof) => break,
            Ok(Line::Oversized(bytes)) => Some(Incoming::Oversized { bytes }),
            Ok(Line::Data(line)) => match serde_json::from_slice::<Value>(&line) {
                Ok(message) => dispatch(&shared, message).await,
                Err(error) => Some(Incoming::Unreadable(ParseError::Json(error))),
            },
            Err(error) => {
                shared.shutdown(Duration::ZERO);
                eprintln!("Claude stdout failed: {error}");
                break;
            }
        };
        if let Some(event) = event
            && incoming.send(event).await.is_err()
        {
            shared.shutdown(Duration::ZERO);
            return;
        }
    }
    // Stdout closing usually means the CLI is exiting, and it may still flush its transcript.
    shared.shutdown(shared.close_timeout);
    let mut exited = shared.exited.clone();
    let exit = exited
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|exit| exit.clone());
    if let Some(exit) = exit {
        let _ = incoming.send(Incoming::Exited(exit)).await;
    }
}

async fn dispatch(shared: &Arc<Shared>, message: Value) -> Option<Incoming> {
    match message["type"].as_str() {
        Some("control_response") => {
            settle(shared, &message["response"]);
            None
        }
        Some("control_cancel_request") => {
            if let Some(id) = message["request_id"].as_str() {
                shared.state().awaiting_answer.remove(id);
            }
            None
        }
        Some("control_request") => reverse_control(shared, message),
        _ => Some(match StreamMessage::parse(message) {
            Ok(message) => Incoming::Message(message),
            Err(error) => Incoming::Unreadable(error),
        }),
    }
}

fn settle(shared: &Shared, response: &Value) {
    let Some(id) = response["request_id"].as_str() else {
        return;
    };
    let Some(sender) = shared.state().pending.remove(id) else {
        return;
    };
    let result = match response["subtype"].as_str() {
        Some("success") => Ok(response.get("response").cloned().unwrap_or(Value::Null)),
        Some("error") => Err(RequestError::Rejected(
            response["error"]
                .as_str()
                .unwrap_or("unreadable Claude control error")
                .to_owned(),
        )),
        _ => Err(RequestError::Rejected(
            "unreadable Claude control response subtype".to_owned(),
        )),
    };
    let _ = sender.send(result);
}

fn reverse_control(shared: &Arc<Shared>, message: Value) -> Option<Incoming> {
    let Some(id) = message["request_id"].as_str().map(str::to_owned) else {
        return Some(Incoming::Unreadable(ParseError::MissingRequestId));
    };
    let permit = match shared.writer.clone().try_reserve_owned() {
        Ok(permit) => permit,
        Err(_) => {
            shared.end_because(ExitCause::ReplyQueueFull);
            return None;
        }
    };
    let request = ControlRequest::parse(message["request"].clone());
    let mut state = shared.state();
    if Shared::running(&state).is_err() || !state.awaiting_answer.insert(id.clone()) {
        return None;
    }
    let request = match request {
        Ok(request) => request,
        Err(error) => {
            drop(state);
            Responder {
                shared: shared.clone(),
                permit,
            }
            .send(&id, Err(error.to_string()));
            return Some(Incoming::Unreadable(error));
        }
    };
    let control = IncomingControl {
        id,
        request,
        responder: Some(Responder {
            shared: shared.clone(),
            permit,
        }),
    };
    if let ControlRequest::Unsupported { subtype } = &control.request {
        let error = format!("unsupported Claude control subtype: {subtype}");
        drop(state);
        let _ = control.fail(&error);
        return None;
    }
    if let Some(startup) = &state.startup {
        let _ = startup.send(control);
        return None;
    }
    drop(state);
    Some(Incoming::Control(control))
}
