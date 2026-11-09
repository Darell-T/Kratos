//! Native file reads cannot expose protected data through direct reads or recursive searches.

use kratos_protocol::{Permission, ThreadSettings};
use serde_json::json;

use crate::harness::{A, Harness, Options};
use crate::requests::{response, send};

#[tokio::test]
async fn protected_native_reads_are_denied_until_the_thread_has_unsandboxed_full_access() {
    // Catches an automatic Claude read exposing the client token or provider and credential folders.
    for (permission, plan_mode, expected) in [
        (Permission::ApproveForMe, false, "deny"),
        (Permission::AskMe, false, "deny"),
        (Permission::FullAccess, false, "allow"),
        (Permission::ApproveForMe, true, "deny"),
        (Permission::AskMe, true, "deny"),
        (Permission::FullAccess, true, "deny"),
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
        std::fs::write(
            harness.root.path().join("kratos/client-token"),
            "synthetic-client-token",
        )
        .unwrap();
        for folder in [
            "kratos",
            "home/.claude",
            "home/.codex",
            "home/.ssh",
            "home/.gnupg",
            "home/.aws",
            "home/.config/gh",
        ] {
            let path = harness.root.path().join(folder);
            for (tool, input) in [
                ("Read", json!({ "file_path": path.join("client-token") })),
                ("Grep", json!({ "pattern": ".", "path": path })),
                ("Glob", json!({ "pattern": "**/*", "path": path })),
            ] {
                let id = format!("{tool}-{folder}");
                send(&harness, &launch, &id, tool, input).await;
                assert_eq!(
                    response(&harness, &launch, &id).await["behavior"],
                    expected,
                    "{tool} in {folder}, {permission:?}, plan {plan_mode}"
                );
            }
        }
        assert!(harness.details().await.approvals.is_empty());
    }
}

#[tokio::test]
async fn recursive_and_absolute_searches_cannot_reach_protected_data_from_an_allowed_folder() {
    // Catches Grep of a parent folder and Glob patterns escaping an otherwise safe search root.
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
            "parent-grep",
            "Grep",
            json!({ "pattern": ".", "path": harness.root.path() }),
        ),
        (
            "parent-glob",
            "Glob",
            json!({ "pattern": "**/*", "path": harness.root.path() }),
        ),
        (
            "absolute-glob",
            "Glob",
            json!({ "pattern": harness.root.path().join("kratos/*") }),
        ),
        ("relative-glob", "Glob", json!({ "pattern": "../kratos/*" })),
    ] {
        send(&harness, &launch, id, tool, input).await;
        assert_eq!(
            response(&harness, &launch, id).await["behavior"],
            "deny",
            "{id}"
        );
    }
    for (id, tool, input) in [
        ("read-control", "Read", json!({ "file_path": "notes.rs" })),
        ("grep-control", "Grep", json!({ "pattern": "hello" })),
        ("glob-control", "Glob", json!({ "pattern": "**/*.rs" })),
    ] {
        send(&harness, &launch, id, tool, input).await;
        assert_eq!(
            response(&harness, &launch, id).await["behavior"],
            "allow",
            "{id}"
        );
    }
}
