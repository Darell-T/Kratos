//! Claude tool requests at the trust boundary. Parsing and filesystem resolution precede the pure
//! permission table.

use std::path::{Path, PathBuf};

use kratos_claude_client::{CanUseTool, ControlRequest, IncomingControl};
use kratos_protocol::{
    ApprovalRequest, ExtraAccess, Question, QuestionId, QuestionItem, QuestionOption, ThreadId,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::super::codex::{ProtectedFolders, sandbox_may_write};
use super::actor::Actor;
use super::paths;
use super::policy::{self, Action, Decision, Destination};
use super::requests::{Answer, Pending};
use super::shell::{self, Shell};

/// Answers a control the CLI sends while no turn runs. Kratos's post-tool hooks only record, so
/// they may go on; nothing may run a tool.
pub(super) fn startup(control: IncomingControl) {
    let queued = match &control.request {
        ControlRequest::HookCallback(_) => control.answer(
            json!({ "continue": false, "stopReason": "No tool hook may run before a turn" }),
        ),
        _ => control.fail("No tool may run before a Claude turn starts"),
    };
    if !queued {
        eprintln!("claude: the request was withdrawn before Kratos refused it");
    }
}

/// How Kratos judges a tool it knows by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    File,
    Question,
    ExitPlan,
    LocalRead,
    Read,
    Command(Shell),
    External,
}

/// The rule for `tool_name`, or `None` for a tool Kratos's table does not cover.
fn rule(tool_name: &str) -> Option<Rule> {
    if let Some(shell) = Shell::of(tool_name) {
        return Some(Rule::Command(shell));
    }
    Some(match tool_name {
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => Rule::File,
        "AskUserQuestion" => Rule::Question,
        "ExitPlanMode" => Rule::ExitPlan,
        "Read" | "Glob" | "Grep" => Rule::LocalRead,
        "WebFetch" | "WebSearch" | "TodoWrite" | "EnterPlanMode" => Rule::Read,
        name if name.starts_with("mcp__") => Rule::External,
        _ => return None,
    })
}

/// Whether Kratos's table decides `tool_name`. The pre-tool hook sends each such call to the
/// permission callback, because the CLI on its own runs read-only commands, in-folder reads, and
/// whatever the user's allow rules name without asking (measured with Claude Code 2.1.322).
pub(super) fn decides(tool_name: &str) -> bool {
    rule(tool_name).is_some()
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

fn action(tool: &CanUseTool, folder: &str, protected: &ProtectedFolders) -> Action {
    if !tool.input.is_object() {
        return Action::Invalid;
    }
    match rule(&tool.tool_name) {
        Some(Rule::File) => file_action(tool, folder, protected),
        Some(Rule::Question) => {
            let validation = kratos_protocol::TurnId::new("validation".into());
            if question(&tool.input, validation).is_some() {
                Action::Question
            } else {
                Action::Invalid
            }
        }
        Some(Rule::ExitPlan) => Action::ExitPlan,
        Some(Rule::LocalRead) => local_read_action(tool, folder, protected),
        Some(Rule::Read) => read_action(tool),
        Some(Rule::Command(shell)) => shell::action(shell, &tool.input),
        Some(Rule::External) => Action::ExternalTool,
        None => Action::Invalid,
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

fn read_scope(tool: &CanUseTool, folder: &str, home: Option<&Path>) -> Option<PathBuf> {
    let base = Path::new(folder);
    let raw = if tool.tool_name == "Read" {
        tool.input["file_path"].as_str()?
    } else {
        let pattern = tool.input["pattern"].as_str()?;
        if pattern.trim().is_empty() || pattern.contains('\0') {
            return None;
        }
        match tool.input.get("path") {
            Some(path) => path.as_str()?,
            None => return Some(base.to_path_buf()),
        }
    };
    if raw.trim().is_empty() || raw.contains('\0') {
        return None;
    }
    paths::expand(raw, base, home)
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

fn local_read_action(tool: &CanUseTool, folder: &str, protected: &ProtectedFolders) -> Action {
    let Some(scope) = read_scope(tool, folder, protected.home()) else {
        return Action::Invalid;
    };
    let mut searched = vec![scope.clone()];
    if tool.tool_name == "Glob" {
        let Some(prefix) = tool.input["pattern"].as_str().and_then(glob_prefix) else {
            return Action::Invalid;
        };
        // The CLI may or may not expand the pattern, so a protected reading of either counts, and
        // a reading Kratos cannot name, such as a drive-relative one, makes the search invalid.
        let joined = scope.join(prefix);
        let Some(expanded) = paths::expand(prefix, &scope, protected.home()) else {
            return Action::Invalid;
        };
        if !joined.is_absolute() {
            return Action::Invalid;
        }
        searched.extend([joined, expanded]);
    }
    for path in &searched {
        match protected.overlaps(path) {
            Ok(true) => return Action::ProtectedRead,
            Ok(false) => {}
            Err(_) => return Action::Invalid,
        }
    }
    Action::Read
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
        let decision = policy::decide(
            thread.turn_settings(),
            action(&tool, &thread.folder, &self.settings.protected),
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
    let ControlRequest::CanUseTool(tool) = &pending.control.request else {
        return ApprovalRequest::Permissions {
            reason: Some("Claude tool permission".to_owned()),
            access: ExtraAccess::default(),
        };
    };
    shell::approval(tool, folder).unwrap_or_else(|| ApprovalRequest::Permissions {
        reason: Some(format!("{} {}", tool.tool_name, tool.input)),
        access: ExtraAccess::default(),
    })
}

fn file_action(tool: &CanUseTool, folder: &str, protected: &ProtectedFolders) -> Action {
    let Some(path) = edit_path(&tool.tool_name, &tool.input) else {
        return Action::Invalid;
    };
    let Some(target) = paths::expand(path, Path::new(folder), protected.home()) else {
        return Action::Invalid;
    };
    match protected.containing(&target) {
        Ok(Some(path)) => Action::File(Destination::Protected(path.display().to_string())),
        Ok(None) => match sandbox_may_write(&target, Path::new(folder)) {
            Ok(true) => Action::File(Destination::Inside),
            Ok(false) => Action::File(Destination::Outside),
            Err(_) => Action::Invalid,
        },
        Err(_) => Action::Invalid,
    }
}
