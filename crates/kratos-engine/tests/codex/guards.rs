//! What Kratos asks of Codex on every launch, thread, and turn, so the sandbox and the approval
//! rules hold whatever Codex's own configuration says (D84, D93).

use kratos_protocol::{AccessLimit, MessageOrigin, Permission, ThreadId, ThreadSettings};
use serde_json::json;

use crate::harness::{Harness, Options};

fn settings(permission: Permission, plan_mode: bool) -> Options {
    Options {
        settings: ThreadSettings {
            permission,
            plan_mode,
            ..ThreadSettings::default()
        },
        ..Options::default()
    }
}

fn from_thread(sender: ThreadId, permission: Permission, plan_mode: bool) -> MessageOrigin {
    MessageOrigin::Thread {
        thread_id: sender,
        limit: AccessLimit {
            permission,
            plan_mode,
        },
    }
}

#[tokio::test]
async fn every_turn_runs_untrusted_with_the_permission_s_own_profile() {
    // Catches a turn that inherits the user's Codex configuration, such as `never` or a looser
    // sandbox, instead of the permission the thread shows.
    let mut harness = Harness::start(settings(Permission::ApproveForMe, false)).await;

    harness.send("a", "first").await;

    let start = &harness.held("turn/start", 1).await[0]["params"];
    assert_eq!(start["approvalPolicy"], "untrusted");
    assert_eq!(start["approvalsReviewer"], "user");
    assert_eq!(start["permissions"], "kratos-approve");
    assert_eq!(start["collaborationMode"]["mode"], "default");
    assert_eq!(
        start["collaborationMode"]["settings"]["model"],
        "fake-model"
    );
}

#[tokio::test]
async fn plan_mode_runs_the_read_only_profile_whatever_the_permission() {
    // Catches plan mode that only changes the agent's instructions, which leaves it free to write.
    let mut harness = Harness::start(settings(Permission::FullAccess, true)).await;

    harness.send("a", "first").await;

    let start = &harness.held("turn/start", 1).await[0]["params"];
    assert_eq!(start["permissions"], "kratos-plan");
    assert_eq!(start["collaborationMode"]["mode"], "plan");
}

#[tokio::test]
async fn another_thread_s_message_runs_with_the_narrower_of_the_two_threads_settings() {
    // Catches a Full access thread that runs an Approve for me sender's request with no sandbox, a
    // sender whose wider permission widens the receiver's, and a planning sender's request that
    // writes.
    for (receiver, (permission, plan_mode), profile, mode) in [
        (
            Permission::FullAccess,
            (Permission::ApproveForMe, false),
            "kratos-approve",
            "default",
        ),
        (
            Permission::AskMe,
            (Permission::FullAccess, false),
            "kratos-ask",
            "default",
        ),
        (
            Permission::FullAccess,
            (Permission::FullAccess, true),
            "kratos-plan",
            "plan",
        ),
    ] {
        let mut harness = Harness::start(settings(receiver, false)).await;
        let sender = harness.another_thread("sender").await;

        harness
            .send_as("a", "work", from_thread(sender, permission, plan_mode))
            .await;

        let start = &harness.held("turn/start", 1).await[0]["params"];
        assert_eq!(
            (&start["permissions"], &start["collaborationMode"]["mode"]),
            (&json!(profile), &json!(mode)),
            "{receiver:?} receiving from {permission:?}, planning {plan_mode}"
        );
        let label = start["input"][0]["text"].as_str().unwrap();
        assert!(label.starts_with("[Message from Kratos thread "), "{label}");
        assert_eq!(start["input"][1]["text"], "work");
    }
}

#[tokio::test]
async fn the_thread_keeps_the_sender_s_permission_until_the_user_raises_it() {
    // Catches a limit that ends with the turn, so the user's next message runs with Full access
    // while the agent still remembers the other thread's request, a lowered permission the thread
    // does not show, and one the user cannot raise again.
    let mut harness = Harness::start(settings(Permission::FullAccess, false)).await;
    let sender = harness.another_thread("sender").await;
    let origin = from_thread(sender, Permission::AskMe, false);
    harness.running_turn_as("a", "turn-1", origin).await;
    assert_eq!(
        harness.details().await.settings.permission,
        Permission::AskMe
    );
    harness.complete("thread-A", "turn-1", "completed").await;

    harness.send("b", "mine").await;
    let started = harness.held("turn/start", 2).await;
    assert_eq!(started[1]["params"]["permissions"], "kratos-ask");

    let full_access = settings(Permission::FullAccess, false).settings;
    harness
        .provider
        .set_settings(harness.thread_id.clone(), full_access)
        .await
        .unwrap();
    harness.settle_start("turn-2", "inProgress").await;
    harness.complete("thread-A", "turn-2", "completed").await;
    harness.send("c", "raised").await;
    let started = harness.held("turn/start", 3).await;
    assert_eq!(started[2]["params"]["permissions"], ":danger-full-access");
}

#[tokio::test]
async fn the_thread_opens_untrusted_and_the_launch_defines_every_profile() {
    // Catches a launch that leaves a profile undefined, which Codex rejects at the first turn, or
    // a profile that lets agents read Kratos's data folder and its client token (D19).
    let mut harness = Harness::start(settings(Permission::AskMe, false)).await;
    harness.send("a", "first").await;
    harness.held("turn/start", 1).await;

    let opened = harness.control("test/threadRequests", json!({})).await;
    let started = &opened[0]["params"];
    assert_eq!(opened[0]["method"], "thread/start");
    assert_eq!(started["approvalPolicy"], "untrusted");
    assert_eq!(started["permissions"], "kratos-ask");
    assert_eq!(started["cwd"], json!(harness.folder()));

    let environment = harness.control("test/environment", json!({})).await;
    let arguments: Vec<String> = serde_json::from_value(environment["arguments"].clone()).unwrap();
    assert_eq!(
        environment["cwd"],
        json!(harness.root.path().join("server"))
    );
    let kratos_home = serde_json::to_string(&harness.root.path().join("kratos")).unwrap();
    for profile in ["kratos-plan", "kratos-ask", "kratos-approve"] {
        let table = arguments
            .iter()
            .find(|argument| argument.starts_with(&format!("permissions.{profile}.filesystem=")))
            .unwrap_or_else(|| panic!("{profile} is defined: {arguments:?}"));
        assert!(
            table.contains(&format!("{kratos_home}=\"deny\"")),
            "{profile} denies Kratos's data folder: {table}"
        );
    }
    assert!(arguments.contains(&"default_permissions=\"kratos-plan\"".to_owned()));
}

#[tokio::test]
async fn a_thread_with_a_model_starts_codex_on_that_model() {
    // Catches the thread's model setting ignored when Codex opens the thread.
    let mut options = settings(Permission::AskMe, false);
    options.settings.model = Some("other-model".into());
    let mut harness = Harness::start(options).await;

    harness.send("a", "first").await;

    let start = &harness.held("turn/start", 1).await[0]["params"];
    assert_eq!(start["model"], "other-model");
    let opened = harness.control("test/threadRequests", json!({})).await;
    assert_eq!(opened[0]["params"]["model"], "other-model");
}
