use kratos_protocol::{
    ApprovalDecision, ApprovalRequest, AskReason, ExtraAccess, Permission, ThreadSettings,
};
use serde_json::json;

use crate::harness::{A, Harness, Launch, Options, WAIT};
use crate::requests::{approval, response, send};

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

#[tokio::test]
async fn powershell_quoting_cannot_hide_a_forced_delete_from_the_check() {
    for permission in Permission::ALL {
        let harness = Harness::start(Options {
            settings: ThreadSettings {
                permission,
                ..ThreadSettings::default()
            },
            ..Options::default()
        })
        .await;
        let launch = harness.begin(A, 1).await;
        for (index, command) in [
            r"Write-Output `'; Remove-Item -Recurse -Force .\src",
            r"<#'#> Remove-Item -Recurse -Force .\src",
            r"Remove-Item -Recurse -Force .\src",
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("powershell-{index}");
            send(
                &harness,
                &launch,
                &id,
                "PowerShell",
                json!({ "command": command }),
            )
            .await;
            let asked = approval_before_answer(&harness, &launch, &id).await;
            assert_eq!(asked.why, AskReason::Dangerous, "{permission:?}: {command}");
            harness
                .provider
                .decide(asked.id, ApprovalDecision::Decline)
                .await
                .unwrap();
            assert_eq!(response(&harness, &launch, &id).await["behavior"], "deny");
        }
    }
}

#[tokio::test]
async fn a_shell_command_runs_unasked_only_when_unflagged_and_the_permission_allows_it() {
    for permission in Permission::ALL {
        for (shell, ordinary, flagged) in [
            ("Bash", "ls -la", "rm -rf keep"),
            (
                "PowerShell",
                "Get-ChildItem",
                "Remove-Item -Recurse -Force keep",
            ),
        ] {
            let mut harness = Harness::start(Options {
                settings: ThreadSettings {
                    permission,
                    ..ThreadSettings::default()
                },
                ..Options::default()
            })
            .await;
            let launch = harness.begin(A, 1).await;
            let label = format!("{permission:?} {shell}");
            send(
                &harness,
                &launch,
                "ordinary",
                shell,
                json!({ "command": ordinary }),
            )
            .await;
            if permission == Permission::AskMe {
                let asked = approval(&mut harness).await;
                assert_eq!(asked.why, AskReason::AskEveryAction, "{label}");
                harness
                    .provider
                    .decide(asked.id, ApprovalDecision::Accept)
                    .await
                    .unwrap();
            }
            assert_eq!(
                response(&harness, &launch, "ordinary").await["behavior"],
                "allow",
                "{label}"
            );
            send(
                &harness,
                &launch,
                "flagged",
                shell,
                json!({ "command": flagged }),
            )
            .await;
            let asked = approval(&mut harness).await;
            assert_eq!(asked.why, AskReason::Dangerous, "{label}");
            assert!(
                serde_json::to_string(&asked).unwrap().contains(flagged),
                "{label}"
            );
            harness
                .provider
                .decide(asked.id, ApprovalDecision::Decline)
                .await
                .unwrap();
            assert_eq!(
                response(&harness, &launch, "flagged").await["behavior"],
                "deny",
                "{label}"
            );
        }
    }
}

#[tokio::test]
async fn unknown_tools_and_bad_inputs_never_gain_automatic_permission() {
    let mut harness = Harness::start(Options {
        settings: ThreadSettings {
            permission: Permission::FullAccess,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    for (id, tool, input) in [
        ("bad-write", "Write", json!({ "file_path": 42 })),
        (
            "bad-question",
            "AskUserQuestion",
            json!({ "questions": "wrong" }),
        ),
        ("empty-shell", "Bash", json!({ "command": " " })),
        ("no-shell-text", "PowerShell", json!({})),
    ] {
        send(&harness, &launch, id, tool, input).await;
        assert_eq!(
            response(&harness, &launch, id).await["behavior"],
            "deny",
            "{id}"
        );
    }
    for (id, tool) in [("unknown", "NewUntrustedTool"), ("helper", "Agent")] {
        send(&harness, &launch, id, tool, json!({})).await;
        let asked = approval(&mut harness).await;
        assert_eq!(asked.why, AskReason::Unchecked, "{id}");
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
    }
    harness.call(&launch, "test/ask", json!({ "requestId": "spoofed", "request": { "subtype": "can_use_tool", "tool_name": "mcp__kratos__run_command", "input": { "commandLine": "echo safe" }, "tool_use_id": "spoofed", "mcp_server": { "name": "kratos", "source": "sdk" } } })).await;
    let asked = approval(&mut harness).await;
    assert_eq!(asked.why, AskReason::Unchecked);
    harness
        .provider
        .decide(asked.id, ApprovalDecision::Decline)
        .await
        .unwrap();
}

#[tokio::test]
async fn file_writes_follow_d84_and_d93_and_every_protected_folder_always_asks() {
    // Catches outside writes incorrectly asking under automatic permissions or protected paths being allowed.
    for permission in Permission::ALL {
        for plan_mode in [false, true] {
            let mut harness = Harness::start(Options {
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
async fn native_file_writes_ask_outside_the_workspace_and_temp_under_approve_for_me() {
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
async fn a_shell_approval_shows_the_command_and_the_thread_folder() {
    for shell in ["Bash", "PowerShell"] {
        let mut harness = Harness::start(Options {
            settings: ThreadSettings {
                permission: Permission::AskMe,
                ..ThreadSettings::default()
            },
            ..Options::default()
        })
        .await;
        let launch = harness.begin(A, 1).await;
        send(
            &harness,
            &launch,
            "shown",
            shell,
            json!({ "command": "echo shown" }),
        )
        .await;
        let asked = approval(&mut harness).await;
        let ApprovalRequest::Command {
            command,
            cwd,
            reason,
            access,
        } = &asked.request
        else {
            panic!(
                "{shell}: a command approval must show the command, not {:?}",
                asked.request
            );
        };
        assert_eq!(command, "echo shown", "{shell}");
        assert_eq!(cwd.as_deref(), harness.folder().to_str(), "{shell}");
        assert_eq!(reason, &None, "{shell}");
        assert_eq!(access, &ExtraAccess::default(), "{shell}");
        harness
            .provider
            .decide(asked.id, ApprovalDecision::Decline)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_write_path_is_judged_where_it_resolves_not_by_how_it_is_spelled() {
    // Catches a `..` climb out of the thread's folder, or a different letter case, being judged by its text.
    let harness = Harness::start(Options {
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

#[tokio::test]
async fn an_approval_shows_a_large_tool_input_cut_and_says_so() {
    let mut harness = Harness::start(Options {
        settings: ThreadSettings {
            permission: Permission::AskMe,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    let content = "x".repeat(100 * 1024);
    send(
        &harness,
        &launch,
        "large",
        "Write",
        json!({ "file_path": "big.txt", "content": content }),
    )
    .await;
    let asked = approval(&mut harness).await;
    let ApprovalRequest::Permissions {
        reason: Some(reason),
        ..
    } = &asked.request
    else {
        panic!("a write asks with its input: {:?}", asked.request);
    };
    assert!(
        reason.starts_with("Write \"big.txt\"\n"),
        "missing edit target"
    );
    assert!(reason.len() < 17 * 1024, "{} bytes", reason.len());
    assert!(reason.contains("more bytes not shown"), "{reason}");
    harness
        .provider
        .decide(asked.id, ApprovalDecision::Decline)
        .await
        .unwrap();
}

#[tokio::test]
async fn tool_search_and_skill_run_without_asking_even_under_ask_me() {
    let harness = Harness::start(Options {
        settings: ThreadSettings {
            permission: Permission::AskMe,
            plan_mode: true,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    for (id, tool, input) in [
        (
            "search",
            "ToolSearch",
            json!({ "query": "select:ExitPlanMode" }),
        ),
        ("skill", "Skill", json!({ "skill": "review" })),
    ] {
        send(&harness, &launch, id, tool, input).await;
        assert_eq!(
            response(&harness, &launch, id).await["behavior"],
            "allow",
            "{id}"
        );
    }
    assert!(harness.details().await.approvals.is_empty());
}

#[tokio::test]
async fn plan_mode_reads_a_file_without_asking_and_a_write_still_does_not_run() {
    for permission in Permission::ALL {
        let mut harness = Harness::start(Options {
            settings: ThreadSettings {
                permission,
                plan_mode: true,
                ..ThreadSettings::default()
            },
            ..Options::default()
        })
        .await;
        let launch = harness.begin(A, 1).await;
        std::fs::write(harness.folder().join("notes.txt"), "notes").unwrap();
        for (id, tool, input) in [
            ("read", "Read", json!({ "file_path": "notes.txt" })),
            ("glob", "Glob", json!({ "pattern": "*.txt" })),
            ("grep", "Grep", json!({ "pattern": "notes" })),
        ] {
            send(&harness, &launch, id, tool, input).await;
            assert_eq!(
                response(&harness, &launch, id).await["behavior"],
                "allow",
                "{permission:?} {id}"
            );
        }
        assert!(
            harness.details().await.approvals.is_empty(),
            "{permission:?}"
        );

        send(
            &harness,
            &launch,
            "write",
            "Write",
            json!({ "file_path": "new.txt" }),
        )
        .await;
        let asked = approval(&mut harness).await;
        assert_eq!(asked.why, AskReason::AskEveryAction, "{permission:?}");
        let frames = harness.received(&launch).await;
        assert!(
            !frames
                .iter()
                .any(|frame| frame["response"]["request_id"] == "write"),
            "{permission:?}: a plan-mode write was answered before the user decided"
        );
    }
}
