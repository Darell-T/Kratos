//! The tools, as a table: adding a tool is adding a row. A tool answers with JSON text, or with a
//! refusal that Codex shows the agent as a failed tool call. Every request names the caller's
//! thread in the `Kratos-Thread` header, and the engine refuses what that thread may not do. A tool
//! reaches only the routes the engine opens to agents, in `server::agent_routes`.

use std::collections::BTreeMap;
use std::time::Duration;

use kratos_api_client::{ApiReply, ClientError};
use kratos_protocol::{
    AnswerQuestion, ApprovalDecision, ApprovalId, CreateThread, DecideApproval, ErrorBody,
    MessageInput, Permission, Provider, QuestionId, RequiredText, SendMessage, Seq, ThreadDetails,
    ThreadEvent, ThreadId, ThreadSettings, WorkspaceId, routes,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Call, MAX_WAIT_SECONDS, transcript, wait};

type Answer = Result<Value, String>;

struct Tool {
    name: &'static str,
    description: &'static str,
    input: fn() -> Value,
    run: fn(&Call, Value) -> Answer,
}

const DEFAULT_WAIT_SECONDS: u64 = 300;

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

pub(super) fn list() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|tool| {
            json!({ "name": tool.name, "description": tool.description,
                    "inputSchema": (tool.input)() })
        })
        .collect()
}

#[derive(Deserialize)]
pub(crate) struct ToolCall {
    name: String,
    arguments: Option<Value>,
}

pub(super) fn call(call: &Call, ToolCall { name, arguments }: ToolCall) -> Value {
    let arguments = arguments.unwrap_or_else(|| json!({}));
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
    call.get(&routes::thread(&call.caller.thread_id))
}

fn list_workspaces(call: &Call, arguments: Value) -> Answer {
    let NoArguments {} = parse_arguments(arguments)?;
    call.get(routes::WORKSPACES)
}

fn list_threads(call: &Call, arguments: Value) -> Answer {
    let InWorkspace { workspace_id } = parse_arguments(arguments)?;
    let workspace = match workspace_id {
        Some(id) => WorkspaceId::new(id.into_string()),
        None => me(call)?.thread.workspace_id,
    };
    call.get(&routes::threads_in(&workspace))
}

fn get_thread(call: &Call, arguments: Value) -> Answer {
    let OnThread { thread_id } = parse_arguments(arguments)?;
    call.get(&routes::thread(&thread_id))
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
        model: request.model,
        ..ThreadSettings::default()
    };
    let workspace_id = match request.workspace_id {
        Some(id) => WorkspaceId::new(id.into_string()),
        None => me.thread.workspace_id,
    };
    call.post(
        routes::THREADS,
        &CreateThread {
            workspace_id,
            title: request.title,
            provider: Provider::Codex,
            settings: Some(settings),
        },
    )
}

fn send_message(call: &Call, arguments: Value) -> Answer {
    let SendArguments { thread_id, text } = parse_arguments(arguments)?;
    call.post(
        &routes::thread_messages(&thread_id),
        &SendMessage {
            message_id: None,
            input: vec![MessageInput::Text { text }],
        },
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
    let details: ThreadDetails = parse(call.get(&routes::thread(&thread_id))?)?;
    let outcome = wait::wait(call, thread_id, details, Duration::from_secs(seconds))?;
    Ok(serde_json::to_value(outcome).expect("a wait outcome serializes"))
}

fn stop_thread(call: &Call, arguments: Value) -> Answer {
    let OnThread { thread_id } = parse_arguments(arguments)?;
    call.post(&routes::thread_stop(&thread_id), &json!({}))
}

fn answer_question(call: &Call, arguments: Value) -> Answer {
    let AnswerArguments {
        question_id,
        answers,
    } = parse_arguments(arguments)?;
    call.post(
        &routes::question(&QuestionId::new(question_id.into_string())),
        &AnswerQuestion { answers },
    )
}

fn decide_approval(call: &Call, arguments: Value) -> Answer {
    let DecideArguments {
        approval_id,
        decision,
    } = parse_arguments(arguments)?;
    call.post(
        &routes::approval(&ApprovalId::new(approval_id.into_string())),
        &DecideApproval { decision },
    )
}

fn me(call: &Call) -> Result<ThreadDetails, String> {
    parse(call.get(&routes::thread(&call.caller.thread_id))?)
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
        let page: Vec<ThreadEvent> =
            parse(call.get(&routes::thread_events_after(thread_id, last))?)?;
        let Some(newest) = page.last() else {
            return Ok((events, last));
        };
        last = newest.seq;
        events.extend(page);
    }
}

impl Call<'_> {
    fn get(&self, path: &str) -> Answer {
        reply_body(self.caller.client.get(path))
    }

    fn post(&self, path: &str, body: &impl Serialize) -> Answer {
        reply_body(self.caller.client.post(path, body))
    }
}

fn reply_body(reply: Result<ApiReply, ClientError>) -> Answer {
    let reply = reply.map_err(|error| format!("Kratos is unreachable: {error}"))?;
    if (200..300).contains(&reply.status) {
        return Ok(reply.body);
    }
    let reason = match ErrorBody::deserialize(&reply.body) {
        Ok(ErrorBody { error }) => error,
        Err(_) => reply.body.to_string(),
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
                "permission": { "enum": Permission::ALL },
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
                "decision": { "enum": ApprovalDecision::ALL } } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_for(tool: &str, arguments: Value) -> Result<(), String> {
        match tool {
            "whoami" | "list_workspaces" => parse_arguments::<NoArguments>(arguments).map(drop),
            "list_threads" => parse_arguments::<InWorkspace>(arguments).map(drop),
            "get_thread" | "stop_thread" => parse_arguments::<OnThread>(arguments).map(drop),
            "read_thread" => parse_arguments::<ReadArguments>(arguments).map(drop),
            "create_thread" => parse_arguments::<CreateArguments>(arguments).map(drop),
            "send_message" => parse_arguments::<SendArguments>(arguments).map(drop),
            "wait_for_turn" => parse_arguments::<WaitArguments>(arguments).map(drop),
            "answer_question" => parse_arguments::<AnswerArguments>(arguments).map(drop),
            "decide_approval" => parse_arguments::<DecideArguments>(arguments).map(drop),
            other => panic!("no arguments struct named for tool {other}"),
        }
    }

    fn samples(schema: &Value) -> Vec<Value> {
        if let Some(values) = schema["enum"].as_array() {
            return values.clone();
        }
        match schema["type"].as_str() {
            Some("string") => vec![json!("x")],
            Some("integer") => vec![schema.get("minimum").cloned().unwrap_or(json!(1))],
            Some("boolean") => vec![json!(true)],
            Some("object") => samples(&schema["additionalProperties"])
                .into_iter()
                .map(|value| json!({ "k": value }))
                .collect(),
            Some("array") => samples(&schema["items"])
                .into_iter()
                .map(|value| json!([value]))
                .collect(),
            other => panic!("no sample for a property of type {other:?}"),
        }
    }

    #[test]
    fn every_tool_schema_describes_the_arguments_its_tool_parses() {
        // Catches a schema that advertises a property, a type, or an enum value the tool refuses,
        // or that marks a property optional or required unlike the tool's struct.
        for tool in &TOOLS {
            let schema = (tool.input)();
            let properties = schema["properties"].as_object().unwrap();
            let required: Vec<&str> = schema["required"]
                .as_array()
                .map(|names| names.iter().map(|name| name.as_str().unwrap()).collect())
                .unwrap_or_default();
            let full: serde_json::Map<String, Value> = properties
                .iter()
                .map(|(name, property)| (name.clone(), samples(property)[0].clone()))
                .collect();
            let minimal: serde_json::Map<String, Value> = full
                .iter()
                .filter(|(name, _)| required.contains(&name.as_str()))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            assert_eq!(minimal.len(), required.len(), "{}: required", tool.name);

            parse_for(tool.name, Value::Object(full.clone()))
                .unwrap_or_else(|error| panic!("{}: every property: {error}", tool.name));
            parse_for(tool.name, Value::Object(minimal.clone()))
                .unwrap_or_else(|error| panic!("{}: required only: {error}", tool.name));
            for (name, property) in properties {
                for sample in samples(property) {
                    let mut arguments = full.clone();
                    arguments.insert(name.clone(), sample.clone());
                    parse_for(tool.name, Value::Object(arguments))
                        .unwrap_or_else(|error| panic!("{}: {name}={sample}: {error}", tool.name));
                }
            }
            for name in &required {
                let mut arguments = minimal.clone();
                arguments.remove(*name);
                assert!(
                    parse_for(tool.name, Value::Object(arguments)).is_err(),
                    "{}: {name} is required by the schema only",
                    tool.name
                );
            }
        }
    }
}
