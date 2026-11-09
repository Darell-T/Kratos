//! Plan settings and post-tool changes through the real Claude control transport.

use kratos_protocol::{ApprovalDecision, Permission, ThreadEventBody, ThreadSettings};
use serde_json::{Value, json};

use crate::harness::{A, B, C, D, E, Harness, Launch, Options};
use crate::requests::{approval, response, send};

#[tokio::test]
async fn launches_in_native_plan_mode_denies_exit_plan_mode_asks_before_tools_and_keeps_the_setting_across_changes_and_restarts()
 {
    // Catches a planning agent disabling its own guard or a mid-turn settings change weakening a running turn.
    let mut harness = Harness::start(Options {
        tools: true,
        settings: ThreadSettings {
            permission: Permission::FullAccess,
            plan_mode: true,
            ..ThreadSettings::default()
        },
        ..Options::default()
    })
    .await;
    let first = harness.begin(A, 1).await;
    assert_eq!(first.value("--permission-mode"), Some("plan"));
    assert!(!first.has("--allow-dangerously-skip-permissions"));
    send(&harness, &first, "exit-plan", "ExitPlanMode", json!({})).await;
    assert_eq!(
        response(&harness, &first, "exit-plan").await["behavior"],
        "deny"
    );
    send(
        &harness,
        &first,
        "write-plan",
        "Write",
        json!({ "file_path": "notes.rs" }),
    )
    .await;
    let write = approval(&mut harness).await;
    harness
        .provider
        .decide(write.id, ApprovalDecision::Decline)
        .await
        .unwrap();
    assert_eq!(
        response(&harness, &first, "write-plan").await["behavior"],
        "deny"
    );
    harness
        .provider
        .set_settings(harness.thread_id.clone(), ThreadSettings::default())
        .await
        .unwrap();
    send(&harness, &first, "still-plan", "ExitPlanMode", json!({})).await;
    assert_eq!(
        response(&harness, &first, "still-plan").await["behavior"],
        "deny"
    );
    send(&harness, &first, "planning-question", "AskUserQuestion", json!({ "questions": [{ "question": "Which folder?", "header": "Folder", "options": [], "multiSelect": false }] })).await;
    let ThreadEventBody::QuestionAsked { question } = harness
        .wait_for(|event| matches!(event, ThreadEventBody::QuestionAsked { .. }))
        .await
    else {
        unreachable!()
    };
    harness
        .provider
        .answer(
            question.id,
            std::collections::BTreeMap::from([("Which folder?".into(), vec!["build".into()])]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        response(&harness, &first, "planning-question").await["behavior"],
        "allow"
    );
    harness.finish(&first, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let next = harness.begin(B, 2).await;
    assert_eq!(next.value("--permission-mode"), Some("default"));
    send(&harness, &next, "exit-default", "ExitPlanMode", json!({})).await;
    let exit = approval(&mut harness).await;
    harness
        .provider
        .decide(exit.id, ApprovalDecision::Accept)
        .await
        .unwrap();
    assert_eq!(
        response(&harness, &next, "exit-default").await["behavior"],
        "allow"
    );
    harness.finish(&next, &[B], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    harness
        .provider
        .set_settings(
            harness.thread_id.clone(),
            ThreadSettings {
                plan_mode: true,
                ..ThreadSettings::default()
            },
        )
        .await
        .unwrap();
    let (root, id) = harness.stop().await;
    let restored = Harness::start_in(
        root,
        Options {
            tools: true,
            ..Options::default()
        },
        Some(id),
    )
    .await;
    let resumed = restored.begin(C, 3).await;
    assert_eq!(resumed.value("--permission-mode"), Some("plan"));
    assert!(resumed.value("--resume").is_some());
    send(
        &restored,
        &resumed,
        "exit-restored",
        "ExitPlanMode",
        json!({}),
    )
    .await;
    assert_eq!(
        response(&restored, &resumed, "exit-restored").await["behavior"],
        "deny"
    );
}

async fn hook(
    harness: &Harness,
    launch: &Launch,
    id: &str,
    event: &str,
    name: &str,
    input: Value,
) -> Value {
    harness.call(launch, "test/ask", json!({ "requestId": id, "request": {
        "subtype": "hook_callback", "callback_id": if event == "PostToolUse" { "kratos-post" } else { "kratos-failed" },
        "input": { "hook_event_name": event, "tool_name": name, "tool_input": input }
    }})).await;
    response(harness, launch, id).await
}

#[tokio::test]
async fn reports_the_files_each_turn_edited_and_whether_one_changed_after_the_last_command() {
    // Catches failed edits counted as writes, repeated file names, and edit state leaking between turns.
    let mut harness = Harness::start(Options {
        tools: true,
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    assert_eq!(
        hook(
            &harness,
            &launch,
            "edit-a",
            "PostToolUse",
            "Edit",
            json!({ "file_path": "a.ts" })
        )
        .await,
        json!({ "continue": true })
    );
    hook(
        &harness,
        &launch,
        "command",
        "PostToolUse",
        "mcp__kratos__run_command",
        json!({ "commandLine": "npm test" }),
    )
    .await;
    hook(
        &harness,
        &launch,
        "write-b",
        "PostToolUse",
        "Write",
        json!({ "file_path": "b.ts" }),
    )
    .await;
    hook(
        &harness,
        &launch,
        "again-a",
        "PostToolUse",
        "Edit",
        json!({ "file_path": "a.ts" }),
    )
    .await;
    hook(
        &harness,
        &launch,
        "failed-c",
        "PostToolUseFailure",
        "Edit",
        json!({ "file_path": "c.ts" }),
    )
    .await;
    hook(
        &harness,
        &launch,
        "read-d",
        "PostToolUse",
        "Read",
        json!({ "file_path": "d.ts" }),
    )
    .await;
    harness.finish(&launch, &[A], json!({})).await;
    let ThreadEventBody::TurnCompleted { changes, .. } = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await
    else {
        unreachable!()
    };
    assert_eq!(changes.files, ["a.ts", "b.ts"]);
    assert!(changes.edited_since_command && changes.ran_command);
    harness.send(B, B).await;
    harness.wait_written(&launch, &[A, B]).await;
    harness.accept(&launch, B).await;
    hook(
        &harness,
        &launch,
        "notebook",
        "PostToolUse",
        "NotebookEdit",
        json!({ "notebook_path": "n.ipynb" }),
    )
    .await;
    hook(
        &harness,
        &launch,
        "multi",
        "PostToolUse",
        "MultiEdit",
        json!({ "file_path": "m.ts" }),
    )
    .await;
    hook(
        &harness,
        &launch,
        "failed-command",
        "PostToolUseFailure",
        "mcp__kratos__run_command",
        json!({ "commandLine": "npm test" }),
    )
    .await;
    harness.finish(&launch, &[B], json!({})).await;
    let ThreadEventBody::TurnCompleted { changes, .. } = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await
    else {
        unreachable!()
    };
    assert_eq!(changes.files, ["n.ipynb", "m.ts"]);
    assert!(!changes.edited_since_command);
    harness.send(C, C).await;
    harness.wait_written(&launch, &[A, B, C]).await;
    harness.accept(&launch, C).await;
    hook(
        &harness,
        &launch,
        "malformed",
        "PostToolUse",
        "Edit",
        json!({ "file_path": 42 }),
    )
    .await;
    harness.finish(&launch, &[C], json!({})).await;
    let ThreadEventBody::TurnCompleted { changes, .. } = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await
    else {
        unreachable!()
    };
    assert!(changes.files.is_empty());
    assert!(!changes.edited_since_command && !changes.ran_command);
    harness.send(D, D).await;
    harness.wait_written(&launch, &[A, B, C, D]).await;
    harness.accept(&launch, D).await;
    hook(
        &harness,
        &launch,
        "edit-e",
        "PostToolUse",
        "Edit",
        json!({ "file_path": "e.ts" }),
    )
    .await;
    harness.finish(&launch, &[D], json!({})).await;
    let ThreadEventBody::TurnCompleted { changes, .. } = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await
    else {
        unreachable!()
    };
    assert_eq!(changes.files, ["e.ts"]);
    assert!(changes.edited_since_command && !changes.ran_command);
    harness.send(E, E).await;
    harness.wait_written(&launch, &[A, B, C, D, E]).await;
    harness.accept(&launch, E).await;
    hook(
        &harness,
        &launch,
        "edit-f",
        "PostToolUse",
        "Edit",
        json!({ "file_path": "f.ts" }),
    )
    .await;
    harness.exit(&launch, 3).await;
    let ThreadEventBody::TurnCompleted {
        changes, outcome, ..
    } = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await
    else {
        unreachable!()
    };
    assert!(matches!(
        outcome,
        kratos_protocol::TurnOutcome::Failed { .. }
    ));
    assert_eq!(changes.files, ["f.ts"]);
    assert!(changes.edited_since_command);
    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    let last = harness
        .begin("00000000-0000-4000-8000-00000000000f", 2)
        .await;
    hook(
        &harness,
        &last,
        "monitor",
        "PostToolUse",
        "Monitor",
        json!({ "command": "npm run dev" }),
    )
    .await;
    harness
        .finish(&last, &["00000000-0000-4000-8000-00000000000f"], json!({}))
        .await;
    let ThreadEventBody::TurnCompleted { changes, .. } = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await
    else {
        unreachable!()
    };
    assert!(changes.files.is_empty());
    assert!(!changes.edited_since_command && !changes.ran_command);
}
