//! MCP execution and fail-closed tool classification.

use kratos_protocol::{
    AgentItem, ApprovalDecision, ApprovalRequest, AskReason, ExtraAccess, ItemStatus,
    NetworkAccess, Permission, ThreadEventBody, ThreadSettings,
};
use serde_json::{Value, json};

use crate::harness::{A, Harness, Launch, Options, WAIT};
use crate::requests::{approval, response, send};

async fn mcp(harness: &Harness, launch: &Launch, id: &str, message: Value) {
    harness.call(launch, "test/ask", json!({ "requestId": id, "request": { "subtype": "mcp_message", "server_name": "kratos", "message": message } })).await;
}

async fn approval_before_answer(
    harness: &Harness,
    launch: &Launch,
    id: &str,
) -> kratos_protocol::Approval {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(approval) = harness.details().await.approvals.into_iter().next() {
            return approval;
        }
        let frames = harness.received(launch).await;
        assert!(
            !frames
                .iter()
                .any(|frame| frame["response"]["request_id"] == id),
            "{id}: a destructive command was answered before approval: {frames:?}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "{id}: no command approval arrived"
        );
        tokio::task::yield_now().await;
    }
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_quoting_cannot_hide_a_forced_delete_from_either_command_entry_point() {
    // Catches a PowerShell wrapper forcing the destructive-command check into its weaker POSIX fallback.
    for permission in [
        Permission::ApproveForMe,
        Permission::FullAccess,
        Permission::AskMe,
    ] {
        let harness = Harness::start(Options {
            tools: true,
            settings: ThreadSettings {
                permission,
                ..ThreadSettings::default()
            },
            ..Options::default()
        })
        .await;
        let launch = harness.begin(A, 1).await;
        let target = harness.folder().join("src");
        std::fs::create_dir(&target).unwrap();
        for (index, command) in [
            r"Write-Output `'; Remove-Item -Recurse -Force .\src",
            r"<#'#> Remove-Item -Recurse -Force .\src",
            r"Remove-Item -Recurse -Force .\src",
        ]
        .into_iter()
        .enumerate()
        {
            let callback = format!("callback-{index}");
            harness.call(&launch, "test/ask", json!({ "requestId": callback, "request": {
                "subtype": "can_use_tool", "tool_name": "mcp__kratos__run_command", "tool_use_id": callback,
                "mcp_server": { "source": "sdk", "name": "kratos" }, "input": { "commandLine": command }
            }})).await;
            let asked = approval_before_answer(&harness, &launch, &callback).await;
            assert_eq!(asked.why, AskReason::Dangerous, "{permission:?}: {command}");
            harness
                .provider
                .decide(asked.id, ApprovalDecision::Decline)
                .await
                .unwrap();
            assert_eq!(
                response(&harness, &launch, &callback).await["behavior"],
                "deny"
            );
            let direct = format!("direct-{index}");
            mcp(&harness, &launch, &direct, json!({ "jsonrpc": "2.0", "id": index, "method": "tools/call", "params": { "name": "run_command", "arguments": { "commandLine": command } } })).await;
            let asked = approval_before_answer(&harness, &launch, &direct).await;
            assert_eq!(asked.why, AskReason::Dangerous, "{permission:?}: {command}");
            harness
                .provider
                .decide(asked.id, ApprovalDecision::Decline)
                .await
                .unwrap();
            assert!(
                response(&harness, &launch, &direct).await["mcp_response"]["error"].is_object()
            );
            assert!(target.is_dir());
        }
        assert!(
            !harness.root.path().join("codex-control").exists(),
            "a declined script reached execution"
        );
    }
}

#[tokio::test]
async fn sdk_server_metadata_and_command_output_reach_claude_through_the_shared_runner() {
    // Catches wrong MCP envelopes, a command outside the thread's sandbox, output lost before
    // Claude receives it, and a command missing from the thread's history or shown twice.
    let mut harness = Harness::start(Options {
        tools: true,
        settings: ThreadSettings {
            permission: Permission::ApproveForMe,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    let initialize = &harness.received(&launch).await[0]["request"];
    assert_eq!(initialize["sdkMcpServers"], json!(["kratos"]));
    assert!(initialize["hooks"]["PostToolUse"].is_array());
    assert!(
        launch
            .arguments
            .windows(2)
            .any(|args| args == ["--disallowedTools", "Bash,PowerShell,Monitor"])
    );
    mcp(&harness, &launch, "initialize", json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2025-11-25" } })).await;
    assert_eq!(
        response(&harness, &launch, "initialize").await["mcp_response"]["result"]["serverInfo"]["name"],
        "kratos"
    );
    mcp(
        &harness,
        &launch,
        "initialized",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert_eq!(
        response(&harness, &launch, "initialized").await,
        json!({ "mcp_response": { "jsonrpc": "2.0", "id": 0, "result": {} } })
    );
    mcp(
        &harness,
        &launch,
        "list",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    assert_eq!(
        response(&harness, &launch, "list").await["mcp_response"]["result"]["tools"][0]["name"],
        "run_command"
    );
    mcp(&harness, &launch, "run", json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "run_command", "arguments": { "commandLine": "echo KRATOS_COMMAND", "timeoutMs": 12345 } } })).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    let request = loop {
        let requests = harness
            .command_control("test/turnRequests", json!({}))
            .await;
        if let Some(request) = requests
            .as_array()
            .unwrap()
            .iter()
            .find(|request| request["method"] == "command/exec")
        {
            break request.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "command never reached Codex"
        );
        tokio::task::yield_now().await;
    };
    assert_eq!(request["params"]["cwd"], json!(harness.folder()));
    assert!(
        std::path::Path::new(request["params"]["command"][0].as_str().unwrap()).is_absolute(),
        "the workspace must never choose the command shell: {request}"
    );
    assert_eq!(request["params"]["permissionProfile"], "kratos-approve");
    assert_eq!(request["params"]["timeoutMs"], 12345);
    harness.command_control("test/settleTurn", json!({ "method": "command/exec", "result": { "exitCode": 0, "stdout": "KRATOS_COMMAND", "stderr": "" } })).await;
    let result = response(&harness, &launch, "run").await;
    assert_eq!(result["mcp_response"]["id"], 3);
    assert_eq!(result["mcp_response"]["result"]["isError"], false);
    assert!(result.to_string().contains("KRATOS_COMMAND"));

    let recorded = harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::ItemCompleted {
                    item: AgentItem::Command { .. },
                    ..
                }
            )
        })
        .await;
    let ThreadEventBody::ItemCompleted {
        item:
            AgentItem::Command {
                command,
                cwd,
                status,
                exit_code,
                output,
                ..
            },
        ..
    } = recorded
    else {
        unreachable!()
    };
    assert_eq!(command, "echo KRATOS_COMMAND");
    assert_eq!(cwd.as_deref(), Some(harness.folder().to_str().unwrap()));
    assert_eq!((status, exit_code), (ItemStatus::Completed, Some(0)));
    assert!(output.text.contains("KRATOS_COMMAND"), "{output:?}");
    harness
        .emit(
            &launch,
            json!({ "type": "assistant", "uuid": "u1", "session_id": "s",
                    "message": { "id": "msg_1", "content": [
                        { "type": "tool_use", "id": "toolu_1", "name": "mcp__kratos__run_command",
                          "input": { "commandLine": "echo KRATOS_COMMAND" } },
                        { "type": "text", "text": "done" } ] } }),
        )
        .await;
    harness
        .wait_for(|event| {
            matches!(
                event,
                ThreadEventBody::ItemCompleted {
                    item: AgentItem::Message { .. },
                    ..
                }
            )
        })
        .await;
    assert!(
        !harness.log().await.iter().any(|event| matches!(event,
            ThreadEventBody::ItemCompleted { item: AgentItem::Other { provider_type, .. }, .. }
                if provider_type.contains("run_command"))),
        "the command is recorded once, as a command"
    );
}

#[tokio::test]
async fn declining_a_flagged_command_preserves_its_target_under_every_permission() {
    // Catches a direct MCP tool call bypassing the dangerous-command check under any permission.
    for permission in Permission::ALL {
        let mut harness = Harness::start(Options {
            tools: true,
            settings: ThreadSettings {
                permission,
                ..ThreadSettings::default()
            },
            ..Options::default()
        })
        .await;
        let target = harness.folder().join("keep");
        std::fs::create_dir(&target).unwrap();
        let launch = harness.begin(A, 1).await;
        let command = if cfg!(windows) {
            "Remove-Item -Recurse -Force keep"
        } else {
            "rm -rf keep"
        };
        mcp(&harness, &launch, "danger", json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "run_command", "arguments": { "commandLine": command } } })).await;
        let asked = approval(&mut harness).await;
        assert_eq!(asked.why, AskReason::Dangerous);
        assert!(serde_json::to_string(&asked).unwrap().contains(command));
        harness
            .provider
            .decide(asked.id, ApprovalDecision::Decline)
            .await
            .unwrap();
        assert!(response(&harness, &launch, "danger").await["mcp_response"]["error"].is_object());
        assert!(target.is_dir());
        assert!(
            !harness.root.path().join("codex-control").exists(),
            "declined command started Codex"
        );
    }
}

#[tokio::test]
async fn unknown_tools_bad_inputs_and_spoofed_sdk_servers_never_gain_automatic_permission() {
    // Catches name-only SDK trust, unknown tools allowed by Full access, and unchecked or disabled command inputs.
    let mut harness = Harness::start(Options {
        tools: true,
        settings: ThreadSettings {
            permission: Permission::FullAccess,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    for (id, tool, input) in [
        ("unknown", "NewUntrustedTool", json!({})),
        ("bad-write", "Write", json!({ "file_path": 42 })),
        (
            "bad-question",
            "AskUserQuestion",
            json!({ "questions": "wrong" }),
        ),
        ("shell", "Bash", json!({ "command": "echo unsafe" })),
    ] {
        send(&harness, &launch, id, tool, input).await;
        assert_eq!(
            response(&harness, &launch, id).await["behavior"],
            "deny",
            "{id}"
        );
    }
    for (id, source, name) in [("spoofed", "project", "kratos"), ("other", "sdk", "other")] {
        harness.call(&launch, "test/ask", json!({ "requestId": id, "request": { "subtype": "can_use_tool", "tool_name": "mcp__kratos__run_command", "input": { "commandLine": "echo safe" }, "tool_use_id": id, "mcp_server": { "name": name, "source": source } } })).await;
        let asked = approval(&mut harness).await;
        assert_eq!(asked.why, AskReason::Unchecked);
        harness
            .provider
            .decide(asked.id, ApprovalDecision::Decline)
            .await
            .unwrap();
    }
    mcp(&harness, &launch, "bad-command", json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": { "name": "run_command", "arguments": { "commandLine": "echo ok", "timeoutMs": 0 } } })).await;
    assert_eq!(
        response(&harness, &launch, "bad-command").await["mcp_response"]["error"]["code"],
        -32602
    );
}

#[tokio::test]
async fn file_writes_follow_d84_and_d93_and_every_protected_folder_always_asks() {
    // Catches outside writes incorrectly asking under automatic permissions or protected paths being allowed.
    for permission in Permission::ALL {
        for plan_mode in [false, true] {
            let mut harness = Harness::start(Options {
                tools: true,
                settings: ThreadSettings {
                    permission,
                    plan_mode,
                    ..ThreadSettings::default()
                },
                ..Options::default()
            })
            .await;
            let launch = harness.begin(A, 1).await;
            for (index, path) in [
                harness.folder().join("inside.txt"),
                harness.root.path().join("outside.txt"),
            ]
            .into_iter()
            .enumerate()
            {
                let id = format!("file-{index}");
                send(
                    &harness,
                    &launch,
                    &id,
                    "Write",
                    json!({ "file_path": path }),
                )
                .await;
                if permission == Permission::AskMe || plan_mode {
                    let asked = approval(&mut harness).await;
                    assert_eq!(asked.why, AskReason::AskEveryAction);
                    harness
                        .provider
                        .decide(asked.id, ApprovalDecision::Accept)
                        .await
                        .unwrap();
                }
                assert_eq!(response(&harness, &launch, &id).await["behavior"], "allow");
            }
            for folder in [
                "home/.claude",
                "home/.codex",
                "home/.ssh",
                "home/.gnupg",
                "home/.aws",
                "home/.config/gh",
                "kratos",
            ] {
                let id = format!("protected-{folder}");
                send(
                    &harness,
                    &launch,
                    &id,
                    "Write",
                    json!({ "file_path": harness.root.path().join(folder).join("test.txt") }),
                )
                .await;
                let asked = approval(&mut harness).await;
                assert!(
                    matches!(asked.why, AskReason::ProtectedFolder { .. }),
                    "{folder} under {permission:?}, plan {plan_mode}"
                );
                harness
                    .provider
                    .decide(asked.id, ApprovalDecision::Decline)
                    .await
                    .unwrap();
                assert_eq!(response(&harness, &launch, &id).await["behavior"], "deny");
            }
        }
    }
}

#[tokio::test]
async fn an_approved_command_callback_grants_one_exact_execution_and_never_a_changed_command() {
    // Catches duplicate prompts for one action, reusable command grants, and substitution after approval.
    let mut harness = Harness::start(Options {
        tools: true,
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    harness.call(&launch, "test/ask", json!({ "requestId": "permission", "request": {
        "subtype": "can_use_tool", "tool_name": "mcp__kratos__run_command", "tool_use_id": "tool-command",
        "mcp_server": { "source": "sdk", "name": "kratos" }, "input": { "commandLine": "echo approved" }
    }})).await;
    let asked = approval(&mut harness).await;
    harness
        .provider
        .decide(asked.id, ApprovalDecision::Accept)
        .await
        .unwrap();
    assert_eq!(
        response(&harness, &launch, "permission").await["behavior"],
        "allow"
    );
    mcp(&harness, &launch, "substituted", json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": { "name": "run_command", "arguments": { "commandLine": "echo substituted" } } })).await;
    let changed = approval(&mut harness).await;
    harness
        .provider
        .decide(changed.id, ApprovalDecision::Decline)
        .await
        .unwrap();
    assert!(response(&harness, &launch, "substituted").await["mcp_response"]["error"].is_object());
    let call = json!({ "jsonrpc": "2.0", "id": 10, "method": "tools/call", "params": { "name": "run_command", "arguments": { "commandLine": "echo approved" } } });
    mcp(&harness, &launch, "approved", call.clone()).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let requests = harness
            .command_control("test/turnRequests", json!({}))
            .await;
        if requests
            .as_array()
            .unwrap()
            .iter()
            .any(|request| request["method"] == "command/exec")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "approved command never reached the runner"
        );
        tokio::task::yield_now().await;
    }
    assert!(
        harness.details().await.approvals.is_empty(),
        "one command asked twice"
    );
    harness.command_control("test/settleTurn", json!({ "method": "command/exec", "result": { "exitCode": 0, "stdout": "approved", "stderr": "" } })).await;
    assert_eq!(
        response(&harness, &launch, "approved").await["mcp_response"]["result"]["isError"],
        false
    );
    mcp(&harness, &launch, "again", call).await;
    let repeated = approval(&mut harness).await;
    harness
        .provider
        .decide(repeated.id, ApprovalDecision::Decline)
        .await
        .unwrap();
    assert!(response(&harness, &launch, "again").await["mcp_response"]["error"].is_object());
}

#[tokio::test]
async fn commands_are_terminated_when_the_cli_withdraws_them_stops_or_exits() {
    // Catches command processes outliving the Claude turn that owned their permission grant.
    for ending in ["withdraw", "stop", "exit", "result"] {
        let harness = Harness::start(Options {
            tools: true,
            settings: ThreadSettings {
                permission: Permission::ApproveForMe,
                ..ThreadSettings::default()
            },
            ..Options::default()
        })
        .await;
        let launch = harness.begin(A, 1).await;
        mcp(&harness, &launch, "running-command", json!({ "jsonrpc": "2.0", "id": 20, "method": "tools/call", "params": { "name": "run_command", "arguments": { "commandLine": "echo waiting" } } })).await;
        let deadline = tokio::time::Instant::now() + WAIT;
        let process_id = loop {
            let requests = harness
                .command_control("test/turnRequests", json!({}))
                .await;
            if let Some(request) = requests
                .as_array()
                .unwrap()
                .iter()
                .find(|request| request["method"] == "command/exec")
            {
                break request["params"]["processId"].clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "command never started"
            );
            tokio::task::yield_now().await;
        };
        match ending {
            "withdraw" => harness
                .emit(
                    &launch,
                    json!({ "type": "control_cancel_request", "request_id": "running-command" }),
                )
                .await,
            "stop" => harness
                .provider
                .stop(harness.thread_id.clone())
                .await
                .unwrap(),
            "exit" => harness.exit(&launch, 3).await,
            "result" => harness.finish(&launch, &[A], json!({})).await,
            _ => unreachable!(),
        }
        loop {
            let requests = harness
                .command_control("test/turnRequests", json!({}))
                .await;
            if let Some(request) = requests
                .as_array()
                .unwrap()
                .iter()
                .find(|request| request["method"] == "command/exec/terminate")
            {
                assert_eq!(request["params"]["processId"], process_id);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{ending} left the command running"
            );
            tokio::task::yield_now().await;
        }
    }
}

#[tokio::test]
async fn claude_commands_select_the_same_profiles_as_codex_for_every_permission_and_plan_mode() {
    // Catches a Claude permission selecting a different sandbox or making the whole filesystem writable.
    for (permission, plan_mode, profile) in [
        (Permission::ApproveForMe, false, "kratos-approve"),
        (Permission::AskMe, false, "kratos-ask"),
        (Permission::FullAccess, false, ":danger-full-access"),
        (Permission::ApproveForMe, true, "kratos-plan"),
        (Permission::AskMe, true, "kratos-plan"),
        (Permission::FullAccess, true, "kratos-plan"),
    ] {
        let harness = Harness::start(Options {
            tools: true,
            settings: ThreadSettings {
                permission,
                plan_mode,
                ..ThreadSettings::default()
            },
            ..Options::default()
        })
        .await;
        let launch = harness.begin(A, 1).await;
        mcp(&harness, &launch, "profile", json!({ "jsonrpc": "2.0", "id": 30, "method": "tools/call", "params": { "name": "run_command", "arguments": { "commandLine": "echo shared-profile" } } })).await;
        if permission == Permission::AskMe || plan_mode {
            let asked = approval_before_answer(&harness, &launch, "profile").await;
            harness
                .provider
                .decide(asked.id, ApprovalDecision::Accept)
                .await
                .unwrap();
        }
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let requests = harness
                .command_control("test/turnRequests", json!({}))
                .await;
            if let Some(request) = requests
                .as_array()
                .unwrap()
                .iter()
                .find(|request| request["method"] == "command/exec")
            {
                assert_eq!(
                    request["params"]["permissionProfile"], profile,
                    "{permission:?}, plan {plan_mode}"
                );
                assert_eq!(request["params"]["cwd"], json!(harness.folder()));
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "command never reached its sandbox"
            );
            tokio::task::yield_now().await;
        }
        harness.command_control("test/settleTurn", json!({ "method": "command/exec", "result": { "exitCode": 0, "stdout": "shared-profile", "stderr": "" } })).await;
        assert_eq!(
            response(&harness, &launch, "profile").await["mcp_response"]["result"]["isError"],
            false
        );
    }
}

#[tokio::test]
async fn native_file_writes_match_the_workspace_and_temp_scope_of_sandboxed_commands() {
    // Catches automatic native writes beyond workspace and temp while preserving Full access and temp writes.
    let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let outside = tempfile::Builder::new()
        .prefix("claude-outside-")
        .tempdir_in(parent)
        .unwrap();
    assert!(
        !std::fs::canonicalize(outside.path())
            .unwrap()
            .starts_with(std::fs::canonicalize(std::env::temp_dir()).unwrap()),
        "this fixture must be outside the system temp grant"
    );
    for permission in [
        Permission::ApproveForMe,
        Permission::AskMe,
        Permission::FullAccess,
    ] {
        let harness = Harness::start(Options {
            tools: true,
            settings: ThreadSettings {
                permission,
                ..ThreadSettings::default()
            },
            ..Options::default()
        })
        .await;
        let launch = harness.begin(A, 1).await;
        for (id, path) in [
            ("outside", outside.path().join("write.txt")),
            ("workspace", harness.folder().join("write.txt")),
            ("temp", harness.root.path().join("write.txt")),
        ] {
            send(&harness, &launch, id, "Write", json!({ "file_path": path })).await;
            if permission == Permission::AskMe
                || (permission == Permission::ApproveForMe && id == "outside")
            {
                let asked = approval_before_answer(&harness, &launch, id).await;
                harness
                    .provider
                    .decide(asked.id, ApprovalDecision::Decline)
                    .await
                    .unwrap();
                assert_eq!(response(&harness, &launch, id).await["behavior"], "deny");
            } else {
                assert_eq!(response(&harness, &launch, id).await["behavior"], "allow");
            }
        }
    }
}

#[tokio::test]
async fn a_command_approval_shows_the_command_folder_and_escape_access_on_both_entry_points() {
    // Catches the real CLI's permission callback reaching the client as a generic Permissions request
    // that hides the command and the extra access an escape asks for.
    for (permission, escape) in [(Permission::ApproveForMe, true), (Permission::AskMe, false)] {
        for callback in [true, false] {
            let mut harness = Harness::start(Options {
                tools: true,
                settings: ThreadSettings {
                    permission,
                    ..ThreadSettings::default()
                },
                ..Options::default()
            })
            .await;
            let launch = harness.begin(A, 1).await;
            let input = json!({ "commandLine": "echo shown", "outsideSandbox": escape });
            if callback {
                harness.call(&launch, "test/ask", json!({ "requestId": "shown", "request": {
                    "subtype": "can_use_tool", "tool_name": "mcp__kratos__run_command", "tool_use_id": "shown",
                    "mcp_server": { "source": "sdk", "name": "kratos" }, "input": input
                }})).await;
            } else {
                mcp(&harness, &launch, "shown", json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "run_command", "arguments": input } })).await;
            }
            let asked = approval(&mut harness).await;
            let label = format!("{permission:?}, escape {escape}, callback {callback}");
            let ApprovalRequest::Command {
                command,
                cwd,
                reason,
                access,
            } = &asked.request
            else {
                panic!(
                    "{label}: a command approval must show the command, not {:?}",
                    asked.request
                );
            };
            assert_eq!(command, "echo shown", "{label}");
            assert_eq!(cwd.as_deref(), harness.folder().to_str(), "{label}");
            assert_eq!(reason.is_some(), escape, "{label}");
            if escape {
                assert_eq!(access.network, Some(NetworkAccess::Any), "{label}");
                assert_eq!(access.paths.len(), 1, "{label}");
                assert_eq!(access.paths[0].path, ":root", "{label}");
            } else {
                assert_eq!(access, &ExtraAccess::default(), "{label}");
            }
            harness
                .provider
                .decide(asked.id, ApprovalDecision::Decline)
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn a_server_borrowing_the_kratos_name_is_an_unchecked_tool_and_earns_no_command_grant() {
    // Catches a spoofed server being shown as Kratos's command or leaving a grant that skips the dangerous-command ask.
    let mut harness = Harness::start(Options {
        tools: true,
        settings: ThreadSettings {
            permission: Permission::ApproveForMe,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    let command = if cfg!(windows) {
        "Remove-Item -Recurse -Force keep"
    } else {
        "rm -rf keep"
    };
    for (id, server) in [
        ("stdio", json!({ "source": "stdio", "name": "kratos" })),
        ("renamed", json!({ "source": "sdk", "name": "imposter" })),
    ] {
        harness.call(&launch, "test/ask", json!({ "requestId": id, "request": {
            "subtype": "can_use_tool", "tool_name": "mcp__kratos__run_command", "tool_use_id": id,
            "mcp_server": server, "input": { "commandLine": command }
        }})).await;
        let asked = approval(&mut harness).await;
        assert_eq!(asked.why, AskReason::Unchecked, "{id}");
        assert!(
            matches!(asked.request, ApprovalRequest::Permissions { .. }),
            "{id}: {:?}",
            asked.request
        );
        harness
            .provider
            .decide(asked.id, ApprovalDecision::Accept)
            .await
            .unwrap();
        assert_eq!(response(&harness, &launch, id).await["behavior"], "allow");
        mcp(&harness, &launch, &format!("{id}-call"), json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": { "name": "run_command", "arguments": { "commandLine": command } } })).await;
        let again = approval(&mut harness).await;
        assert_eq!(
            again.why,
            AskReason::Dangerous,
            "{id}: an approved impostor must not grant the command"
        );
        harness
            .provider
            .decide(again.id, ApprovalDecision::Decline)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_write_path_is_judged_where_it_resolves_not_by_how_it_is_spelled() {
    // Catches a `..` climb out of the thread's folder, or a different letter case, being judged by its text.
    let harness = Harness::start(Options {
        tools: true,
        settings: ThreadSettings {
            permission: Permission::ApproveForMe,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    let folder = harness.folder();
    std::fs::create_dir(folder.join("sub")).unwrap();
    let temp = std::env::temp_dir();
    let beyond_temp = folder
        .join("..")
        .join("..")
        .join("..")
        .join("..")
        .join("climbed.txt");
    let mut cases = vec![
        ("climb-out", beyond_temp, true),
        (
            "climb-within",
            folder.join("sub").join("..").join("kept.txt"),
            false,
        ),
        ("temp-climb-out", temp.join("..").join("climbed.txt"), true),
    ];
    if cfg!(windows) {
        cases.push((
            "other-case",
            folder
                .join("case.txt")
                .to_string_lossy()
                .to_uppercase()
                .into(),
            false,
        ));
    }
    for (id, path, asks) in cases {
        send(&harness, &launch, id, "Write", json!({ "file_path": path })).await;
        if asks {
            let asked = approval_before_answer(&harness, &launch, id).await;
            assert_eq!(asked.why, AskReason::Escalation, "{id}");
            harness
                .provider
                .decide(asked.id, ApprovalDecision::Decline)
                .await
                .unwrap();
            assert_eq!(
                response(&harness, &launch, id).await["behavior"],
                "deny",
                "{id}"
            );
        } else {
            assert_eq!(
                response(&harness, &launch, id).await["behavior"],
                "allow",
                "{id}: {path:?}"
            );
        }
    }
}

fn drive_forms(path: &std::path::Path) -> Vec<String> {
    let text = path
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let Some((drive, rest)) = text.split_once(":/") else {
        return Vec::new();
    };
    let drive = drive.to_lowercase();
    vec![
        format!("/{drive}/{rest}"),
        format!("/mnt/{drive}/{rest}"),
        format!("/cygdrive/{drive}/{rest}"),
    ]
}

#[tokio::test]
async fn paths_claude_expands_are_judged_as_the_files_they_name() {
    // Catches `~` and `/c/...` being joined to the workspace as plain folder names, so a write to the
    // user's Claude settings, where a planted hook runs outside every sandbox, looks like a workspace file.
    for permission in [
        Permission::ApproveForMe,
        Permission::FullAccess,
        Permission::AskMe,
    ] {
        let harness = Harness::start(Options {
            tools: true,
            settings: ThreadSettings {
                permission,
                ..ThreadSettings::default()
            },
            ..Options::default()
        })
        .await;
        let launch = harness.begin(A, 1).await;
        let home = harness.root.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let settings = home.join(".claude").join("settings.json");
        let mut forms = vec![
            "~/.claude/settings.json".to_owned(),
            r"~\.claude\settings.json".to_owned(),
            "  ~/.claude/settings.json ".to_owned(),
        ];
        if cfg!(windows) {
            forms.extend(drive_forms(&settings));
        }
        for (index, form) in forms.iter().enumerate() {
            let id = format!("write-{index}");
            send(
                &harness,
                &launch,
                &id,
                "Write",
                json!({ "file_path": form }),
            )
            .await;
            let asked = approval_before_answer(&harness, &launch, &id).await;
            assert!(
                matches!(asked.why, AskReason::ProtectedFolder { .. }),
                "{permission:?} {form:?}: {:?}",
                asked.why
            );
            harness
                .provider
                .decide(asked.id, ApprovalDecision::Decline)
                .await
                .unwrap();
            assert_eq!(
                response(&harness, &launch, &id).await["behavior"],
                "deny",
                "{permission:?} {form:?}"
            );
        }
        if permission == Permission::FullAccess {
            continue;
        }
        for (index, form) in ["~/.claude/settings.json", "~/../kratos/client-token"]
            .into_iter()
            .enumerate()
        {
            let id = format!("read-{index}");
            send(&harness, &launch, &id, "Read", json!({ "file_path": form })).await;
            assert_eq!(
                response(&harness, &launch, &id).await["behavior"],
                "deny",
                "{permission:?} {form:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_tilde_form_claude_may_expand_to_another_user_is_refused() {
    // Catches `~name/...` being judged as a workspace folder when the CLI may resolve it elsewhere.
    let harness = Harness::start(Options {
        tools: true,
        settings: ThreadSettings {
            permission: Permission::ApproveForMe,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    for (id, tool, input) in [
        (
            "write",
            "Write",
            json!({ "file_path": "~other/.ssh/authorized_keys" }),
        ),
        ("read", "Read", json!({ "file_path": "~other/.ssh/id_rsa" })),
    ] {
        send(&harness, &launch, id, tool, input).await;
        assert_eq!(
            response(&harness, &launch, id).await["behavior"],
            "deny",
            "{id}"
        );
    }
}
