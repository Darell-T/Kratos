//! What Kratos asks Codex for itself: the skills in a thread's folder, and a command run in the
//! thread's sandbox (D97, L11).

use std::time::Duration;

use kratos_protocol::{CommandResult, Permission, ThreadSettings};
use serde_json::json;

use crate::harness::{Harness, Options};

#[tokio::test]
async fn skills_are_listed_for_the_thread_s_folder() {
    // Catches skills read from the engine's folder instead of the thread's.
    let harness = Harness::start(Options::default()).await;

    let skills = harness
        .provider
        .skills(harness.thread_id.clone())
        .await
        .unwrap();

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "project");
    assert!(
        skills[0]
            .path
            .starts_with(&harness.folder().display().to_string())
    );
}

#[tokio::test]
async fn a_command_runs_in_the_thread_s_folder_under_its_permission_s_sandbox() {
    // Catches a command Kratos runs for itself escaping the sandbox the thread's permission sets.
    let mut harness = Harness::start(Options {
        settings: ThreadSettings {
            permission: Permission::ApproveForMe,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;

    let provider = harness.provider.clone();
    let thread_id = harness.thread_id.clone();
    let run = tokio::spawn(async move {
        provider
            .run_command(
                thread_id,
                vec!["git".into(), "status".into()],
                Duration::from_secs(30),
            )
            .await
    });
    let exec = &harness.held("command/exec", 1).await[0]["params"];
    assert_eq!(exec["command"], json!(["git", "status"]));
    assert_eq!(exec["cwd"], json!(harness.folder()));
    assert_eq!(exec["permissionProfile"], "kratos-approve");
    assert_eq!(exec["timeoutMs"], 30_000);
    harness
        .control(
            "test/settleTurn",
            json!({ "method": "command/exec", "result": { "exitCode": 0, "stdout": "clean", "stderr": "" } }),
        )
        .await;

    let result = run.await.unwrap().unwrap();
    assert_eq!(
        result,
        CommandResult::Exited {
            exit_code: 0,
            stdout: "clean".into(),
            stderr: String::new(),
        }
    );
}
