//! The tools, as a table: adding a tool is adding a row. A tool answers with JSON text, or with a
//! refusal that Codex shows the agent as a failed tool call. Every request names the caller's
//! thread in the `Kratos-Thread` header, and the engine refuses what that thread may not do.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use kratos_client::{ApiReply, ApiRequest, ClientError};
use kratos_protocol::{
    ApprovalDecision, Permission, RequiredText, Seq, ThreadDetails, ThreadEvent, ThreadEventBody,
    ThreadId, ThreadSettings,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{Call, MAX_WAIT_SECONDS, transcript};
use crate::server::ACTING_THREAD_HEADER;

type Answer = Result<Value, String>;

struct Tool {
    name: &'static str,
    description: &'static str,
    input: fn() -> Value,
    run: fn(&Call, Value) -> Answer,
}

const DEFAULT_WAIT_SECONDS: u64 = 300;
/// How often a wait checks whether Codex cancelled the call.
const CANCEL_CHECK: Duration = Duration::from_secs(1);

#[rustfmt::skip]
const TOOLS: [Tool; 11] = [
    Tool { name: "whoami", input: no_input, run: whoami,
        description: "Your own thread: its id, title, workspace, and settings." },
    Tool { name: "list_workspaces", input: no_input, run: list_workspaces,
        description: "The workspaces Kratos knows, each a folder threads work in." },
    Tool { name: "list_threads", input: list_threads_input, run: list_threads,
        description: "The threads in a workspace, yours when workspaceId is omitted." },
    Tool { name: "get_thread", input: thread_input, run: get_thread,
        description: "A thread's settings, running turn, queued messages, and the approvals and questions it waits on." },
    Tool { name: "read_thread", input: read_input, run: read_thread,
        description: "A thread's transcript as short lines, newest last. Pass the returned lastSeq as after to read only what is new." },
    Tool { name: "create_thread", input: create_input, run: create_thread,
        description: "Start a new Codex thread, in your workspace unless you name one. It gets your permission unless you ask for less." },
    Tool { name: "send_message", input: send_input, run: send_message,
        description: "Send a message to another thread. It starts a turn when the thread is idle and queues otherwise. The message is labelled with your thread's id, and once its agent has it, that thread runs with no more than your permission until the user raises it." },
    Tool { name: "wait_for_turn", input: wait_input, run: wait_for_turn,
        description: "Wait until another thread's turn ends, it needs an approval or an answer, or it pauses." },
    Tool { name: "stop_thread", input: thread_input, run: stop_thread,
        description: "Stop another thread's running turn." },
    Tool { name: "answer_question", input: answer_input, run: answer_question,
        description: "Answer a question another thread's agent is waiting on (see get_thread)." },
    Tool { name: "decide_approval", input: decide_input, run: decide_approval,
        description: "Decide an approval another thread waits on. Only a Full access thread may." },
];

pub fn list() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|tool| {
            json!({ "name": tool.name, "description": tool.description,
                    "inputSchema": (tool.input)() })
        })
        .collect()
}

pub fn call(call: &Call, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(arguments) => arguments.clone(),
    };
    let answer = match TOOLS.iter().find(|tool| tool.name == name) {
        Some(tool) => (tool.run)(call, arguments),
        None => Err(format!("Kratos has no tool named {name}")),
    };
    let (text, refused) = match answer {
        Ok(value) => (value.to_string(), false),
        Err(refusal) => (refusal, true),
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": refused })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OnThread {
    thread_id: ThreadId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InWorkspace {
    workspace_id: Option<RequiredText>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadArguments {
    thread_id: ThreadId,
    after: Option<Seq>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateArguments {
    title: RequiredText,
    workspace_id: Option<RequiredText>,
    permission: Option<Permission>,
    plan_mode: Option<bool>,
    model: Option<RequiredText>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SendArguments {
    thread_id: ThreadId,
    text: RequiredText,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WaitArguments {
    thread_id: ThreadId,
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AnswerArguments {
    question_id: RequiredText,
    answers: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DecideArguments {
    approval_id: RequiredText,
    decision: ApprovalDecision,
}

fn parse_arguments<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|error| format!("Invalid arguments: {error}"))
}

fn whoami(call: &Call, arguments: Value) -> Answer {
    let NoArguments {} = parse_arguments(arguments)?;
    call.get(&thread_path(&call.caller.thread_id))
}

fn list_workspaces(call: &Call, arguments: Value) -> Answer {
    let NoArguments {} = parse_arguments(arguments)?;
    call.get("/api/workspaces")
}

fn list_threads(call: &Call, arguments: Value) -> Answer {
    let InWorkspace { workspace_id } = parse_arguments(arguments)?;
    let workspace = match workspace_id {
        Some(id) => id.into_string(),
        None => me(call)?.thread.workspace_id.as_str().to_owned(),
    };
    call.get(&format!(
        "/api/threads?workspaceId={}",
        percent_encode(&workspace)
    ))
}

fn get_thread(call: &Call, arguments: Value) -> Answer {
    let OnThread { thread_id } = parse_arguments(arguments)?;
    call.get(&thread_path(&thread_id))
}

fn read_thread(call: &Call, arguments: Value) -> Answer {
    let ReadArguments { thread_id, after } = parse_arguments(arguments)?;
    let (events, last) = events_after(call, &thread_id, after.unwrap_or(Seq::BEFORE_FIRST))?;
    let lines: Vec<String> = events
        .iter()
        .filter_map(|event| transcript::line(&event.event))
        .collect();
    Ok(json!({ "lastSeq": last, "transcript": transcript::newest(&lines) }))
}

fn create_thread(call: &Call, arguments: Value) -> Answer {
    let request: CreateArguments = parse_arguments(arguments)?;
    let me = me(call)?;
    let settings = ThreadSettings {
        permission: request.permission.unwrap_or(me.settings.permission),
        plan_mode: request.plan_mode.unwrap_or(me.settings.plan_mode),
        model: request.model.map(RequiredText::into_string),
        ..ThreadSettings::default()
    };
    let workspace = match request.workspace_id {
        Some(id) => id.into_string(),
        None => me.thread.workspace_id.as_str().to_owned(),
    };
    call.post(
        "/api/threads",
        &json!({ "workspaceId": workspace, "title": request.title, "provider": "codex",
                 "settings": settings }),
    )
}

fn send_message(call: &Call, arguments: Value) -> Answer {
    let SendArguments { thread_id, text } = parse_arguments(arguments)?;
    call.post(
        &format!("{}/messages", thread_path(&thread_id)),
        &json!({ "text": text }),
    )
}

fn wait_for_turn(call: &Call, arguments: Value) -> Answer {
    let WaitArguments {
        thread_id,
        timeout_seconds,
    } = parse_arguments(arguments)?;
    if thread_id == call.caller.thread_id {
        return Err("Your own turn cannot end while you wait for it.".to_owned());
    }
    let seconds = timeout_seconds.unwrap_or(DEFAULT_WAIT_SECONDS);
    if !(1..=MAX_WAIT_SECONDS).contains(&seconds) {
        return Err(format!(
            "Invalid arguments: timeoutSeconds must be 1 to {MAX_WAIT_SECONDS}"
        ));
    }
    let details: ThreadDetails = parse(call.get(&thread_path(&thread_id))?)?;
    if let Some(state) = settled(&details) {
        return Ok(state);
    }
    let deadline = Instant::now() + Duration::from_secs(seconds);
    follow(call, thread_id, details.last_seq, deadline)
}

/// Follows the thread's events after `after` until one ends the wait, Codex cancels the call, or
/// `deadline` passes.
fn follow(call: &Call, thread_id: ThreadId, after: Seq, deadline: Instant) -> Answer {
    let mut follower = call.caller.client.follow(thread_id, after);
    let mut last = after;
    let mut reply = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if call.cancelled.load(Ordering::Relaxed) {
            return Err("Cancelled".to_owned());
        }
        let event = match follower.next_event(left.min(CANCEL_CHECK)) {
            Ok(event) => event,
            Err(ClientError::TimedOut(_)) => continue,
            Err(error) => return Err(format!("Kratos is unreachable: {error}")),
        };
        last = event.seq;
        if let Some(text) = transcript::reply(&event.event) {
            reply = Some(text.to_owned());
        }
        if let Some(state) = ended(&event.event, reply.as_deref()) {
            return Ok(state);
        }
    }
    Ok(json!({ "state": "timedOut", "lastSeq": last }))
}

fn settled(details: &ThreadDetails) -> Option<Value> {
    if !details.approvals.is_empty() || !details.questions.is_empty() {
        return Some(
            json!({ "state": "waitingForInput", "approvals": details.approvals,
                            "questions": details.questions }),
        );
    }
    if let Some(reason) = &details.paused {
        return Some(json!({ "state": "paused", "reason": reason }));
    }
    (details.running_turn.is_none() && details.queue.is_empty()).then(|| json!({ "state": "idle" }))
}

fn ended(event: &ThreadEventBody, reply: Option<&str>) -> Option<Value> {
    match event {
        ThreadEventBody::TurnCompleted { outcome, .. } => {
            Some(json!({ "state": "turnEnded", "outcome": outcome, "reply": reply }))
        }
        ThreadEventBody::ApprovalRequested { approval } => {
            Some(json!({ "state": "waitingForInput", "approvals": [approval] }))
        }
        ThreadEventBody::QuestionAsked { question } => {
            Some(json!({ "state": "waitingForInput", "questions": [question] }))
        }
        ThreadEventBody::ThreadPaused { reason } => {
            Some(json!({ "state": "paused", "reason": reason }))
        }
        _ => None,
    }
}

fn stop_thread(call: &Call, arguments: Value) -> Answer {
    let OnThread { thread_id } = parse_arguments(arguments)?;
    call.post(&format!("{}/stop", thread_path(&thread_id)), &json!({}))
}

fn answer_question(call: &Call, arguments: Value) -> Answer {
    let AnswerArguments {
        question_id,
        answers,
    } = parse_arguments(arguments)?;
    call.post(
        &format!("/api/questions/{}", percent_encode(question_id.as_str())),
        &json!({ "answers": answers }),
    )
}

fn decide_approval(call: &Call, arguments: Value) -> Answer {
    let DecideArguments {
        approval_id,
        decision,
    } = parse_arguments(arguments)?;
    call.post(
        &format!("/api/approvals/{}", percent_encode(approval_id.as_str())),
        &json!({ "decision": decision }),
    )
}

fn me(call: &Call) -> Result<ThreadDetails, String> {
    parse(call.get(&thread_path(&call.caller.thread_id))?)
}

/// Every event after `after`, page by page, and the seq of the newest.
fn events_after(
    call: &Call,
    thread_id: &ThreadId,
    after: Seq,
) -> Result<(Vec<ThreadEvent>, Seq), String> {
    let mut events = Vec::new();
    let mut last = after;
    loop {
        let path = format!("{}/events?after={}", thread_path(thread_id), last.get());
        let page: Vec<ThreadEvent> = parse(call.get(&path)?)?;
        let Some(newest) = page.last() else {
            return Ok((events, last));
        };
        last = newest.seq;
        events.extend(page);
    }
}

fn thread_path(thread_id: &ThreadId) -> String {
    format!("/api/threads/{}", percent_encode(thread_id.as_str()))
}

/// Everything but unreserved characters, for a path segment or query value.
fn percent_encode(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

impl Call<'_> {
    fn get(&self, path: &str) -> Answer {
        self.send(ApiRequest::new("GET", path))
    }

    fn post(&self, path: &str, body: &Value) -> Answer {
        let body = serde_json::to_vec(body).expect("a JSON value serializes");
        self.send(
            ApiRequest::new("POST", path)
                .header("Content-Type", "application/json")
                .body(body),
        )
    }

    fn send(&self, request: ApiRequest) -> Answer {
        let request = request.header(ACTING_THREAD_HEADER, self.caller.thread_id.as_str());
        reply_body(self.caller.client.send(&request))
    }
}

fn reply_body(reply: Result<ApiReply, ClientError>) -> Answer {
    let reply = reply.map_err(|error| format!("Kratos is unreachable: {error}"))?;
    if (200..300).contains(&reply.status) {
        return Ok(reply.body);
    }
    let reason = match reply.body["error"].as_str() {
        Some(reason) => reason.to_owned(),
        None => reply.body.to_string(),
    };
    Err(format!("Kratos refused ({}): {reason}", reply.status))
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value)
        .map_err(|error| format!("Kratos sent an answer this server cannot read: {error}"))
}

fn no_input() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn thread_input() -> Value {
    json!({ "type": "object", "required": ["threadId"], "additionalProperties": false,
            "properties": { "threadId": { "type": "string" } } })
}

fn list_threads_input() -> Value {
    json!({ "type": "object", "additionalProperties": false,
            "properties": { "workspaceId": { "type": "string" } } })
}

fn read_input() -> Value {
    json!({ "type": "object", "required": ["threadId"], "additionalProperties": false,
            "properties": { "threadId": { "type": "string" },
                            "after": { "type": "integer", "minimum": 0 } } })
}

fn create_input() -> Value {
    json!({ "type": "object", "required": ["title"], "additionalProperties": false,
            "properties": {
                "title": { "type": "string" },
                "workspaceId": { "type": "string" },
                "permission": { "enum": ["askMe", "approveForMe", "fullAccess"] },
                "planMode": { "type": "boolean" },
                "model": { "type": "string" } } })
}

fn send_input() -> Value {
    json!({ "type": "object", "required": ["threadId", "text"], "additionalProperties": false,
            "properties": { "threadId": { "type": "string" }, "text": { "type": "string" } } })
}

fn wait_input() -> Value {
    json!({ "type": "object", "required": ["threadId"], "additionalProperties": false,
            "properties": { "threadId": { "type": "string" },
                            "timeoutSeconds": { "type": "integer", "minimum": 1,
                                                "maximum": MAX_WAIT_SECONDS } } })
}

fn answer_input() -> Value {
    json!({ "type": "object", "required": ["questionId", "answers"], "additionalProperties": false,
            "properties": {
                "questionId": { "type": "string" },
                "answers": { "type": "object", "description": "Each question item's id, mapped to the chosen labels or free text.",
                             "additionalProperties": { "type": "array", "items": { "type": "string" } } } } })
}

fn decide_input() -> Value {
    json!({ "type": "object", "required": ["approvalId", "decision"], "additionalProperties": false,
            "properties": {
                "approvalId": { "type": "string" },
                "decision": { "enum": ["accept", "acceptForSession", "decline", "cancel"] } } })
}
