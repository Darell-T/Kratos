//! Codex's generated protocol types in Kratos's terms, and back. Nothing else in the engine reads a
//! Codex item or writes a Codex request or answer by hand.

use std::collections::{BTreeMap, HashMap};

use std::time::{Duration, SystemTime};

use kratos_codex_client::{RequestError, ServerRequest, ServerResponse, types};
use kratos_protocol::{
    AccessLimit, AccountUsage, AgentItem, Allowance, ApprovalDecision, CappedText, CommandResult,
    ContextUsage, ExtraAccess, FileChange, FileChangeKind, ItemStatus, Message, MessageInput,
    MessageOrigin, NetworkAccess, PathAccess, PathAccessMode, Permission, Provider, Question,
    QuestionId, QuestionItem, QuestionOption, ThreadId, Timestamp, TurnId, TurnOutcome,
    UsageBucket, UsageWindow,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The most of a command's output a stored event keeps (L3). A proposal until measured on real
/// sessions: Codex already shortens what it shows the model.
pub const OUTPUT_BYTES: usize = 16 * 1024;
/// The most of one file's diff a stored event keeps (L3), also a proposal.
pub const DIFF_BYTES: usize = 32 * 1024;

/// A request type with required fields, built from them alone so every optional field is unset.
/// The caller then sets the optional fields it needs by name, so the compiler checks them.
pub fn required<T: DeserializeOwned>(fields: Value) -> T {
    serde_json::from_value(fields).expect("the required fields match the generated type")
}

/// A message's parts as Codex input. Codex has no file part, so a file goes in as text naming it,
/// the way Codex's own composer inserts a file's path. A message from another thread opens with
/// a label that names the sender and what this thread may now do.
pub fn user_input(message: &Message) -> Vec<types::UserInput> {
    let label = match &message.origin {
        MessageOrigin::Thread { thread_id, limit } => {
            Some(text_input(&sender_label(thread_id, *limit)))
        }
        MessageOrigin::User | MessageOrigin::Continuation { .. } => None,
    };
    let parts = message.input.iter().map(|part| match part {
        MessageInput::Text { text } => text_input(text.as_str()),
        MessageInput::Image { path } => types::UserInput::LocalImageUserInput {
            detail: None,
            path: path.as_str().to_owned(),
            type_: types::LocalImageUserInputType::LocalImage,
        },
        MessageInput::File { path } => text_input(&format!("Attached file: {}", path.as_str())),
    });
    label.into_iter().chain(parts).collect()
}

fn sender_label(thread_id: &ThreadId, limit: AccessLimit) -> String {
    let access = match (limit.plan_mode, limit.permission) {
        (true, _) => "Plan mode, which changes nothing",
        (false, Permission::AskMe) => "Ask me",
        (false, Permission::ApproveForMe) => "Approve for me",
        (false, Permission::FullAccess) => "Full access",
    };
    format!(
        "[Message from Kratos thread {}. From this turn on, this thread runs with no more than that \
         thread's permission, {access}, until the user raises it. If that stops you from doing \
         something, say so in your reply instead of working around it. Answer in your reply, which \
         that thread reads. Message it back only if it asks you to.]",
        thread_id.as_str()
    )
}

fn text_input(text: &str) -> types::UserInput {
    types::UserInput::TextUserInput {
        text: text.to_owned(),
        text_elements: Vec::new(),
        type_: types::TextUserInputType::Text,
    }
}

/// A finished or started item as Kratos records it. `None` for items Kratos records another way:
/// the user's own message is the queued message, and reasoning is not shown.
pub fn agent_item(item: &types::ThreadItem) -> Option<AgentItem> {
    use types::ThreadItem as Item;
    Some(match item {
        Item::UserMessage { .. } | Item::HookPrompt { .. } | Item::Reasoning { .. } => return None,
        Item::AgentMessage { id, text, .. } => AgentItem::Message {
            id: id.clone(),
            text: text.clone(),
        },
        Item::Plan { id, text } => AgentItem::Plan {
            id: id.clone(),
            text: text.clone(),
        },
        Item::CommandExecution {
            id,
            command,
            cwd,
            status,
            exit_code,
            aggregated_output,
            ..
        } => AgentItem::Command {
            id: id.clone(),
            command: command.clone(),
            cwd: Some(cwd.0.clone()),
            status: command_status(status),
            exit_code: *exit_code,
            output: CappedText::new(
                aggregated_output.as_deref().unwrap_or_default(),
                OUTPUT_BYTES,
            ),
        },
        Item::FileChange {
            id,
            status,
            changes,
        } => AgentItem::FileChanges {
            id: id.clone(),
            status: patch_status(status),
            changes: file_changes(changes),
        },
        other => {
            let value = serde_json::to_value(other).expect("Codex items serialize");
            AgentItem::Other {
                id: value["id"].as_str().unwrap_or_default().to_owned(),
                provider_type: value["type"].as_str().unwrap_or_default().to_owned(),
            }
        }
    })
}

pub fn file_changes(changes: &[types::FileUpdateChange]) -> Vec<FileChange> {
    changes
        .iter()
        .map(|change| FileChange {
            path: change.path.clone(),
            kind: match &change.kind {
                types::PatchChangeKind::Add => FileChangeKind::Add,
                types::PatchChangeKind::Delete => FileChangeKind::Delete,
                types::PatchChangeKind::Update { move_path } => FileChangeKind::Update {
                    move_path: move_path.clone(),
                },
            },
            diff: CappedText::new(&change.diff, DIFF_BYTES),
        })
        .collect()
}

fn command_status(status: &types::CommandExecutionStatus) -> ItemStatus {
    match status {
        types::CommandExecutionStatus::InProgress => ItemStatus::InProgress,
        types::CommandExecutionStatus::Completed => ItemStatus::Completed,
        types::CommandExecutionStatus::Failed => ItemStatus::Failed,
        types::CommandExecutionStatus::Declined => ItemStatus::Declined,
    }
}

fn patch_status(status: &types::PatchApplyStatus) -> ItemStatus {
    match status {
        types::PatchApplyStatus::InProgress => ItemStatus::InProgress,
        types::PatchApplyStatus::Completed => ItemStatus::Completed,
        types::PatchApplyStatus::Failed => ItemStatus::Failed,
        types::PatchApplyStatus::Declined => ItemStatus::Declined,
    }
}

/// How a turn Codex reports finished ended. `None` while it is still in progress.
pub fn turn_outcome(turn: &types::Turn) -> Option<TurnOutcome> {
    Some(match turn.status {
        types::TurnStatus::InProgress => return None,
        types::TurnStatus::Completed => TurnOutcome::Completed,
        types::TurnStatus::Interrupted => TurnOutcome::Interrupted,
        types::TurnStatus::Failed => TurnOutcome::Failed {
            error: turn.error.as_ref().map_or_else(
                || "Codex turn failed".to_owned(),
                |error| error.message.clone(),
            ),
        },
    })
}

/// Whether the turn stopped because the account ran out of its included usage (D39).
pub fn stopped_at_usage_limit(turn: &types::Turn) -> bool {
    matches!(
        turn.error
            .as_ref()
            .and_then(|error| error.codex_error_info.as_ref()),
        Some(types::CodexErrorInfo::UsageLimitExceeded)
    )
}

/// The context one turn used: Codex's `last` counts are the latest request's, which is what fills
/// the window (D76).
pub fn context_usage(usage: &types::ThreadTokenUsage) -> ContextUsage {
    let last = &usage.last;
    ContextUsage {
        input_tokens: last.input_tokens,
        cached_input_tokens: last.cached_input_tokens,
        output_tokens: last.output_tokens,
        reasoning_output_tokens: last.reasoning_output_tokens,
        total_tokens: last.total_tokens,
        context_window: usage.model_context_window,
    }
}

pub fn question(id: QuestionId, params: &types::ToolRequestUserInputParams) -> Question {
    Question {
        id,
        turn_id: TurnId::new(params.turn_id.clone()),
        items: params
            .questions
            .iter()
            .map(|item| QuestionItem {
                id: item.id.clone(),
                header: item.header.clone(),
                question: item.question.clone(),
                options: item
                    .options
                    .iter()
                    .flatten()
                    .map(|option| QuestionOption {
                        label: option.label.clone(),
                        description: option.description.clone(),
                    })
                    .collect(),
                allows_other: item.is_other,
                secret: item.is_secret,
            })
            .collect(),
        blocking: params.is_blocking,
    }
}

pub fn answers(answers: BTreeMap<String, Vec<String>>) -> types::ToolRequestUserInputResponse {
    types::ToolRequestUserInputResponse {
        answers: answers
            .into_iter()
            .map(|(id, answers)| (id, types::ToolRequestUserInputAnswer { answers }))
            .collect::<HashMap<_, _>>(),
    }
}

pub fn command_decision(
    decision: ApprovalDecision,
) -> types::CommandExecutionRequestApprovalResponse {
    use types::CommandExecutionApprovalDecision as Codex;
    let decision = match decision {
        ApprovalDecision::Accept => Codex::Accept,
        ApprovalDecision::AcceptForSession => Codex::AcceptForSession,
        ApprovalDecision::Decline => Codex::Decline,
        ApprovalDecision::Cancel => Codex::Cancel,
    };
    types::CommandExecutionRequestApprovalResponse { decision }
}

pub fn file_change_decision(
    decision: ApprovalDecision,
) -> types::FileChangeRequestApprovalResponse {
    use types::FileChangeApprovalDecision as Codex;
    let decision = match decision {
        ApprovalDecision::Accept => Codex::Accept,
        ApprovalDecision::AcceptForSession => Codex::AcceptForSession,
        ApprovalDecision::Decline => Codex::Decline,
        ApprovalDecision::Cancel => Codex::Cancel,
    };
    types::FileChangeRequestApprovalResponse { decision }
}

/// Codex's answer to a request for extra permissions: what it asked for, for this turn or the
/// session, or nothing.
pub fn permissions_decision(
    decision: ApprovalDecision,
    requested: &types::RequestPermissionProfile,
) -> types::PermissionsRequestApprovalResponse {
    let granted = types::GrantedPermissionProfile {
        file_system: requested.file_system.clone(),
        network: requested.network.clone(),
    };
    let (permissions, scope) = match decision {
        ApprovalDecision::Accept => (granted, types::PermissionGrantScope::Turn),
        ApprovalDecision::AcceptForSession => (granted, types::PermissionGrantScope::Session),
        ApprovalDecision::Decline | ApprovalDecision::Cancel => (
            types::GrantedPermissionProfile::default(),
            types::PermissionGrantScope::Turn,
        ),
    };
    types::PermissionsRequestApprovalResponse {
        permissions,
        scope,
        strict_auto_review: None,
    }
}

/// The answer to a request Kratos will not put to the user: the refusal of its kind.
pub fn refusal(request: &ServerRequest) -> ServerResponse {
    match request {
        ServerRequest::CommandApproval(_) => {
            ServerResponse::CommandApproval(command_decision(ApprovalDecision::Cancel))
        }
        ServerRequest::FileChangeApproval(_) => {
            ServerResponse::FileChangeApproval(file_change_decision(ApprovalDecision::Cancel))
        }
        ServerRequest::Permissions(params) => ServerResponse::Permissions(permissions_decision(
            ApprovalDecision::Decline,
            &params.permissions,
        )),
        ServerRequest::UserInput(_) => ServerResponse::UserInput(answers(BTreeMap::new())),
        ServerRequest::McpElicitation(_) => {
            ServerResponse::McpElicitation(types::McpServerElicitationRequestResponse {
                action: types::McpServerElicitationAction::Cancel,
                // Codex has always been sent both keys as explicit nulls.
                content: Some(Value::Null),
                meta: Some(Value::Null),
            })
        }
    }
}

/// What accepting a request for extra permissions grants. Accepting answers Codex with
/// `requested` itself, so the user sees what Codex gets.
pub fn permissions_access(requested: &types::RequestPermissionProfile) -> ExtraAccess {
    extra_access(requested.file_system.as_ref(), requested.network.as_ref())
}

/// What accepting a command's approval lets the command use beyond the sandbox: the extra
/// permissions it asks for, and the network host it wants to reach.
pub fn command_access(params: &types::CommandExecutionRequestApprovalParams) -> ExtraAccess {
    let mut access = params
        .additional_permissions
        .as_ref()
        .map_or_else(ExtraAccess::default, |extra| {
            extra_access(extra.file_system.as_ref(), extra.network.as_ref())
        });
    if access.network.is_none()
        && let Some(context) = &params.network_approval_context
    {
        access.network = Some(NetworkAccess::Host {
            host: context.host.clone(),
            protocol: context.protocol.to_string(),
        });
    }
    access
}

fn extra_access(
    file_system: Option<&types::AdditionalFileSystemPermissions>,
    network: Option<&types::AdditionalNetworkPermissions>,
) -> ExtraAccess {
    let mut paths = Vec::new();
    if let Some(file_system) = file_system {
        for entry in file_system.entries.iter().flatten() {
            let access = match entry.access {
                types::FileSystemAccessMode::Read => PathAccessMode::Read,
                types::FileSystemAccessMode::Write => PathAccessMode::Write,
                types::FileSystemAccessMode::Deny => PathAccessMode::Deny,
            };
            paths.push(PathAccess {
                path: path_name(&entry.path),
                access,
            });
        }
        let listed = [
            (&file_system.read, PathAccessMode::Read),
            (&file_system.write, PathAccessMode::Write),
        ];
        for (list, access) in listed {
            for path in list.iter().flatten() {
                paths.push(PathAccess {
                    path: path.0.clone(),
                    access,
                });
            }
        }
    }
    let is_network_on = network.is_some_and(|network| network.enabled == Some(true));
    ExtraAccess {
        paths,
        network: is_network_on.then_some(NetworkAccess::Any),
    }
}

/// The path, the glob pattern, or Codex's name for a special location, such as `:root`.
fn path_name(path: &types::FileSystemPath) -> String {
    use types::FileSystemSpecialPath as Special;
    let (name, subpath) = match path {
        types::FileSystemPath::Path { path } => return path.0.clone(),
        types::FileSystemPath::GlobPattern { pattern } => return pattern.clone(),
        types::FileSystemPath::Special { value } => match value {
            Special::Root => (":root", None),
            Special::Minimal => (":minimal", None),
            Special::ProjectRoots { subpath } => (":project_roots", subpath.as_ref()),
            Special::Tmpdir => (":tmpdir", None),
            Special::SlashTmp => (":slash_tmp", None),
            Special::Unknown { path, subpath } => (path.as_str(), subpath.as_ref()),
        },
    };
    match subpath {
        Some(subpath) => format!("{name}/{}", subpath.0),
        None => name.to_owned(),
    }
}

/// The turn of `turns` that holds the user message Kratos sent as `message_id`.
pub fn turn_with_message<'a>(
    turns: &'a [types::Turn],
    message_id: &str,
) -> Option<&'a types::Turn> {
    turns.iter().find(|turn| {
        turn.items.iter().any(|item| {
            matches!(item, types::ThreadItem::UserMessage { client_id: Some(id), .. } if id == message_id)
        })
    })
}

/// The plan that pays for the signed-in account (D24). Never its email.
pub fn plan(read: &types::GetAccountResponse) -> Option<String> {
    match &read.account {
        Some(types::Account::Chatgpt { plan_type, .. }) => Some(plan_type.to_string()),
        Some(types::Account::ApiKey) => Some("apiKey".to_owned()),
        Some(types::Account::AmazonBedrock { .. }) => Some("amazonBedrock".to_owned()),
        None => None,
    }
}

/// A full usage read (D38). `ordinaryUsageAllowed` alone says whether the account may run: no
/// answer is unavailable, never allowed.
pub fn account_usage(
    read: &types::GetAccountRateLimitsResponse,
    plan: Option<String>,
) -> AccountUsage {
    let buckets = match &read.rate_limits_by_limit_id {
        Some(by_id) if !by_id.is_empty() => {
            let mut buckets: Vec<UsageBucket> = by_id
                .iter()
                .map(|(limit_id, snapshot)| UsageBucket {
                    limit_id: limit_id.clone(),
                    ..usage_bucket(snapshot)
                })
                .collect();
            buckets.sort_by(|a, b| a.limit_id.cmp(&b.limit_id));
            buckets
        }
        _ => vec![usage_bucket(&read.rate_limits)],
    };
    let allowance = match read.ordinary_usage_allowed {
        Some(true) => Allowance::Allowed,
        Some(false) => Allowance::Blocked,
        None => Allowance::Unavailable {
            reason: "Codex did not say whether the account may use its included quota".to_owned(),
        },
    };
    AccountUsage {
        provider: Provider::Codex,
        account: read.account_id.clone(),
        plan: plan.or_else(|| read.rate_limits.plan_type.as_ref().map(ToString::to_string)),
        allowance,
        buckets,
        read_at: Timestamp::now(),
    }
}

/// One bucket of a read or a rolling update. A bucket without an id is Codex's default, `codex`.
pub fn usage_bucket(snapshot: &types::RateLimitSnapshot) -> UsageBucket {
    UsageBucket {
        limit_id: snapshot
            .limit_id
            .clone()
            .unwrap_or_else(|| "codex".to_owned()),
        name: snapshot.limit_name.clone(),
        primary: snapshot.primary.as_ref().map(usage_window),
        secondary: snapshot.secondary.as_ref().map(usage_window),
        reached: snapshot
            .rate_limit_reached_type
            .as_ref()
            .map(ToString::to_string),
    }
}

fn usage_window(window: &types::RateLimitWindow) -> UsageWindow {
    UsageWindow {
        used_percent: window.used_percent,
        window_minutes: window.window_duration_mins,
        resets_at: window
            .resets_at
            .and_then(|seconds| u64::try_from(seconds).ok())
            .map(|seconds| Timestamp::at(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))),
    }
}

/// How a `command/exec` ended. Measured on Codex 0.159.1: a command that runs out of time ends with
/// exit code 124 on Linux, as `timeout` reports it, and on Windows fails the request with a
/// message that says it timed out.
pub fn command_outcome(result: Result<types::CommandExecResponse, RequestError>) -> CommandResult {
    match result {
        Ok(done) if cfg!(unix) && done.exit_code == 124 => CommandResult::TimedOut {
            output: format!("{}{}", done.stdout, done.stderr),
        },
        Ok(done) => CommandResult::Exited {
            exit_code: done.exit_code,
            stdout: done.stdout,
            stderr: done.stderr,
        },
        Err(RequestError::Rpc { message, .. }) if message.contains("timed out") => {
            CommandResult::TimedOut {
                output: String::new(),
            }
        }
        Err(error) => CommandResult::Refused {
            error: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn item(value: Value) -> types::ThreadItem {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn each_refusal_is_the_answer_codex_has_always_been_sent() {
        // Catches a refusal that changed shape, such as an MCP form refusal without its explicit
        // nulls, or a permissions refusal that grants what was asked.
        let params = |extra: Value| {
            let mut params =
                json!({ "threadId": "t", "turnId": "u", "itemId": "i", "startedAtMs": 0 });
            params
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            params
        };
        let cases = [
            (
                ServerRequest::CommandApproval(required(params(
                    json!({ "command": "ls", "cwd": "/w" }),
                ))),
                json!({ "decision": "cancel" }),
            ),
            (
                ServerRequest::FileChangeApproval(required(params(json!({})))),
                json!({ "decision": "cancel" }),
            ),
            (
                ServerRequest::Permissions(required(params(
                    json!({ "cwd": "/w", "permissions": { "network": { "enabled": true } } }),
                ))),
                json!({ "permissions": {}, "scope": "turn" }),
            ),
            (
                ServerRequest::UserInput(required(params(
                    json!({ "questions": [], "isBlocking": true }),
                ))),
                json!({ "answers": {} }),
            ),
            (
                ServerRequest::McpElicitation(required(json!({ "threadId": "t", "turnId": "u",
                    "serverName": "docs", "mode": "form", "message": "Sign in",
                    "requestedSchema": { "type": "object", "properties": {} } }))),
                json!({ "action": "cancel", "content": null, "_meta": null }),
            ),
        ];
        for (request, expected) in cases {
            let refused = serde_json::to_value(refusal(&request)).unwrap();
            assert_eq!(refused, expected, "{}", request.method());
        }
    }

    #[test]
    fn a_command_that_ran_out_of_time_is_told_apart_from_one_codex_refused() {
        // Catches the Windows timeout, which Codex reports as a failed request, being shown as a
        // refusal, and any other failed request being shown as a timeout.
        let rpc = |message: &str| RequestError::Rpc {
            code: -32000,
            message: message.to_owned(),
        };
        let exited: types::CommandExecResponse =
            required(json!({ "exitCode": 0, "stdout": "ok", "stderr": "" }));

        assert_eq!(
            command_outcome(Err(rpc("command timed out after 5000 ms"))),
            CommandResult::TimedOut {
                output: String::new()
            }
        );
        assert_eq!(
            command_outcome(Err(rpc("sandbox denied"))),
            CommandResult::Refused {
                error: "sandbox denied".to_owned()
            }
        );
        assert_eq!(
            command_outcome(Ok(exited)),
            CommandResult::Exited {
                exit_code: 0,
                stdout: "ok".to_owned(),
                stderr: String::new()
            }
        );
    }

    #[test]
    fn unknown_items_keep_their_codex_type_and_long_output_is_capped() {
        // Catches an item kind Kratos does not describe being dropped from history, and command
        // output stored whole (L3).
        let search = item(json!({ "type": "webSearch", "id": "w-1", "query": "rust" }));
        let noisy = item(
            json!({ "type": "commandExecution", "id": "c", "command": "yes",
            "commandActions": [], "cwd": "/w", "status": "completed", "exitCode": 0,
            "aggregatedOutput": "y\n".repeat(OUTPUT_BYTES) }),
        );

        assert_eq!(
            agent_item(&search),
            Some(AgentItem::Other {
                id: "w-1".into(),
                provider_type: "webSearch".into()
            })
        );
        let Some(AgentItem::Command { output, .. }) = agent_item(&noisy) else {
            panic!("a command item");
        };
        assert_eq!(output.text.len(), OUTPUT_BYTES);
        assert_eq!(output.omitted_bytes, OUTPUT_BYTES as u64);
    }
}
