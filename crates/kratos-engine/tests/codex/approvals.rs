//! Requests Codex sends for the user: what the thread's permission settles alone, what reaches the
//! user and why, and how each request closes (D83, D84).

use std::collections::BTreeMap;
use std::time::Duration;

use kratos_protocol::{
    Approval, ApprovalDecision, ApprovalOutcome, AskReason, Permission, QuestionOutcome,
    ThreadEventBody, ThreadSettings,
};
use serde_json::{Value, json};

use crate::harness::{Harness, Options};

const THREAD: &str = "thread-A";
const COMMAND: &str = "item/commandExecution/requestApproval";
const PATCH: &str = "item/fileChange/requestApproval";

fn with_permission(permission: Permission) -> Options {
    Options {
        settings: ThreadSettings {
            permission,
            ..ThreadSettings::default()
        },
        ..Options::default()
    }
}

fn command(item_id: &str, command: &str) -> Value {
    json!({
        "threadId": THREAD, "turnId": "turn-1", "itemId": item_id, "startedAtMs": 0,
        "command": command, "cwd": "/work",
    })
}

async fn requested(harness: &mut Harness) -> Approval {
    let event = harness
        .wait_for(|event| matches!(event, ThreadEventBody::ApprovalRequested { .. }))
        .await;
    let ThreadEventBody::ApprovalRequested { approval } = event else {
        unreachable!("wait_for picked an approval request");
    };
    approval
}

#[tokio::test]
async fn under_ask_me_every_command_reaches_the_user_and_the_first_decision_wins() {
    // Catches a permission that lets a command run unseen under Ask me, and a second decision,
    // from another device, overriding the first or answering Codex twice (D83).
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;

    harness.ask("ask-1", COMMAND, command("item-1", "ls")).await;
    let approval = requested(&mut harness).await;
    assert_eq!(approval.why, AskReason::AskEveryAction);
    assert!(
        harness.answers().await.is_empty(),
        "Codex waits for the user"
    );

    let first = harness
        .provider
        .decide(approval.id.clone(), ApprovalDecision::Accept)
        .await
        .unwrap();
    let second = harness
        .provider
        .decide(approval.id.clone(), ApprovalDecision::Decline)
        .await
        .unwrap();

    let decided = ApprovalOutcome::Decided {
        decision: ApprovalDecision::Accept,
    };
    assert_eq!((first, second), (decided.clone(), decided));
    let answers = harness.answered(1).await;
    assert_eq!(answers[0]["id"], "ask-1");
    assert_eq!(answers[0]["result"], json!({ "decision": "accept" }));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(harness.answers().await.len(), 1);
}

#[tokio::test]
async fn under_full_access_a_plain_command_runs_without_asking() {
    // Catches Full access asking about ordinary work, which would make it no different from Ask me.
    let mut harness = Harness::start(with_permission(Permission::FullAccess)).await;
    harness.running_turn("a", "turn-1").await;

    harness
        .ask("ask-1", COMMAND, command("item-1", "cargo test"))
        .await;

    let answers = harness.answered(1).await;
    assert_eq!(answers[0]["result"], json!({ "decision": "accept" }));
    assert!(harness.details().await.approvals.is_empty());
}

#[tokio::test]
async fn a_dangerous_command_reaches_the_user_under_full_access() {
    // Catches the dangerous-command check skipped for the most trusting permission (D18).
    let mut harness = Harness::start(with_permission(Permission::FullAccess)).await;
    harness.running_turn("a", "turn-1").await;

    harness
        .ask("ask-1", COMMAND, command("item-1", "rm -rf /tmp/project"))
        .await;

    assert_eq!(requested(&mut harness).await.why, AskReason::Dangerous);
}

#[tokio::test]
async fn a_bare_shell_reaches_the_user_because_its_later_input_never_does() {
    // Catches the measured gap: text typed into a shell Codex keeps open runs without another
    // approval, so starting the shell is the last point Kratos can ask.
    let mut harness = Harness::start(with_permission(Permission::ApproveForMe)).await;
    harness.running_turn("a", "turn-1").await;

    harness
        .ask("ask-1", COMMAND, command("item-1", "bash"))
        .await;

    assert_eq!(requested(&mut harness).await.why, AskReason::Unchecked);
}

#[tokio::test]
async fn a_command_that_asks_to_leave_the_sandbox_reaches_the_user() {
    // Catches an escalation accepted by the thread's permission, which would run the command
    // outside the sandbox the permission promises.
    let mut harness = Harness::start(with_permission(Permission::FullAccess)).await;
    harness.running_turn("a", "turn-1").await;
    let mut params = command("item-1", "ls");
    params["reason"] = json!("needs to read the cache");

    harness.ask("ask-1", COMMAND, params).await;

    let approval = requested(&mut harness).await;
    assert_eq!(approval.why, AskReason::Escalation);
    assert!(matches!(
        approval.request,
        kratos_protocol::ApprovalRequest::Command { reason: Some(ref reason), .. } if reason == "needs to read the cache"
    ));
}

#[tokio::test]
async fn a_patch_into_kratos_s_own_folder_reaches_the_user_under_full_access() {
    // Catches the measured hole: an approved patch writes outside the sandbox, so a patch into a
    // protected folder must reach the user whatever the permission (D20).
    let mut harness = Harness::start(with_permission(Permission::FullAccess)).await;
    harness.running_turn("a", "turn-1").await;
    let target = harness.root.path().join("kratos").join("client-token");
    harness
        .emit(
            "item/started",
            json!({ "threadId": THREAD, "turnId": "turn-1", "startedAtMs": 0, "item": {
                "type": "fileChange", "id": "patch-1", "status": "inProgress",
                "changes": [{ "path": target, "kind": { "type": "add" }, "diff": "stolen" }],
            } }),
        )
        .await;

    harness
        .ask(
            "ask-1",
            PATCH,
            json!({ "threadId": THREAD, "turnId": "turn-1", "itemId": "patch-1", "startedAtMs": 0 }),
        )
        .await;

    let approval = requested(&mut harness).await;
    assert!(
        matches!(approval.why, AskReason::ProtectedFolder { ref path } if path.ends_with("kratos")),
        "{:?}",
        approval.why
    );
}

#[tokio::test]
async fn a_patch_whose_files_codex_never_named_reaches_the_user() {
    // Catches a patch accepted on trust when Kratos cannot tell where it writes.
    let mut harness = Harness::start(with_permission(Permission::FullAccess)).await;
    harness.running_turn("a", "turn-1").await;

    harness
        .ask(
            "ask-1",
            PATCH,
            json!({ "threadId": THREAD, "turnId": "turn-1", "itemId": "unseen", "startedAtMs": 0 }),
        )
        .await;

    assert_eq!(requested(&mut harness).await.why, AskReason::Unchecked);
}

#[tokio::test]
async fn approvals_waiting_together_are_answered_each_under_its_own_codex_id() {
    // Catches answers crossed between two waiting requests, which would run the command the user
    // declined.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness.ask("ask-1", COMMAND, command("item-1", "ls")).await;
    let first = requested(&mut harness).await;
    harness
        .ask("ask-2", COMMAND, command("item-2", "pwd"))
        .await;
    let second = requested(&mut harness).await;

    harness
        .provider
        .decide(second.id, ApprovalDecision::Decline)
        .await
        .unwrap();
    harness
        .provider
        .decide(first.id, ApprovalDecision::Accept)
        .await
        .unwrap();

    let answers = harness.answered(2).await;
    let by_id: BTreeMap<String, Value> = answers
        .iter()
        .map(|answer| {
            (
                answer["id"].as_str().unwrap().to_owned(),
                answer["result"].clone(),
            )
        })
        .collect();
    assert_eq!(by_id["ask-1"], json!({ "decision": "accept" }));
    assert_eq!(by_id["ask-2"], json!({ "decision": "decline" }));
}

#[tokio::test]
async fn a_request_codex_withdraws_closes_and_a_late_decision_learns_it() {
    // Catches a decision sent for a request Codex no longer waits on, and an approval left open on
    // every device after Codex moved on.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness.ask("ask-1", COMMAND, command("item-1", "ls")).await;
    let approval = requested(&mut harness).await;

    harness
        .emit(
            "serverRequest/resolved",
            json!({ "threadId": THREAD, "requestId": "ask-1" }),
        )
        .await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::ApprovalClosed { .. }))
        .await;

    let late = harness
        .provider
        .decide(approval.id, ApprovalDecision::Accept)
        .await
        .unwrap();
    assert!(matches!(late, ApprovalOutcome::Closed { .. }), "{late:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(harness.answers().await.is_empty());
}

#[tokio::test]
async fn the_end_of_a_turn_closes_the_requests_it_left_waiting() {
    // Catches an approval that outlives its turn, which a user could still accept for a command
    // that will never run.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness.ask("ask-1", COMMAND, command("item-1", "ls")).await;
    requested(&mut harness).await;

    harness.complete(THREAD, "turn-1", "interrupted").await;

    let closed = harness
        .wait_for(|event| matches!(event, ThreadEventBody::ApprovalClosed { .. }))
        .await;
    assert!(
        matches!(closed, ThreadEventBody::ApprovalClosed { ref reason, .. } if reason == "The turn ended")
    );
    assert!(harness.details().await.approvals.is_empty());
}

#[tokio::test]
async fn codex_exiting_closes_every_waiting_request() {
    // Catches approvals left waiting on a process that is gone.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness.ask("ask-1", COMMAND, command("item-1", "ls")).await;
    requested(&mut harness).await;

    harness.exit_fake(1).await;

    harness
        .wait_for(|event| matches!(event, ThreadEventBody::ApprovalClosed { .. }))
        .await;
    assert!(harness.details().await.approvals.is_empty());
}

#[tokio::test]
async fn a_turn_waiting_on_the_user_is_not_stopped_at_its_deadline() {
    // Catches the deadline counting time the user takes to decide, which would stop every turn
    // whose approval waited longer than the deadline.
    let mut harness = Harness::start(Options {
        turn_timeout: Duration::from_millis(300),
        ..Options::default()
    })
    .await;
    harness.running_turn("a", "turn-1").await;
    harness.ask("ask-1", COMMAND, command("item-1", "ls")).await;
    let approval = requested(&mut harness).await;

    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let interrupts = harness
        .turn_requests()
        .await
        .into_iter()
        .filter(|request| request["method"] == "turn/interrupt")
        .count();
    assert_eq!(interrupts, 0);

    harness
        .provider
        .decide(approval.id, ApprovalDecision::Accept)
        .await
        .unwrap();
    harness.held("turn/interrupt", 1).await;
}

#[tokio::test]
async fn a_request_for_more_permissions_grants_what_was_asked_once_accepted() {
    // Catches an accepted permissions request answered with nothing granted, or with more than
    // the agent asked for.
    let mut harness = Harness::start(with_permission(Permission::FullAccess)).await;
    harness.running_turn("a", "turn-1").await;
    let wanted = json!({ "network": { "enabled": true } });
    harness
        .ask(
            "ask-1",
            "item/permissions/requestApproval",
            json!({ "threadId": THREAD, "turnId": "turn-1", "itemId": "item-1", "startedAtMs": 0,
                    "cwd": "/work", "permissions": wanted, "reason": "fetch crates" }),
        )
        .await;
    let approval = requested(&mut harness).await;
    assert_eq!(approval.why, AskReason::Escalation);

    harness
        .provider
        .decide(approval.id, ApprovalDecision::Accept)
        .await
        .unwrap();

    let answers = harness.answered(1).await;
    assert_eq!(answers[0]["result"]["scope"], "turn");
    assert_eq!(
        answers[0]["result"]["permissions"]["network"]["enabled"],
        true
    );
}

#[tokio::test]
async fn a_permissions_approval_shows_the_access_that_accepting_it_grants() {
    // Catches an approval that shows only the agent's reason, so the user grants paths and
    // network they never saw, and two requests with one reason look the same.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    let folder = harness.folder().to_string_lossy().into_owned();
    let wanted = json!({ "fileSystem": { "write": [folder] }, "network": { "enabled": true } });
    harness
        .ask(
            "ask-1",
            "item/permissions/requestApproval",
            json!({ "threadId": THREAD, "turnId": "turn-1", "itemId": "item-1", "startedAtMs": 0,
                    "cwd": "/work", "permissions": wanted, "reason": "run project checks" }),
        )
        .await;
    let approval = requested(&mut harness).await;

    assert_eq!(
        serde_json::to_value(&approval.request).unwrap()["access"],
        json!({ "paths": [{ "path": folder, "access": "write" }], "network": { "kind": "any" } })
    );
    harness
        .provider
        .decide(approval.id, ApprovalDecision::Accept)
        .await
        .unwrap();
    let answers = harness.answered(1).await;
    assert_eq!(answers[0]["result"]["permissions"], wanted);
}

#[tokio::test]
async fn a_command_approval_shows_the_extra_access_the_command_asks_for() {
    // Catches a command approval that hides the extra paths and the network host that accepting
    // it lets the command use.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    let mut params = command("item-1", "cargo fetch");
    params["additionalPermissions"] = json!({ "fileSystem": { "entries": [
        { "access": "read", "path": { "type": "special", "value": { "kind": "root" } } },
        { "access": "deny", "path": { "type": "glob_pattern", "pattern": "**/.env" } },
    ] } });
    params["networkApprovalContext"] = json!({ "host": "crates.io", "protocol": "https" });

    harness.ask("ask-1", COMMAND, params).await;

    let approval = requested(&mut harness).await;
    assert_eq!(
        serde_json::to_value(&approval.request).unwrap()["access"],
        json!({
            "paths": [
                { "path": ":root", "access": "read" },
                { "path": "**/.env", "access": "deny" },
            ],
            "network": { "kind": "host", "host": "crates.io", "protocol": "https" },
        })
    );
}

#[tokio::test]
async fn a_question_takes_the_first_answer_only() {
    // Catches a second device's answer reaching Codex after the first.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness
        .ask(
            "ask-1",
            "item/tool/requestUserInput",
            json!({ "threadId": THREAD, "turnId": "turn-1", "itemId": "item-1", "isBlocking": true,
                    "questions": [{ "id": "color", "header": "Color", "question": "Which color?",
                                    "options": [{ "label": "Red", "description": "warm" }] }] }),
        )
        .await;
    let asked = harness
        .wait_for(|event| matches!(event, ThreadEventBody::QuestionAsked { .. }))
        .await;
    let ThreadEventBody::QuestionAsked { question } = asked else {
        unreachable!("wait_for picked a question");
    };
    assert_eq!(question.items[0].options[0].label, "Red");

    let answer = |text: &str| BTreeMap::from([("color".to_owned(), vec![text.to_owned()])]);
    let first = harness
        .provider
        .answer(question.id.clone(), answer("Red"))
        .await
        .unwrap();
    let second = harness
        .provider
        .answer(question.id, answer("Blue"))
        .await
        .unwrap();

    assert_eq!(
        (first, second),
        (QuestionOutcome::Answered, QuestionOutcome::Answered)
    );
    let answers = harness.answered(1).await;
    assert_eq!(
        answers[0]["result"]["answers"]["color"]["answers"],
        json!(["Red"])
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(harness.answers().await.len(), 1);
}

#[tokio::test]
async fn an_mcp_form_is_declined_because_kratos_cannot_show_it() {
    // Catches an elicitation left unanswered, which would hang the tool call that asked.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;

    harness
        .ask(
            "ask-1",
            "mcpServer/elicitation/request",
            json!({ "threadId": THREAD, "turnId": "turn-1", "serverName": "docs", "mode": "form",
                    "message": "Sign in", "requestedSchema": { "type": "object", "properties": {} } }),
        )
        .await;

    let answers = harness.answered(1).await;
    assert_eq!(answers[0]["result"]["action"], "cancel");
}

#[tokio::test]
async fn a_request_for_a_thread_kratos_does_not_run_is_refused() {
    // Catches a grant to a thread no user of this engine watches.
    let mut harness = Harness::start(with_permission(Permission::FullAccess)).await;
    harness.running_turn("a", "turn-1").await;
    let mut params = command("item-1", "ls");
    params["threadId"] = json!("someone-else");

    harness.ask("ask-1", COMMAND, params).await;

    let answers = harness.answered(1).await;
    assert_eq!(answers[0]["result"], json!({ "decision": "cancel" }));
}
