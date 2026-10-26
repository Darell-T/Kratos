//! A fake `codex app-server`: JSON-RPC over stdio, one JSON object per line.
//!
//! It answers the protocol requests Kratos sends from a small in-memory model of Codex's threads,
//! and it holds turn requests until a test settles them, so a test decides when and how each turn
//! starts. Tests steer it with `test/*` requests. A reply to a request the fake sent the client is
//! reported back as `test/answered` and `test/answeredWithId` notifications.
//!
//! Flags: `--hang-initialize` never answers `initialize`; `--bad-initialize` answers it with `{}`;
//! `--sleep` makes the process sleep for an hour, for `test/spawnChild`; `--control-file <path>`
//! also takes `test/*` requests on a localhost TCP port, written to `<path>`, for a test whose
//! client under test owns stdio; `--state-file <path>` starts with the `threads` and `rateLimits`
//! of the JSON object in `<path>`, as a Codex restarted on an existing history does. The variables
//! `FAKE_CODEX_CONTROL_FILE` and `FAKE_CODEX_STATE_FILE` stand in for the two flags when the fake
//! is started by an engine, which passes its own variables on but not its own flags.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--sleep") {
        std::thread::sleep(Duration::from_secs(3600));
        return;
    }
    let fake = Arc::new(Mutex::new(Fake {
        hang_initialize: arguments
            .iter()
            .any(|argument| argument == "--hang-initialize"),
        bad_initialize: arguments
            .iter()
            .any(|argument| argument == "--bad-initialize"),
        ..Fake::default()
    }));
    if let Some(path) = flag_or_variable(&arguments, "--state-file", "FAKE_CODEX_STATE_FILE") {
        let text = std::fs::read_to_string(path).expect("the state file is readable");
        let state: Value = serde_json::from_str(&text).expect("the state file is JSON");
        let mut fake = lock(&fake);
        for thread in state["threads"].as_array().into_iter().flatten() {
            let thread_id = thread["id"].as_str().unwrap_or_default().to_owned();
            fake.threads.insert(thread_id, thread.clone());
        }
        if !state["rateLimits"].is_null() {
            fake.rate_limits = Some(state["rateLimits"].clone());
        }
        fake.sandbox_readiness = state["sandboxReadiness"].as_str().map(str::to_owned);
        fake.sandbox_setup = state["sandboxSetup"].clone();
    }
    if let Some(path) = flag_or_variable(&arguments, "--control-file", "FAKE_CODEX_CONTROL_FILE") {
        serve_control(fake.clone(), &path);
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        let message: Value = serde_json::from_str(&line).expect("the client sends JSON lines");
        let out = lock(&fake).receive(message);
        for out in out {
            match out {
                Out::Client(message) | Out::Caller(message) => send(&message),
            }
        }
    }
}

fn flag_or_variable(arguments: &[String], flag: &str, variable: &str) -> Option<String> {
    arguments
        .iter()
        .position(|argument| argument == flag)
        .and_then(|index| arguments.get(index + 1))
        .cloned()
        .or_else(|| {
            std::env::var(variable)
                .ok()
                .filter(|value| !value.is_empty())
        })
}

/// Takes `test/*` requests on a localhost port and answers each on its own connection. What they
/// make the fake send its client still goes to stdout.
fn serve_control(fake: Arc<Mutex<Fake>>, path: &str) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a localhost port is free");
    let port = listener.local_addr().expect("the port is known").port();
    // Written whole and renamed, so a test that sees the file reads the whole port.
    let partial = format!("{path}.partial");
    std::fs::write(&partial, port.to_string()).expect("the control file is writable");
    std::fs::rename(&partial, path).expect("the control file can be renamed");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let fake = fake.clone();
            std::thread::spawn(move || control_connection(&fake, stream));
        }
    });
}

fn control_connection(fake: &Mutex<Fake>, stream: TcpStream) {
    let mut writer = stream.try_clone().expect("the stream can be cloned");
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            return;
        };
        let out = lock(fake).receive(message);
        for out in out {
            match out {
                Out::Client(message) => send(&message),
                Out::Caller(message) => {
                    if writeln!(writer, "{message}").is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn lock(fake: &Mutex<Fake>) -> std::sync::MutexGuard<'_, Fake> {
    fake.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A message the fake sends: to its client on stdout, or as the answer to whoever asked.
enum Out {
    Client(Value),
    Caller(Value),
}

#[derive(Default)]
struct Fake {
    turn_observer: Option<TcpStream>,
    hang_initialize: bool,
    bad_initialize: bool,
    initialized: bool,
    /// Every held request, in arrival order, for `test/turnRequests`.
    held_log: Vec<Value>,
    /// Requests not yet settled, in arrival order.
    held: Vec<Held>,
    /// Every `thread/start`, `thread/resume`, and `thread/unsubscribe`, for `test/threadRequests`.
    thread_log: Vec<Value>,
    threads: BTreeMap<String, Value>,
    threads_started: u32,
    model_pages: Option<Vec<Value>>,
    rate_limits: Option<Value>,
    sandbox_readiness: Option<String>,
    sandbox_setup: Value,
    sandbox_log: Vec<Value>,
    pending_sandbox_mode: Option<Value>,
    /// Every answer the client gave to a request the fake sent, for `test/answers`.
    answers: Vec<Value>,
    /// What handling the current message sends, in order.
    outbox: Vec<Out>,
}

struct Held {
    id: Value,
    method: String,
    params: Value,
}

/// Requests the fake keeps until `test/settleTurn` answers them.
const HELD_METHODS: [&str; 4] = ["turn/start", "turn/steer", "turn/interrupt", "command/exec"];

impl Fake {
    fn receive(&mut self, message: Value) -> Vec<Out> {
        let method = message["method"].as_str().map(str::to_owned);
        match method.as_deref() {
            Some("initialized") => self.initialized = true,
            Some(method) => self.request(message["id"].clone(), method, &message["params"]),
            None => self.answered(&message),
        }
        std::mem::take(&mut self.outbox)
    }

    /// Answers the request being handled, to whoever sent it.
    fn reply(&mut self, id: &Value, result: Value) {
        self.outbox
            .push(Out::Caller(json!({ "id": id, "result": result })));
    }

    fn fail(&mut self, id: &Value, code: i64, message: &str) {
        self.outbox.push(Out::Caller(
            json!({ "id": id, "error": { "code": code, "message": message } }),
        ));
    }

    /// Answers a request the client sent earlier, such as a held `turn/start`.
    fn reply_to_client(&mut self, id: &Value, result: Value) {
        self.outbox
            .push(Out::Client(json!({ "id": id, "result": result })));
    }

    fn fail_to_client(&mut self, id: &Value, code: i64, message: &str) {
        self.outbox.push(Out::Client(
            json!({ "id": id, "error": { "code": code, "message": message } }),
        ));
    }

    fn send(&mut self, message: Value) {
        self.outbox.push(Out::Client(message));
    }

    /// The client answered a request the fake sent it.
    fn answered(&mut self, message: &Value) {
        let (result, error) = (&message["result"], &message["error"]);
        self.send(json!({ "method": "test/answered", "params": answer(result, error) }));
        let mut with_id = answer(result, error);
        with_id["id"] = message["id"].clone();
        self.answers.push(with_id.clone());
        self.send(json!({ "method": "test/answeredWithId", "params": with_id }));
    }

    // #lizard forgives: one arm for each protocol method the fake answers, each a line or two.
    fn request(&mut self, id: Value, method: &str, params: &Value) {
        if method == "initialize" {
            return self.initialize(id, params);
        }
        if !self.initialized {
            return self.fail(&id, -32002, "Not initialized");
        }
        if HELD_METHODS.contains(&method) {
            if let Some(observer) = &mut self.turn_observer {
                writeln!(
                    observer,
                    "{}",
                    json!({ "method": method, "params": params })
                )
                .expect("the benchmark observer stays connected");
            }
            self.held_log
                .push(json!({ "method": method, "params": params }));
            self.held.push(Held {
                id,
                method: method.to_owned(),
                params: params.clone(),
            });
            return;
        }
        match method {
            "thread/start" => self.start_thread(&id, params),
            "thread/resume" => self.resume_thread(&id, params),
            "thread/read" => {
                let thread = self.thread(params["threadId"].as_str().unwrap_or_default(), None);
                self.reply(&id, json!({ "thread": thread }));
            }
            "thread/unsubscribe" => {
                self.thread_log
                    .push(json!({ "method": "thread/unsubscribe", "params": params }));
                self.reply(&id, json!({ "status": "unsubscribed" }));
            }
            "model/list" => self.models(&id, params),
            "skills/list" => {
                let skills = skills(params);
                self.reply(&id, skills);
            }
            "plugin/installed" => {
                let plugins = plugins(params);
                self.reply(&id, plugins);
            }
            "account/read" => self.reply(
                &id,
                json!({ "account": { "type": "chatgpt", "email": "fake@example.invalid", "planType": "plus" }, "requiresOpenaiAuth": true }),
            ),
            "account/rateLimits/read" => match self.rate_limits.clone() {
                Some(read) if read.get("error").is_some() => self.fail(
                    &id,
                    read["code"].as_i64().unwrap_or(-32600),
                    read["error"].as_str().unwrap_or("error"),
                ),
                Some(read) => self.reply(&id, read),
                None => self.fail(
                    &id,
                    -32600,
                    "codex account authentication required to read rate limits",
                ),
            },
            "windowsSandbox/readiness" => {
                self.sandbox_log.push(json!({ "method": method, "params": params }));
                self.reply(&id, json!({ "status": self.sandbox_readiness.as_deref().unwrap_or("ready") }));
            }
            "windowsSandbox/setupStart" => {
                self.sandbox_log.push(json!({ "method": method, "params": params }));
                self.pending_sandbox_mode = Some(params["mode"].clone());
                self.reply(&id, json!({ "started": true }));
                if self.sandbox_setup["hold"] != true {
                    self.finish_sandbox_setup();
                }
            }
            method if method.starts_with("test/") => self.control(&id, method, params),
            method => self.fail(&id, -32601, &format!("Method not found: {method}")),
        }
    }

    fn initialize(&mut self, id: Value, params: &Value) {
        if self.hang_initialize {
            return;
        }
        if self.bad_initialize {
            return self.reply(&id, json!({}));
        }
        let client = &params["clientInfo"];
        self.reply(
            &id,
            json!({
                "userAgent": format!("{}/{} (fake codex)", client["name"].as_str().unwrap_or_default(), client["version"].as_str().unwrap_or_default()),
                "codexHome": "C:\\fake\\.codex",
                "platformFamily": "windows",
                "platformOs": "windows",
            }),
        );
    }

    fn start_thread(&mut self, id: &Value, params: &Value) {
        self.thread_log
            .push(json!({ "method": "thread/start", "params": params }));
        let thread_id = format!("thread-{}", char::from(b'A' + self.threads_started as u8));
        self.threads_started += 1;
        let mut thread = new_thread(&thread_id, params["cwd"].as_str());
        if let Some(model) = params["model"].as_str() {
            thread["model"] = json!(model);
        }
        self.threads.insert(thread_id, thread.clone());
        self.reply(id, thread_reply(&thread));
    }

    fn resume_thread(&mut self, id: &Value, params: &Value) {
        self.thread_log
            .push(json!({ "method": "thread/resume", "params": params }));
        let thread_id = params["threadId"].as_str().unwrap_or_default().to_owned();
        let mut thread = self.thread(&thread_id, params["cwd"].as_str());
        if let Some(model) = params["model"].as_str() {
            thread["model"] = json!(model);
        }
        self.threads.insert(thread_id, thread.clone());
        self.reply(id, thread_reply(&thread));
    }

    fn thread(&self, thread_id: &str, cwd: Option<&str>) -> Value {
        self.threads
            .get(thread_id)
            .cloned()
            .unwrap_or_else(|| new_thread(thread_id, cwd))
    }

    fn models(&mut self, id: &Value, params: &Value) {
        let page = params["cursor"]
            .as_str()
            .and_then(|cursor| cursor.parse::<usize>().ok())
            .unwrap_or(0);
        let pages = self
            .model_pages
            .clone()
            .unwrap_or_else(|| vec![json!({ "data": [], "nextCursor": null })]);
        self.reply(id, pages.get(page).cloned().unwrap_or(Value::Null));
    }

    fn control(&mut self, id: &Value, method: &str, params: &Value) {
        match method {
            "test/observeTurns" => {
                let port =
                    u16::try_from(params["port"].as_u64().expect("an observer port")).unwrap();
                let observer = TcpStream::connect(("127.0.0.1", port)).unwrap();
                observer.set_nodelay(true).unwrap();
                self.turn_observer = Some(observer);
                self.reply(id, json!({}));
            }
            "test/setThread" => {
                let thread = params["thread"].clone();
                let thread_id = thread["id"].as_str().unwrap_or_default().to_owned();
                self.threads.insert(thread_id, thread);
                self.reply(id, Value::Null);
            }
            "test/models" => {
                self.model_pages = params["pages"].as_array().cloned();
                self.reply(id, Value::Null);
            }
            "test/rateLimits" => {
                self.rate_limits = Some(params.clone());
                self.reply(id, Value::Null);
            }
            "test/sandboxReadiness" => {
                self.sandbox_readiness = params["status"].as_str().map(str::to_owned);
                self.reply(id, Value::Null);
            }
            "test/sandboxRequests" => self.reply(id, json!(self.sandbox_log)),
            "test/releaseSandboxSetup" => {
                self.finish_sandbox_setup();
                self.reply(id, Value::Null);
            }
            "test/turnRequests" => {
                let log = Value::Array(self.held_log.clone());
                self.reply(id, log);
            }
            "test/threadRequests" => {
                let log = Value::Array(self.thread_log.clone());
                self.reply(id, log);
            }
            "test/answers" => {
                let answers = Value::Array(self.answers.clone());
                self.reply(id, answers);
            }
            "test/settleTurn" => self.settle(id, params),
            "test/echo" => self.reply(id, params.clone()),
            "test/fail" => self.fail(id, -32000, "boom"),
            "test/environment" => self.reply(
                id,
                json!({
                    "codexHome": std::env::var("CODEX_HOME").ok(),
                    "cwd": std::env::current_dir().ok(),
                    "arguments": std::env::args().skip(1).collect::<Vec<_>>(),
                }),
            ),
            "test/notify" => {
                self.send(json!({ "method": "test/notified", "params": params }));
                self.reply(id, Value::Null);
            }
            "test/noise" => {
                self.send(json!("this is not json"));
                self.reply(id, json!("still alive"));
            }
            "test/pid" => self.reply(id, json!(std::process::id())),
            "test/ask" => {
                let request_id = match &params["requestId"] {
                    Value::Null => json!("ask-1"),
                    request_id => request_id.clone(),
                };
                send(
                    &json!({ "id": request_id, "method": params["method"], "params": params["params"] }),
                );
                self.reply(id, Value::Null);
            }
            "test/emit" => {
                self.emit(params);
                self.reply(id, Value::Null);
            }
            "test/spawnChild" => self.reply(id, json!(spawn_sleeper())),
            "test/exit" => std::process::exit(params["code"].as_i64().unwrap_or(0) as i32),
            // Never answers, so the client has a request in flight when the process goes away.
            "test/hang" => {}
            method => self.fail(id, -32601, &format!("Method not found: {method}")),
        }
    }

    fn finish_sandbox_setup(&mut self) {
        let Some(mode) = self.pending_sandbox_mode.take() else {
            return;
        };
        let success = self.sandbox_setup["success"].as_bool().unwrap_or(true);
        if success {
            self.sandbox_readiness = Some(
                self.sandbox_setup["readinessAfter"]
                    .as_str()
                    .unwrap_or("ready")
                    .to_owned(),
            );
        }
        self.send(json!({
            "method": "windowsSandbox/setupCompleted",
            "params": { "mode": mode, "success": success, "error": self.sandbox_setup["error"] },
        }));
    }

    /// Answers the oldest held request with this method and thread, with `result` or with an error.
    fn settle(&mut self, id: &Value, params: &Value) {
        let method = params["method"].as_str().unwrap_or_default();
        let thread_id = &params["threadId"];
        let position = self.held.iter().position(|held| {
            held.method == method && (thread_id.is_null() || held.params["threadId"] == *thread_id)
        });
        let Some(position) = position else {
            return self.fail(id, -32000, "No matching pending turn request");
        };
        let held = self.held.remove(position);
        match params["error"].as_str() {
            Some(error) => {
                self.fail_to_client(&held.id, params["code"].as_i64().unwrap_or(-32000), error)
            }
            None => {
                self.remember_input(&held, &params["result"]);
                self.reply_to_client(&held.id, params["result"].clone());
            }
        }
        self.reply(id, Value::Null);
    }

    /// Records a started or steered message in the thread's history, as Codex does, so a later
    /// `thread/read` finds it by its client id.
    fn remember_input(&mut self, held: &Held, result: &Value) {
        let thread_id = held.params["threadId"].as_str().unwrap_or_default();
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        let message = json!({
            "type": "userMessage",
            "id": held.params["clientUserMessageId"],
            "clientId": held.params["clientUserMessageId"],
            "content": held.params["input"],
        });
        match held.method.as_str() {
            "turn/start" if result["turn"].is_object() => {
                let mut turn = result["turn"].clone();
                let mut items = turn["items"].as_array().cloned().unwrap_or_default();
                items.push(message);
                turn["items"] = Value::Array(items);
                let active = turn["status"] == "inProgress";
                push(&mut thread["turns"], turn);
                thread["status"] = if active {
                    json!({ "type": "active", "activeFlags": [] })
                } else {
                    json!({ "type": "idle" })
                };
            }
            "turn/steer" => {
                let expected = &held.params["expectedTurnId"];
                if let Some(turn) = thread["turns"]
                    .as_array_mut()
                    .and_then(|turns| turns.iter_mut().find(|turn| turn["id"] == *expected))
                {
                    push(&mut turn["items"], message);
                }
            }
            _ => {}
        }
    }

    /// Sends a notification. A `turn/completed` also finishes that turn in the thread's history.
    fn emit(&mut self, params: &Value) {
        if params["method"] == "turn/completed" {
            let completed = &params["params"]["turn"];
            let thread_id = params["params"]["threadId"].as_str().unwrap_or_default();
            if let Some(thread) = self.threads.get_mut(thread_id) {
                if let Some(turn) = thread["turns"]
                    .as_array_mut()
                    .and_then(|turns| turns.iter_mut().find(|turn| turn["id"] == completed["id"]))
                {
                    let items = turn["items"].clone();
                    *turn = completed.clone();
                    if turn["items"].as_array().is_none_or(Vec::is_empty) {
                        turn["items"] = items;
                    }
                }
                thread["status"] = json!({ "type": "idle" });
            }
        }
        self.send(json!({ "method": params["method"], "params": params["params"] }));
    }
}

fn new_thread(thread_id: &str, cwd: Option<&str>) -> Value {
    let cwd = cwd.map(str::to_owned).unwrap_or_else(|| {
        std::env::current_dir()
            .map(|cwd| cwd.display().to_string())
            .unwrap_or_default()
    });
    json!({
        "id": thread_id,
        "cwd": cwd,
        "model": "fake-model",
        "status": { "type": "idle" },
        "turns": [],
        "ephemeral": false,
        "cliVersion": "0.0.0-fake",
        "createdAt": 0,
        "updatedAt": 0,
        "modelProvider": "fake",
        "preview": "",
        "projectId": null,
        "sessionId": thread_id,
        "source": "appServer",
    })
}

/// The answer to `thread/start` and `thread/resume`, with every field Codex's schema requires.
fn thread_reply(thread: &Value) -> Value {
    json!({
        "thread": thread,
        "model": thread["model"],
        "cwd": thread["cwd"],
        "modelProvider": "fake",
        "approvalPolicy": "untrusted",
        "approvalsReviewer": "user",
        "sandbox": { "type": "readOnly" },
    })
}

fn skills(params: &Value) -> Value {
    let cwd = params["cwds"][0].as_str().unwrap_or_default();
    json!({ "data": [{ "cwd": cwd, "errors": [], "skills": [{
        "name": "project", "description": "Project skill", "path": format!("{cwd}/SKILL.md"),
        "enabled": true, "scope": "repo",
    }] }] })
}

fn plugins(params: &Value) -> Value {
    let cwd = params["cwds"][0].as_str().unwrap_or_default();
    json!({ "marketplaces": [{ "name": cwd, "plugins": [{ "id": "project", "name": "Project plugin", "installed": true, "enabled": true }] }] })
}

/// Starts a copy of this fake that sleeps, outside any wait, and returns its process id. The client
/// must end it when it closes the fake, because the fake never does.
#[allow(
    clippy::zombie_processes,
    reason = "the sleeper must outlive this call; the client under test ends it"
)]
fn spawn_sleeper() -> u32 {
    let exe = std::env::current_exe().expect("the fake knows its own path");
    let child = std::process::Command::new(exe)
        .arg("--sleep")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the fake can start a copy of itself");
    child.id()
}

fn push(array: &mut Value, value: Value) {
    match array.as_array_mut() {
        Some(items) => items.push(value),
        None => *array = Value::Array(vec![value]),
    }
}

fn answer(result: &Value, error: &Value) -> Value {
    let mut answer = json!({});
    if !result.is_null() {
        answer["result"] = result.clone();
    }
    if !error.is_null() {
        answer["error"] = error.clone();
    }
    answer
}

fn send(message: &Value) {
    // `test/noise` sends a bare string, which goes out as its text: a line that is not JSON.
    match message.as_str() {
        Some(text) => write_line(text),
        None => write_line(&message.to_string()),
    }
}

fn write_line(line: &str) {
    let mut stdout = std::io::stdout().lock();
    // A closed stdout means the client is gone, and the fake exits when its stdin closes.
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}
