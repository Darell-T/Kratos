//! A fake `codex app-server`: JSON-RPC over stdio, one JSON object per line.
//!
//! It answers the protocol requests Kratos sends from a small in-memory model of Codex's threads,
//! and it holds turn requests until a test settles them, so a test decides when and how each turn
//! starts. Tests steer it with `test/*` requests. A reply to a request the fake sent the client is
//! reported back as `test/answered` and `test/answeredWithId` notifications.
//!
//! Flags: `--hang-initialize` never answers `initialize`; `--bad-initialize` answers it with `{}`;
//! `--sleep` makes the process sleep for an hour, for `test/spawnChild`.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--sleep") {
        std::thread::sleep(Duration::from_secs(3600));
        return;
    }
    let mut fake = Fake {
        hang_initialize: arguments
            .iter()
            .any(|argument| argument == "--hang-initialize"),
        bad_initialize: arguments
            .iter()
            .any(|argument| argument == "--bad-initialize"),
        ..Fake::default()
    };
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        let message: Value = serde_json::from_str(&line).expect("the client sends JSON lines");
        fake.receive(message);
    }
}

#[derive(Default)]
struct Fake {
    hang_initialize: bool,
    bad_initialize: bool,
    initialized: bool,
    /// Every held request, in arrival order, for `test/turnRequests`.
    held_log: Vec<Value>,
    /// Requests not yet settled, in arrival order.
    held: Vec<Held>,
    /// Every `thread/start` and `thread/resume`, for `test/threadRequests`.
    thread_log: Vec<Value>,
    threads: BTreeMap<String, Value>,
    threads_started: u32,
    model_pages: Option<Vec<Value>>,
    rate_limits: Option<Value>,
    sandbox_readiness: Option<String>,
}

struct Held {
    id: Value,
    method: String,
    params: Value,
}

/// Requests the fake keeps until `test/settleTurn` answers them.
const HELD_METHODS: [&str; 4] = ["turn/start", "turn/steer", "turn/interrupt", "command/exec"];

impl Fake {
    fn receive(&mut self, message: Value) {
        let method = message["method"].as_str().map(str::to_owned);
        match method.as_deref() {
            Some("initialized") => self.initialized = true,
            Some(method) => self.request(message["id"].clone(), method, &message["params"]),
            None => answered(&message),
        }
    }

    // #lizard forgives: one arm for each protocol method the fake answers, each a line or two.
    fn request(&mut self, id: Value, method: &str, params: &Value) {
        if method == "initialize" {
            return self.initialize(id, params);
        }
        if !self.initialized {
            return fail(&id, -32002, "Not initialized");
        }
        if HELD_METHODS.contains(&method) {
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
                reply(&id, json!({ "thread": thread }));
            }
            "thread/unsubscribe" => reply(&id, json!({})),
            "model/list" => self.models(&id, params),
            "skills/list" => skills(&id, params),
            "plugin/installed" => plugins(&id, params),
            "account/read" => reply(
                &id,
                json!({ "account": { "type": "chatgpt", "email": "fake@example.invalid", "planType": "plus" }, "requiresOpenaiAuth": true }),
            ),
            "account/rateLimits/read" => match &self.rate_limits {
                Some(read) if read.get("error").is_some() => fail(
                    &id,
                    read["code"].as_i64().unwrap_or(-32600),
                    read["error"].as_str().unwrap_or("error"),
                ),
                Some(read) => reply(&id, read.clone()),
                None => fail(
                    &id,
                    -32600,
                    "codex account authentication required to read rate limits",
                ),
            },
            "windowsSandbox/readiness" => reply(
                &id,
                json!({ "status": self.sandbox_readiness.as_deref().unwrap_or("ready") }),
            ),
            "windowsSandbox/setupStart" => {
                self.sandbox_readiness = Some("ready".to_owned());
                reply(&id, json!({}));
                send(&json!({
                    "method": "windowsSandbox/setupCompleted",
                    "params": { "mode": params["mode"], "success": true, "error": null },
                }));
            }
            method if method.starts_with("test/") => self.control(&id, method, params),
            method => fail(&id, -32601, &format!("Method not found: {method}")),
        }
    }

    fn initialize(&mut self, id: Value, params: &Value) {
        if self.hang_initialize {
            return;
        }
        if self.bad_initialize {
            return reply(&id, json!({}));
        }
        let client = &params["clientInfo"];
        reply(
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
        reply(id, thread_reply(&thread));
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
        reply(id, thread_reply(&thread));
    }

    fn thread(&self, thread_id: &str, cwd: Option<&str>) -> Value {
        self.threads
            .get(thread_id)
            .cloned()
            .unwrap_or_else(|| new_thread(thread_id, cwd))
    }

    fn models(&self, id: &Value, params: &Value) {
        let page = params["cursor"]
            .as_str()
            .and_then(|cursor| cursor.parse::<usize>().ok())
            .unwrap_or(0);
        let pages = self
            .model_pages
            .clone()
            .unwrap_or_else(|| vec![json!({ "data": [], "nextCursor": null })]);
        reply(id, pages.get(page).cloned().unwrap_or(Value::Null));
    }

    fn control(&mut self, id: &Value, method: &str, params: &Value) {
        match method {
            "test/setThread" => {
                let thread = params["thread"].clone();
                let thread_id = thread["id"].as_str().unwrap_or_default().to_owned();
                self.threads.insert(thread_id, thread);
                reply(id, Value::Null);
            }
            "test/models" => {
                self.model_pages = params["pages"].as_array().cloned();
                reply(id, Value::Null);
            }
            "test/rateLimits" => {
                self.rate_limits = Some(params.clone());
                reply(id, Value::Null);
            }
            "test/sandboxReadiness" => {
                self.sandbox_readiness = params["status"].as_str().map(str::to_owned);
                reply(id, Value::Null);
            }
            "test/turnRequests" => reply(id, Value::Array(self.held_log.clone())),
            "test/threadRequests" => reply(id, Value::Array(self.thread_log.clone())),
            "test/settleTurn" => self.settle(id, params),
            "test/echo" => reply(id, params.clone()),
            "test/fail" => fail(id, -32000, "boom"),
            "test/environment" => reply(
                id,
                json!({
                    "codexHome": std::env::var("CODEX_HOME").ok(),
                    "cwd": std::env::current_dir().ok(),
                }),
            ),
            "test/notify" => {
                send(&json!({ "method": "test/notified", "params": params }));
                reply(id, Value::Null);
            }
            "test/noise" => {
                write_line("this is not json");
                reply(id, json!("still alive"));
            }
            "test/pid" => reply(id, json!(std::process::id())),
            "test/ask" => {
                let request_id = match &params["requestId"] {
                    Value::Null => json!("ask-1"),
                    request_id => request_id.clone(),
                };
                send(
                    &json!({ "id": request_id, "method": params["method"], "params": params["params"] }),
                );
                reply(id, Value::Null);
            }
            "test/emit" => {
                self.emit(params);
                reply(id, Value::Null);
            }
            "test/spawnChild" => reply(id, json!(spawn_sleeper())),
            "test/exit" => std::process::exit(params["code"].as_i64().unwrap_or(0) as i32),
            // Never answers, so the client has a request in flight when the process goes away.
            "test/hang" => {}
            method => fail(id, -32601, &format!("Method not found: {method}")),
        }
    }

    /// Answers the oldest held request with this method and thread, with `result` or with an error.
    fn settle(&mut self, id: &Value, params: &Value) {
        let method = params["method"].as_str().unwrap_or_default();
        let thread_id = &params["threadId"];
        let position = self.held.iter().position(|held| {
            held.method == method && (thread_id.is_null() || held.params["threadId"] == *thread_id)
        });
        let Some(position) = position else {
            return fail(id, -32000, "No matching pending turn request");
        };
        let held = self.held.remove(position);
        match params["error"].as_str() {
            Some(error) => fail(&held.id, params["code"].as_i64().unwrap_or(-32000), error),
            None => {
                self.remember_input(&held, &params["result"]);
                reply(&held.id, params["result"].clone());
            }
        }
        reply(id, Value::Null);
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
        send(&json!({ "method": params["method"], "params": params["params"] }));
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
    })
}

fn thread_reply(thread: &Value) -> Value {
    json!({ "thread": thread, "model": thread["model"], "cwd": thread["cwd"] })
}

fn skills(id: &Value, params: &Value) {
    let cwd = params["cwds"][0].as_str().unwrap_or_default();
    reply(
        id,
        json!({ "data": [{ "cwd": cwd, "skills": [{ "name": "project", "description": "Project skill", "path": format!("{cwd}/SKILL.md") }] }] }),
    );
}

fn plugins(id: &Value, params: &Value) {
    let cwd = params["cwds"][0].as_str().unwrap_or_default();
    reply(
        id,
        json!({ "marketplaces": [{ "name": cwd, "plugins": [{ "id": "project", "name": "Project plugin", "installed": true, "enabled": true }] }] }),
    );
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

/// The client answered a request the fake sent it.
fn answered(message: &Value) {
    let (result, error) = (&message["result"], &message["error"]);
    send(&json!({ "method": "test/answered", "params": answer(result, error) }));
    let mut with_id = answer(result, error);
    with_id["id"] = message["id"].clone();
    send(&json!({ "method": "test/answeredWithId", "params": with_id }));
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

fn reply(id: &Value, result: Value) {
    send(&json!({ "id": id, "result": result }));
}

fn fail(id: &Value, code: i64, message: &str) {
    send(&json!({ "id": id, "error": { "code": code, "message": message } }));
}

fn send(message: &Value) {
    write_line(&message.to_string());
}

fn write_line(line: &str) {
    let mut stdout = std::io::stdout().lock();
    // A closed stdout means the client is gone, and the fake exits when its stdin closes.
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}
