//! Claude tool requests at the trust boundary. Without the command service the provider refuses
//! every request; with it, parsing precedes the pure permission table.

use std::path::Path;

use kratos_claude_client::{CanUseTool, ControlRequest, IncomingControl};
use kratos_protocol::{
    ApprovalRequest, ExtraAccess, Question, QuestionId, QuestionItem, QuestionOption, ThreadId,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::actor::Actor;
use super::policy::{self, Action, Decision, Destination};
use super::requests::{Answer, Pending};
use super::tools::{self, CommandArgs};

const DENIED: &str = "Kratos's command service is unavailable, so it declines every tool request";

pub(super) fn answer(control: IncomingControl) {
    let queued = match &control.request {
        ControlRequest::CanUseTool(_) => {
            control.answer(json!({ "behavior": "deny", "message": DENIED }))
        }
        ControlRequest::Elicitation(_) => control.answer(json!({ "action": "decline" })),
        ControlRequest::HookCallback(_) => control.fail("Kratos registers no hooks"),
        ControlRequest::McpMessage { .. } => control.fail("Kratos serves no MCP servers yet"),
        ControlRequest::Unsupported { .. } => control.fail("Kratos does not answer this request"),
    };
    if !queued {
        eprintln!("claude: the request was withdrawn before Kratos refused it");
    }
}

pub(super) fn startup(control: IncomingControl, enabled: bool) {
    if !enabled {
        answer(control);
        return;
    }
    let body = match &control.request {
        ControlRequest::McpMessage {
            server_name,
            message,
        } if server_name == "kratos" => tools::metadata(message),
        ControlRequest::HookCallback(_) => {
            Some(json!({ "continue": false, "stopReason": "No tool hook may run before a turn" }))
        }
        _ => None,
    };
    match body {
        Some(body) => {
            let _ = control.answer(body);
        }
        None => {
            let _ = control.fail("No tool may run before a Claude turn starts");
        }
    }
}

pub(super) fn edit_path<'a>(tool: &str, input: &'a Value) -> Option<&'a str> {
    let key = match tool {
        "Write" | "Edit" | "MultiEdit" => "file_path",
        "NotebookEdit" => "notebook_path",
        _ => return None,
    };
    input[key]
        .as_str()
        .filter(|path| !path.trim().is_empty() && !path.contains('\0'))
}

fn action(
    tool: &CanUseTool,
    folder: &str,
    protected: &super::super::codex::ProtectedFolders,
) -> Action {
    if !tool.input.is_object() {
        return Action::Invalid;
    }
    match tool.tool_name.as_str() {
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => file_action(tool, folder, protected),
        "AskUserQuestion" => {
            if question(
                &tool.input,
                kratos_protocol::TurnId::new("validation".into()),
            )
            .is_some()
            {
                Action::Question
            } else {
                Action::Invalid
            }
        }
        "ExitPlanMode" => Action::ExitPlan,
        "Read" | "Glob" | "Grep" => local_read_action(tool, folder, protected),
        "WebFetch" | "WebSearch" | "TodoWrite" | "EnterPlanMode" => read_action(tool),
        name if name.starts_with("mcp__") => mcp_action(tool),
        _ => Action::Invalid,
    }
}

fn read_action(tool: &CanUseTool) -> Action {
    let key = match tool.tool_name.as_str() {
        "WebFetch" => "url",
        "WebSearch" => "query",
        "TodoWrite" => {
            return if tool.input["todos"].is_array() {
                Action::Read
            } else {
                Action::Invalid
            };
        }
        "EnterPlanMode" => return Action::Read,
        _ => return Action::Invalid,
    };
    if tool.input[key]
        .as_str()
        .is_some_and(|value| !value.trim().is_empty())
    {
        Action::Read
    } else {
        Action::Invalid
    }
}

fn read_scope(tool: &CanUseTool, folder: &str) -> Option<std::path::PathBuf> {
    let path = if tool.tool_name == "Read" {
        tool.input["file_path"].as_str()?
    } else {
        let pattern = tool.input["pattern"].as_str()?;
        if pattern.trim().is_empty() || pattern.contains('\0') {
            return None;
        }
        match tool.input.get("path") {
            Some(path) => path.as_str()?,
            None => folder,
        }
    };
    (!path.trim().is_empty() && !path.contains('\0')).then(|| Path::new(folder).join(path))
}

fn glob_prefix(pattern: &str) -> Option<&str> {
    let end = pattern
        .find(['*', '?', '[', '{', '('])
        .unwrap_or(pattern.len());
    let dynamic = &pattern[end..];
    // A dynamic glob must not choose another root or climb out of its checked scope.
    let escapes = [
        "..", ":", "~", "{/", "{\\", ",/", ",\\", "(/", "(\\", "|/", "|\\",
    ];
    (!escapes.iter().any(|part| dynamic.contains(part))).then_some(&pattern[..end])
}

fn local_read_action(
    tool: &CanUseTool,
    folder: &str,
    protected: &super::super::codex::ProtectedFolders,
) -> Action {
    let Some(scope) = read_scope(tool, folder) else {
        return Action::Invalid;
    };
    let target = if tool.tool_name == "Glob" {
        let Some(prefix) = tool.input["pattern"].as_str().and_then(glob_prefix) else {
            return Action::Invalid;
        };
        scope.join(prefix)
    } else {
        scope.clone()
    };
    for path in [&scope, &target] {
        match protected.overlaps(path) {
            Ok(true) => return Action::ProtectedRead,
            Ok(false) => {}
            Err(_) => return Action::Invalid,
        }
    }
    Action::Read
}

fn mcp_action(tool: &CanUseTool) -> Action {
    let sdk = tool
        .mcp_server
        .as_ref()
        .is_some_and(|server| server.source == "sdk" && server.name == "kratos");
    if tool.tool_name != "mcp__kratos__run_command" || !sdk {
        return Action::ExternalTool;
    }
    CommandArgs::parse(tool.input.clone()).map_or(Action::Invalid, |args| args.action())
}

#[derive(Deserialize)]
struct Questions {
    questions: Vec<Asked>,
}
#[derive(Deserialize)]
struct Asked {
    question: String,
    header: String,
    options: Vec<QuestionOption>,
}

fn question(input: &Value, turn_id: kratos_protocol::TurnId) -> Option<Question> {
    let parsed: Questions = serde_json::from_value(input.clone()).ok()?;
    if parsed.questions.is_empty()
        || parsed
            .questions
            .iter()
            .any(|item| item.question.trim().is_empty())
    {
        return None;
    }
    Some(Question {
        id: QuestionId::new(Uuid::new_v4().to_string()),
        turn_id,
        blocking: true,
        items: parsed
            .questions
            .into_iter()
            .map(|item| QuestionItem {
                id: item.question.clone(),
                question: item.question,
                header: item.header,
                options: item.options,
                allows_other: true,
                secret: false,
            })
            .collect(),
    })
}

impl Actor {
    pub(super) async fn on_control(&mut self, thread_id: &ThreadId, control: IncomingControl) {
        if self.settings.tools.is_none() {
            answer(control);
            return;
        }
        if !control.is_pending() {
            return;
        }
        if self.threads[thread_id].stopping
            && matches!(control.request, ControlRequest::CanUseTool(_))
        {
            let _ = control
                .answer(json!({ "behavior": "deny", "message": "The Claude turn is stopping" }));
            return;
        }
        match control.request.clone() {
            ControlRequest::CanUseTool(tool) => self.on_tool(thread_id, control, tool).await,
            ControlRequest::McpMessage {
                server_name,
                message,
            } if server_name == "kratos" => self.on_mcp(thread_id, control, message).await,
            ControlRequest::HookCallback(hook) => self.on_hook(thread_id, control, hook),
            ControlRequest::Elicitation(request) => {
                self.on_elicitation(thread_id, control, request).await
            }
            _ => {
                let _ = control.fail("Unknown Claude control or MCP server");
            }
        }
    }

    async fn on_elicitation(
        &mut self,
        thread_id: &ThreadId,
        control: IncomingControl,
        request: kratos_claude_client::Elicitation,
    ) {
        let Some(turn_id) = self.threads[thread_id].run.clone() else {
            let _ = control.answer(json!({ "action": "cancel" }));
            return;
        };
        let question = Question {
            id: QuestionId::new(Uuid::new_v4().to_string()),
            turn_id: turn_id.clone(),
            blocking: true,
            items: vec![QuestionItem {
                id: "response".into(),
                header: request.mcp_server_name,
                question: match request.url {
                    Some(url) => format!("{}\n{url}", request.message),
                    None => request.message,
                },
                options: Vec::new(),
                allows_other: true,
                secret: false,
            }],
        };
        self.request_question(
            Pending {
                thread_id: thread_id.clone(),
                turn_id,
                control,
                answer: Answer::Elicitation,
            },
            question,
        )
        .await;
    }

    async fn on_tool(&mut self, thread_id: &ThreadId, control: IncomingControl, tool: CanUseTool) {
        let thread = &self.threads[thread_id];
        let Some(turn_id) = thread.run.clone() else {
            let _ = control
                .answer(json!({ "behavior": "deny", "message": "No Claude turn is running" }));
            return;
        };
        let protected = &self
            .settings
            .tools
            .as_ref()
            .expect("checked by on_control")
            .protected;
        let decision = policy::decide(
            thread.turn_settings(),
            action(&tool, &thread.folder, protected),
        );
        let pending = Pending {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            control,
            answer: Answer::Tool(tool.input.clone()),
        };
        match decision {
            Decision::Allow => self.allow_pending(pending),
            Decision::Ask(why) => self.request_approval(pending, why).await,
            Decision::Question => {
                let question = question(&tool.input, turn_id).expect("validated before deciding");
                self.request_question(pending, question).await;
            }
            Decision::Deny => {
                let _ = pending.control.answer(json!({ "behavior": "deny", "message": "Kratos denied a protected read or an unknown, unreadable, disabled, or plan-exit tool request" }));
            }
        }
    }
}

pub(super) fn shown(pending: &Pending, folder: &str) -> ApprovalRequest {
    if let Answer::Command { args, .. } = &pending.answer {
        return ApprovalRequest::Command {
            command: args.command_line.clone(),
            cwd: Some(folder.to_owned()),
            reason: args
                .outside_sandbox
                .then(|| "Run outside the sandbox, including protected folders".into()),
            access: if args.outside_sandbox {
                ExtraAccess {
                    paths: vec![kratos_protocol::PathAccess {
                        path: ":root".into(),
                        access: kratos_protocol::PathAccessMode::Write,
                    }],
                    network: Some(kratos_protocol::NetworkAccess::Any),
                }
            } else {
                ExtraAccess::default()
            },
        };
    }
    let reason = match &pending.control.request {
        ControlRequest::CanUseTool(tool) => format!("{} {}", tool.tool_name, tool.input),
        _ => "Claude tool permission".to_owned(),
    };
    ApprovalRequest::Permissions {
        reason: Some(reason),
        access: ExtraAccess::default(),
    }
}

fn file_action(
    tool: &CanUseTool,
    folder: &str,
    protected: &super::super::codex::ProtectedFolders,
) -> Action {
    let Some(path) = edit_path(&tool.tool_name, &tool.input) else {
        return Action::Invalid;
    };
    match protected.containing(&Path::new(folder).join(path)) {
        Ok(Some(path)) => Action::File(Destination::Protected(path.display().to_string())),
        Ok(None) => Action::File(if Path::new(folder).join(path).starts_with(folder) {
            Destination::Inside
        } else {
            Destination::Outside
        }),
        Err(_) => Action::Invalid,
    }
}
