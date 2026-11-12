//! The installed Claude Code release: the oldest one Kratos starts, the newest one on the feed, and
//! the update Kratos runs for the user.

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
    // Catches an update that is offered but never runs, and a quiet session left on the old
    // release after the update.
    let mut harness = Harness::start(Options::default()).await;
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
async fn a_failed_update_says_why_and_leaves_the_release_and_its_sessions() {
    // Catches an updater's failure reported as success or without its reason, and sessions closed
    // for an update that never happened.
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
    assert!(harness.is_running(&first), "the session still runs");
    let after = harness.provider.cli_release().await;
    assert_eq!(after.installed.as_deref(), Some("2.1.303"));
    assert!(
        matches!(after.update, CliUpdate::Unknown { .. }),
        "the feed holds no release, so the newest is unknown: {:?}",
        after.update
    );
}
