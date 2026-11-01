//! What Kratos asks of Codex on every launch, thread, and turn, so the sandbox and the approval
//! rules hold whatever Codex's own configuration says (D84, D93).

use kratos_protocol::{Permission, ThreadSettings};
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
