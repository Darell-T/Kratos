//! The in-process SDK MCP server. Execution checks policy even without a permission callback.

use std::time::Duration;

use kratos_claude_client::IncomingControl;
use kratos_command_safety::{
    DangerousCommandPlatform, dangerous_command, reads_commands_from_input, split_command_text,
};
use kratos_protocol::ThreadId;
use serde::Deserialize;
use serde_json::{Value, json};

use super::actor::Actor;
use super::policy::{self, Action, Decision};
use super::requests::{Answer, Pending};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct CommandArgs {
    #[serde(skip)]
    shell: String,
    pub command_line: String,
    #[serde(default)]
    pub outside_sandbox: bool,
    pub timeout_ms: Option<u64>,
}

impl CommandArgs {
    pub(super) fn parse(input: Value) -> Option<Self> {
        let mut args: Self = serde_json::from_value(input).ok()?;
        let shell = if cfg!(windows) {
            let root = std::path::PathBuf::from(std::env::var_os("SystemRoot")?);
            if !root.is_absolute() {
                return None;
            }
            root.join("System32/WindowsPowerShell/v1.0/powershell.exe")
        } else {
            std::path::PathBuf::from("/bin/sh")
        };
        args.shell = shell.to_str()?.to_owned();
        (!args.command_line.trim().is_empty()
            && !args.command_line.contains('\0')
            && (1..=600_000).contains(&args.timeout_ms.unwrap_or(30_000)))
        .then_some(args)
    }

    pub(super) fn words(&self) -> Vec<String> {
        if cfg!(windows) {
            [&self.shell, "-NoProfile", "-Command", &self.command_line]
                .map(str::to_owned)
                .to_vec()
        } else {
            [self.shell.as_str(), "-c", &self.command_line]
                .map(str::to_owned)
                .to_vec()
        }
    }

    pub(super) fn action(&self) -> Action {
        let words = self.words();
        Action::Command {
            flagged: dangerous_command(&words, DangerousCommandPlatform::host()).is_some()
                || reads_commands_from_input(&words)
                || split_command_text(&self.command_line).is_none(),
            outside: self.outside_sandbox,
        }
    }
}

pub(super) fn result(id: Value, body: Value) -> Value {
    json!({ "mcp_response": { "jsonrpc": "2.0", "id": id, "result": body } })
}

pub(super) fn error(id: Value, code: i32, message: &str) -> Value {
    json!({ "mcp_response": { "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } } })
}

pub(super) fn metadata(message: &Value) -> Option<Value> {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    match message["method"].as_str()? {
        "initialize" => Some(result(
            id,
            json!({
                "protocolVersion": "2025-11-25", "capabilities": { "tools": {} },
                "serverInfo": { "name": "kratos", "version": env!("CARGO_PKG_VERSION") }
            }),
        )),
        // The recorded CLI bridge expects an MCP envelope even for this notification.
        "notifications/initialized" => Some(result(json!(0), json!({}))),
        "tools/list" => Some(result(
            id,
            json!({ "tools": [{
            "name": "run_command", "description": "Run a command in the thread's folder through Kratos's command sandbox.",
            "inputSchema": { "type": "object", "required": ["commandLine"], "additionalProperties": false,
                "properties": { "commandLine": { "type": "string", "minLength": 1 },
                    "outsideSandbox": { "type": "boolean" },
                    "timeoutMs": { "type": "integer", "minimum": 1, "maximum": 600000 } } }
        }] }),
        )),
        _ => None,
    }
}

fn command_call(message: &Value) -> Result<(Value, CommandArgs), Value> {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    if message["jsonrpc"] != "2.0"
        || message["method"] != "tools/call"
        || message["params"]["name"] != "run_command"
        || !(id.is_string() || id.is_number())
    {
        return Err(error(
            id,
            -32601,
            "Unknown or unreadable Kratos MCP request",
        ));
    }
    let args = CommandArgs::parse(message["params"]["arguments"].clone())
        .ok_or_else(|| error(id.clone(), -32602, "Invalid run_command arguments"))?;
    Ok((id, args))
}

impl Actor {
    pub(super) async fn on_mcp(
        &mut self,
        thread_id: &ThreadId,
        control: IncomingControl,
        message: Value,
    ) {
        if self.threads[thread_id].stopping {
            let _ = control.answer(error(
                message["id"].clone(),
                -32000,
                "The Claude turn is stopping",
            ));
            return;
        }
        if let Some(body) = metadata(&message) {
            let _ = control.answer(body);
            return;
        }
        let (id, args) = match command_call(&message) {
            Ok(call) => call,
            Err(body) => {
                let _ = control.answer(body);
                return;
            }
        };
        let Some(turn_id) = self.threads[thread_id].run.clone() else {
            let _ = control.answer(error(id, -32600, "No Claude turn is running"));
            return;
        };
        let action = args.action();
        let thread = self
            .threads
            .get_mut(thread_id)
            .expect("loaded by on_control");
        let grant = thread
            .command_grants
            .iter()
            .position(|grant| grant == &args);
        let decision = match grant {
            Some(index) => {
                thread.command_grants.remove(index);
                Decision::Allow
            }
            None => policy::decide(thread.turn_settings(), action),
        };
        let pending = Pending {
            thread_id: thread_id.clone(),
            turn_id,
            control,
            answer: Answer::Command { args, id },
        };
        match decision {
            Decision::Allow => self.allow_pending(pending),
            Decision::Ask(why) => self.request_approval(pending, why).await,
            Decision::Deny | Decision::Question => pending.deny(),
        }
    }

    pub(super) fn execute_command(&mut self, pending: Pending, args: CommandArgs, id: Value) {
        let Some(tools) = &self.settings.tools else {
            pending.deny();
            return;
        };
        if !pending.control.is_pending() {
            return;
        }
        let runner = tools.runner.clone();
        let thread = &self.threads[&pending.thread_id];
        let (folder, settings) = (thread.folder.clone(), thread.turn_settings().clone());
        let profile = if args.outside_sandbox && !settings.plan_mode {
            super::super::codex::CommandProfile::ApprovedOutside
        } else {
            super::super::codex::CommandProfile::Claude(settings)
        };
        let control_id = pending.control.id.clone();
        let thread_id = pending.thread_id.clone();
        let epoch = thread.epoch;
        let done = self.done.clone();
        let command = tokio::spawn({
            let control_id = control_id.clone();
            let thread_id = thread_id.clone();
            async move {
                let output = runner
                    .run_at(
                        folder,
                        profile,
                        args.words(),
                        Duration::from_millis(args.timeout_ms.unwrap_or(30_000)),
                    )
                    .await;
                let body = match output {
                    Ok(output) => {
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string(&output).expect("command results serialize") }], "isError": !matches!(output, kratos_protocol::CommandResult::Exited { exit_code: 0, .. }) })
                    }
                    Err(error) => {
                        json!({ "content": [{ "type": "text", "text": error.to_string() }], "isError": true })
                    }
                };
                let _ = pending.control.answer(result(id, body));
                let _ = done.send(super::actor::Done::CommandFinished {
                    thread_id,
                    epoch,
                    control_id,
                });
            }
        });
        self.threads
            .get_mut(&thread_id)
            .expect("pending thread is loaded")
            .commands
            .insert(control_id, command.abort_handle());
    }
}
