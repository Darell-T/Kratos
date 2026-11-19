use std::path::{Path, PathBuf};

use kratos_claude_client::{CanUseTool, ControlRequest, IncomingControl};
use kratos_protocol::{
    ApprovalRequest, CappedText, ExtraAccess, Question, QuestionId, QuestionItem, QuestionOption,
    ThreadId,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::super::codex::ProtectedFolders;
use super::actor::Actor;
use super::paths;
use super::policy::{self, Action, Decision};
use super::requests::{Answer, Pending};
use super::shell::{self, Shell};
use crate::providers::writes;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    File,
    Question,
    ExitPlan,
    LocalRead,
    Planning,
    Web,
    Command(Shell),
    /// Loads instructions or tool schemas and needs no permission: `ToolSearch`, which the CLI
    /// uses to load deferred tools such as `ExitPlanMode`, and `Skill`.
    Free,
    Unchecked,
}

fn rule(tool_name: &str) -> Rule {
    if let Some(shell) = Shell::of(tool_name) {
        return Rule::Command(shell);
    }
    match tool_name {
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => Rule::File,
        "AskUserQuestion" => Rule::Question,
        "ExitPlanMode" => Rule::ExitPlan,
        "Read" | "Glob" | "Grep" => Rule::LocalRead,
        "TodoWrite" | "EnterPlanMode" => Rule::Planning,
        "WebFetch" | "WebSearch" => Rule::Web,
        "ToolSearch" | "Skill" => Rule::Free,
        _ => Rule::Unchecked,
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

fn action(tool: &CanUseTool, folder: &str, protected: &ProtectedFolders) -> Action {
    if !tool.input.is_object() {
        return Action::Invalid;
    }
    match rule(&tool.tool_name) {
        Rule::File => file_action(tool, folder, protected),
        Rule::Question => {
            let validation = kratos_protocol::TurnId::new("validation".into());
            if question(&tool.input, validation).is_some() {
                Action::Question
            } else {
                Action::Invalid
            }
        }
        Rule::ExitPlan => Action::ExitPlan,
        Rule::LocalRead => local_read_action(tool, folder, protected),
        Rule::Planning => planning_action(tool),
        Rule::Web => web_action(tool),
        Rule::Command(shell) => shell::action(shell, &tool.input),
        Rule::Free => Action::Free,
        Rule::Unchecked => Action::Unchecked,
    }
}

fn planning_action(tool: &CanUseTool) -> Action {
    let readable = tool.tool_name == "EnterPlanMode" || tool.input["todos"].is_array();
    if readable {
        Action::Read
    } else {
        Action::Invalid
    }
}

fn web_action(tool: &CanUseTool) -> Action {
    let key = if tool.tool_name == "WebFetch" {
        "url"
    } else {
        "query"
    };
    if tool.input[key]
        .as_str()
        .is_some_and(|value| !value.trim().is_empty())
    {
        Action::Fetch
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
        let Some(turn_id) = self.threads[thread_id].running_id().cloned() else {
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
        let Some(turn_id) = thread.running_id().cloned() else {
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
    shell::approval(tool, folder).unwrap_or_else(|| {
        let preview = match edit_path(&tool.tool_name, &tool.input) {
            Some(path) => format!("{} {}\n{}", tool.tool_name, json!(path), tool.input),
            None => format!("{} {}", tool.tool_name, tool.input),
        };
        ApprovalRequest::Permissions {
            reason: Some(approval_text(&preview)),
            access: ExtraAccess::default(),
        }
    })
}

/// The most of a tool's input an approval shows. A `Write` asks with the whole file it writes,
/// and every approval is stored and sent to each client, so the text is cut and says so.
const APPROVAL_TEXT_BYTES: usize = 16 * 1024;

fn approval_text(text: &str) -> String {
    let capped = CappedText::new(text, APPROVAL_TEXT_BYTES);
    if capped.omitted_bytes == 0 {
        return capped.text;
    }
    format!(
        "{}... ({} more bytes not shown)",
        capped.text, capped.omitted_bytes
    )
}

fn file_action(tool: &CanUseTool, folder: &str, protected: &ProtectedFolders) -> Action {
    let Some(path) = edit_path(&tool.tool_name, &tool.input) else {
        return Action::Invalid;
    };
    let Some(target) = paths::expand(path, Path::new(folder), protected.home()) else {
        return Action::Invalid;
    };
    match writes::destination(&target, Path::new(folder), protected) {
        Ok(destination) => Action::File(destination),
        Err(_) => Action::Invalid,
    }
}
