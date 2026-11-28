use kratos_claude_client::{HookCallback, HookMatcher, IncomingControl, Initialize};
use kratos_protocol::{
    AgentItem, CappedText, FileChange, FileChangeKind, ItemStatus, ReportFormat, ThreadId,
};
use std::path::{Component, Path};

use serde_json::{Value, json};

use super::actor::Actor;
use super::permissions::edit_path;
use super::shell::{Shell, command_text};
use crate::providers::codex::resolve_destination;

const PRE: &str = "kratos-pre";
const POST: &str = "kratos-post";
const FAILED: &str = "kratos-failed";

pub(super) fn initialize(report_format: Option<ReportFormat>) -> Initialize {
    Initialize {
        json_schema: report_format.map(|format| Value::Object(format.schema().clone())),
        hooks: [
            ("PreToolUse", PRE),
            ("PostToolUse", POST),
            ("PostToolUseFailure", FAILED),
        ]
        .map(|(event, id)| {
            (
                event.to_owned(),
                vec![HookMatcher {
                    hook_callback_ids: vec![id.into()],
                    ..HookMatcher::default()
                }],
            )
        })
        .into(),
        ..Initialize::default()
    }
}

/// Asking here routes CLI allow rules through Kratos. In plan mode the CLI decides direct edits in its plans directory.
fn pre_tool(hook: &HookCallback, home: Option<&Path>) -> Value {
    if home.is_some_and(|home| edits_in_plan_directory(hook, home)) {
        return json!({ "continue": true });
    }
    json!({
        "continue": true,
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "ask",
            "permissionDecisionReason": "Kratos decides this tool",
        },
    })
}

fn edits_in_plan_directory(hook: &HookCallback, home: &Path) -> bool {
    let Some(path) = direct_plan_edit_path(hook) else {
        return false;
    };
    let Ok(resolved_home) = resolve_destination(home) else {
        return false;
    };
    let expected_plans = resolved_home.join(".claude").join("plans");
    resolve_destination(&expected_plans).is_ok_and(|plans| plans == expected_plans)
        && resolve_destination(path)
            .is_ok_and(|resolved| resolved.parent() == Some(expected_plans.as_path()))
}

fn direct_plan_edit_path(hook: &HookCallback) -> Option<&Path> {
    let name = hook.input["tool_name"].as_str()?;
    let path = Path::new(edit_path(name, &hook.input["tool_input"])?);
    let direct = path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        && !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    direct.then_some(path)
}

fn item(hook: &HookCallback) -> Option<AgentItem> {
    let name = hook.input["tool_name"].as_str()?;
    let id = hook.tool_use_id.clone().unwrap_or_default();
    let failed = hook.callback_id == FAILED;
    if Shell::of(name).is_some() {
        return Some(AgentItem::Command {
            id,
            command: command_text(&hook.input["tool_input"])?.into(),
            cwd: None,
            status: if failed {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            },
            exit_code: None,
            output: CappedText::new("", 0),
        });
    }
    match name {
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" if !failed => {
            let path = edit_path(name, &hook.input["tool_input"])?;
            Some(AgentItem::FileChanges {
                id,
                status: ItemStatus::Completed,
                changes: vec![FileChange {
                    path: path.into(),
                    kind: FileChangeKind::Update { move_path: None },
                    diff: CappedText::new("", 0),
                }],
            })
        }
        _ => None,
    }
}

impl Actor {
    pub(super) fn on_hook(
        &mut self,
        thread_id: &ThreadId,
        control: IncomingControl,
        hook: HookCallback,
    ) {
        let expected = match hook.callback_id.as_str() {
            PRE => "PreToolUse",
            POST => "PostToolUse",
            FAILED => "PostToolUseFailure",
            _ => {
                let _ = control
                    .answer(json!({ "continue": false, "stopReason": "Unknown Kratos hook" }));
                return;
            }
        };
        if hook.input["hook_event_name"] != expected {
            let _ = control
                .answer(json!({ "continue": false, "stopReason": "Unreadable Kratos hook" }));
            return;
        }
        if hook.callback_id == PRE {
            let home = self
                .threads
                .get(thread_id)
                .filter(|thread| thread.turn_settings().plan_mode)
                .and(self.settings.protected.home());
            let _ = control.answer(pre_tool(&hook, home));
            return;
        }
        if let Some(thread) = self.threads.get_mut(thread_id)
            && thread.run.is_some()
            && let Some(item) = item(&hook)
        {
            thread.changes.record(&item);
        }
        let _ = control.answer(json!({ "continue": true }));
    }
}
