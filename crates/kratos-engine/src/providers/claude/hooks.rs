//! Post-tool observations count changes; they never grant a tool permission.

use kratos_claude_client::{HookCallback, HookMatcher, IncomingControl, Initialize};
use kratos_protocol::{AgentItem, CappedText, FileChange, FileChangeKind, ItemStatus, ThreadId};
use serde_json::json;

use super::actor::Actor;
use super::permissions::edit_path;

pub(super) fn initialize(enabled: bool) -> Initialize {
    if !enabled {
        return Initialize::default();
    }
    Initialize {
        sdk_mcp_servers: vec!["kratos".into()],
        hooks: [
            ("PostToolUse", "kratos-post"),
            ("PostToolUseFailure", "kratos-failed"),
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

fn item(hook: &HookCallback) -> Option<AgentItem> {
    let name = hook.input["tool_name"].as_str()?;
    let id = hook.tool_use_id.clone().unwrap_or_default();
    let failed = hook.callback_id == "kratos-failed";
    match name {
        "mcp__kratos__run_command" => Some(AgentItem::Command {
            id,
            command: hook.input["tool_input"]["commandLine"]
                .as_str()
                .unwrap_or_default()
                .into(),
            cwd: None,
            status: if failed {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            },
            exit_code: None,
            output: CappedText::new("", 0),
        }),
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
            "kratos-post" => "PostToolUse",
            "kratos-failed" => "PostToolUseFailure",
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
        if let Some(thread) = self.threads.get_mut(thread_id)
            && thread.run.is_some()
            && let Some(item) = item(&hook)
        {
            thread.changes.record(&item);
        }
        let _ = control.answer(json!({ "continue": true }));
    }
}
