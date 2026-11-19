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
async fn an_update_waits_while_a_thread_runs_a_background_command() {
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    let tasks = |tasks: serde_json::Value| json!({ "type": "system", "subtype": "background_tasks_changed", "tasks": tasks });
    harness
        .emit(
            &launch,
            tasks(json!([{ "task_id": "t1", "task_type": "local_bash" }])),
        )
        .await;
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    let error = harness.provider.update_cli().await.unwrap_err();

    assert!(
        matches!(&error, ProviderError::Conflict(why) if why.contains("background")),
        "{error}"
    );
    assert!(
        harness.is_running(&launch),
        "the CLI and its background command still run"
    );
    let after = harness.provider.cli_release().await;
    assert_eq!(
        after.installed.as_deref(),
        Some("2.1.303"),
        "no updater ran"
    );

    harness.emit(&launch, tasks(json!([]))).await;
    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    loop {
        match harness.provider.update_cli().await {
            Err(ProviderError::Conflict(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            updated => break assert!(updated.is_ok(), "{updated:?}"),
        }
    }
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

#[tokio::test]
async fn engine_shutdown_cancels_an_update_that_has_not_started_installing() {
    let mut harness = Harness::start(Options {
        exit_delay: Duration::from_millis(3_000),
        ..Options::default()
    })
    .await;
    std::fs::write(harness.root.path().join("release"), "2.1.303").unwrap();
    let first = harness.begin(A, 1).await;
    harness.finish(&first, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let provider = harness.provider.clone();
    let update = tokio::spawn(async move { provider.update_cli().await });
    tokio::time::sleep(Duration::from_millis(500)).await;

    harness.provider.shutdown().await;

    let updated = update.await.unwrap();
    assert!(
        matches!(updated, Err(ProviderError::Stopped)),
        "{updated:?}"
    );
    assert!(
        !harness.root.path().join("release.started").exists(),
        "the updater never started"
    );
    assert_eq!(
        std::fs::read_to_string(harness.root.path().join("release")).unwrap(),
        "2.1.303"
    );
}

#[tokio::test]
async fn engine_shutdown_waits_for_an_update_that_is_installing() {
    let harness = Harness::start(Options {
        update_delay: Duration::from_millis(1_500),
        ..Options::default()
    })
    .await;
    std::fs::write(harness.root.path().join("release"), "2.1.303").unwrap();
    let provider = harness.provider.clone();
    let update = tokio::spawn(async move { provider.update_cli().await });
    let started = harness.root.path().join("release.started");
    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    while !started.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the updater never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    harness.provider.shutdown().await;

    assert_eq!(
        std::fs::read_to_string(harness.root.path().join("release")).unwrap(),
        "2.1.400",
        "the updater finished before shutdown returned"
    );
    update.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_caller_that_goes_away_mid_update_does_not_block_the_next_update() {
    let harness = Harness::start(Options {
        update_delay: Duration::from_millis(800),
        ..Options::default()
    })
    .await;
    std::fs::write(harness.root.path().join("release"), "2.1.303").unwrap();
    let provider = harness.provider.clone();
    let caller = tokio::spawn(async move { provider.update_cli().await });
    let started = harness.root.path().join("release.started");
    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    while !started.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the updater never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    caller.abort();
    let updated = harness.root.path().join("release");
    while std::fs::read_to_string(&updated).unwrap() != "2.1.400" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the updater never finished"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    std::fs::remove_file(&started).unwrap();
    let provider = harness.provider.clone();
    let again = tokio::spawn(async move {
        loop {
            match provider.update_cli().await {
                Err(ProviderError::Conflict(_)) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                other => return other,
            }
        }
    });
    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    while !started.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a later update still answers that one runs"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let again = tokio::time::timeout(crate::harness::WAIT, again).await;
    assert!(
        matches!(again, Ok(Ok(Ok(_)))),
        "the later update did not finish: {again:?}"
    );
}
