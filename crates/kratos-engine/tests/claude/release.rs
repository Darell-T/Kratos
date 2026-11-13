//! The installed Claude Code release: the oldest one Kratos starts, the newest one on the feed, and
//! the update Kratos runs for the user.

use std::time::Duration;

use kratos_engine::providers::ProviderError;
use kratos_protocol::{CliRelease, CliUpdate, PauseReason, ThreadEventBody};
use serde_json::json;

use crate::harness::{A, B, Harness, Options};

fn release(installed: &str, update: CliUpdate) -> CliRelease {
    CliRelease {
        installed: Some(installed.to_owned()),
        oldest_supported: "2.1.303".to_owned(),
        update,
    }
}

#[tokio::test]
async fn a_cli_older_than_kratos_runs_starts_no_session_and_the_thread_names_the_release() {
    // Catches Kratos driving a CLI too old for its control requests, and a refusal that hides
    // which release to update from.
    let harness = Harness::start(Options::default()).await;
    std::fs::write(harness.root.path().join("release"), "2.1.200").unwrap();

    harness.send(A, A).await;

    let paused = harness
        .paused_for(|reason| matches!(reason, PauseReason::Unavailable { .. }))
        .await;
    let PauseReason::Unavailable { error } = paused else {
        unreachable!()
    };
    assert!(
        error.contains("2.1.200") && error.contains("2.1.303"),
        "{error}"
    );
    assert!(harness.launches().is_empty(), "no session started");
}

#[tokio::test]
async fn an_update_installs_the_newest_release_and_the_next_message_resumes_on_it() {
    // Catches an update that is offered but never runs, an updater run while a session still
    // uses the CLI's files (the fake's updater refuses then), and a thread that cannot resume
    // after the update.
    let mut harness = Harness::start(Options {
        exit_delay: Duration::from_millis(500),
        ..Options::default()
    })
    .await;
    std::fs::write(harness.root.path().join("feed").join("latest"), "2.1.400\n").unwrap();
    assert_eq!(
        harness.provider.cli_release().await,
        release(
            "2.1.303",
            CliUpdate::Available {
                latest: "2.1.400".to_owned()
            }
        )
    );
    let first = harness.begin(A, 1).await;
    harness.finish(&first, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    let updated = harness.provider.update_cli().await.unwrap();

    assert_eq!(updated, release("2.1.400", CliUpdate::Current));
    harness.wait_ended(&first).await;
    harness.send(B, B).await;
    let second = harness.launch(2).await;
    assert_eq!(
        second.value("--resume"),
        Some(first.session().to_string().as_str())
    );
}

#[tokio::test]
async fn a_failed_update_says_why_leaves_the_release_and_lets_the_thread_go_on() {
    // Catches an updater's failure reported as success or without its reason, and a hold on new
    // sessions that outlives an update that never happened.
    let mut harness = Harness::start(Options {
        update_to: "fail",
        ..Options::default()
    })
    .await;
    let first = harness.begin(A, 1).await;
    harness.finish(&first, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    let error = harness.provider.update_cli().await.unwrap_err();

    assert!(
        matches!(&error, ProviderError::Unavailable(why) if why.contains("the download was interrupted")),
        "{error}"
    );
    let after = harness.provider.cli_release().await;
    assert_eq!(after.installed.as_deref(), Some("2.1.303"));
    assert!(
        matches!(after.update, CliUpdate::Unknown { .. }),
        "the feed holds no release, so the newest is unknown: {:?}",
        after.update
    );
    harness.send(B, B).await;
    let second = harness.launch(2).await;
    assert_eq!(
        second.value("--resume"),
        Some(first.session().to_string().as_str())
    );
}

#[tokio::test]
async fn an_update_waits_for_the_user_while_a_thread_works() {
    // Catches an updater run under a running turn, which would replace files its CLI still loads,
    // and an update that closes a working thread's session to make way.
    let mut harness = Harness::start(Options::default()).await;
    let first = harness.begin(A, 1).await;

    let error = harness.provider.update_cli().await.unwrap_err();

    assert!(
        matches!(&error, ProviderError::Conflict(why) if why.contains("working")),
        "{error}"
    );
    assert!(harness.is_running(&first), "the working session still runs");
    let after = harness.provider.cli_release().await;
    assert_eq!(
        after.installed.as_deref(),
        Some("2.1.303"),
        "no updater ran"
    );
    harness.finish(&first, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    harness.provider.update_cli().await.unwrap();
}

#[tokio::test]
async fn a_cli_another_manager_installed_is_never_updated_under_its_running_sessions() {
    // Catches Kratos running npm's or WinGet's updater, which replaces files a running CLI still
    // loads, and a refusal that hides which manager to update with.
    let harness = Harness::start(Options {
        install: "npm",
        ..Options::default()
    })
    .await;
    let first = harness.begin(A, 1).await;

    let error = harness.provider.update_cli().await.unwrap_err();

    assert!(
        matches!(&error, ProviderError::Unsupported(why) if why.contains("npm")),
        "{error}"
    );
    assert!(harness.is_running(&first), "the session still runs");
    let after = harness.provider.cli_release().await;
    assert_eq!(
        after.installed.as_deref(),
        Some("2.1.303"),
        "nothing updated"
    );
    assert!(
        matches!(&after.update, CliUpdate::Unknown { reason } if reason.contains("npm")),
        "{:?}",
        after.update
    );
}
