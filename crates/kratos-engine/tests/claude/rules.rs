//! The rules of Appendix I that bind a provider that drives a CLI: a thread never shows running
//! without a process (L1), a failed write stops the provider (L2), a CLI that keeps dying is held
//! back (L4), a failed resume says so (L7), and each completed reply is saved (D88).

use std::time::{Duration, Instant};

use kratos_claude_client::transcript_path;
use kratos_engine::providers::ProviderError;
use kratos_protocol::{
    AgentItem, LiveUpdate, MessageStatus, PauseReason, ThreadEventBody, ThreadSettings, TurnId,
    TurnOutcome,
};
use serde_json::json;

use crate::harness::{A, B, Harness, Options, message_id, text_input};

fn accepted_by(status: &MessageStatus, turn: &str) -> bool {
    matches!(status, MessageStatus::Accepted { turn_id } if turn_id.as_str() == turn)
}

#[tokio::test]
async fn a_crash_ends_the_running_turn_so_no_thread_shows_running_without_a_process() {
    // Catches a thread that keeps showing a turn running after its CLI died (L1), and a crashed
    // turn that leaves no failure in the log for the user to read.
    let harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    harness.message_becomes(A, |s| accepted_by(s, A)).await;
    assert_eq!(
        harness.details().await.running_turn,
        Some(TurnId::new(A.to_owned()))
    );
    harness.send(B, "B").await;

    harness.exit(&launch, 9).await;
    let paused = harness
        .paused_for(|r| matches!(r, PauseReason::TurnFailed { .. }))
        .await;

    assert!(
        matches!(&paused, PauseReason::TurnFailed { error, .. } if error.contains("code 9")),
        "{paused:?}"
    );
    assert_eq!(harness.details().await.running_turn, None);
    let failed = harness.log().await.into_iter().any(|event| {
        matches!(event, ThreadEventBody::TurnCompleted { outcome: TurnOutcome::Failed { error }, .. } if error.contains("code 9"))
    });
    assert!(failed, "the turn's end is in the log");
}

#[tokio::test]
async fn a_failed_event_write_closes_claude_and_refuses_new_work() {
    // Catches a Claude process that keeps running after the records stopped keeping up with it
    // (L2), and a provider that takes new messages it cannot record.
    let harness = Harness::start(Options::default()).await;
    harness.send(A, "A").await;
    let launch = harness.launch(1).await;
    harness.wait_written(&launch, &[A]).await;
    harness.init(&launch).await;
    let database = harness.root.path().join("kratos").join("kratos.db");
    let sabotage = rusqlite::Connection::open(database).unwrap();
    sabotage
        .execute_batch(
            "CREATE TRIGGER no_turns BEFORE INSERT ON thread_events
             WHEN NEW.event LIKE '%\"turn.started\"%'
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();

    harness.accept(&launch, A).await;
    harness.wait_ended(&launch).await;
    let refused = harness
        .provider
        .send(
            harness.thread_id.clone(),
            message_id(B),
            text_input("B"),
            kratos_protocol::MessageOrigin::User,
        )
        .await;

    assert!(
        matches!(&refused, Err(ProviderError::Unavailable(reason)) if reason.contains("could not record")),
        "{refused:?}"
    );
    assert_eq!(harness.launches().len(), 1, "nothing starts again");
}

/// Sends message A, kills the CLI that took it before it began a turn, and retries the message
/// that failed, as many times as `deaths`. Returns when each retry was issued and when each
/// restart showed up, both measured from a moment before the CLI was told to die, so a restart
/// that came sooner than its wait would show.
async fn die_and_retry(harness: &Harness, deaths: usize) -> Vec<(Instant, Instant)> {
    harness.send(A, "A").await;
    let mut timings = Vec::new();
    for attempt in 1..=deaths {
        let launch = harness.launch(attempt).await;
        harness.wait_written(&launch, &[A]).await;
        let before_the_death = Instant::now();
        harness.exit(&launch, 1).await;
        harness
            .message_becomes(A, |s| matches!(s, MessageStatus::Failed { .. }))
            .await;
        harness
            .provider
            .retry(harness.thread_id.clone(), message_id(A))
            .await
            .unwrap();
        harness.launch(attempt + 1).await;
        timings.push((before_the_death, Instant::now()));
    }
    timings
}

#[tokio::test]
async fn a_retry_inside_the_wait_starts_the_cli_once_the_wait_ends_and_each_failure_doubles_it() {
    // Catches a pause that promises a restart nothing performs (L4), a restart before the wait
    // ends, and a wait that does not grow: the second death must hold the next start for twice
    // the first wait.
    const WAIT: Duration = Duration::from_millis(500);
    let harness = Harness::start(Options {
        restart_wait: WAIT,
        ..Options::default()
    })
    .await;

    let timings = die_and_retry(&harness, 2).await;

    for (attempt, (before_the_death, started)) in timings.iter().enumerate() {
        let wait = WAIT * (1 << attempt);
        assert!(
            started.duration_since(*before_the_death) >= wait,
            "start {} came {:?} after the CLI died, inside its wait of {wait:?}",
            attempt + 2,
            started.duration_since(*before_the_death)
        );
    }
    assert_eq!(
        harness.details().await.paused,
        None,
        "the wait pauses nothing"
    );
    let third = harness.launch(3).await;
    harness.wait_written(&third, &[A]).await;
}

#[tokio::test]
async fn removing_the_message_during_the_wait_starts_no_cli() {
    // Catches a restart timer that starts a CLI for a thread with nothing left to send.
    let harness = Harness::start(Options {
        restart_wait: Duration::from_millis(500),
        ..Options::default()
    })
    .await;
    harness.send(A, "A").await;
    let first = harness.launch(1).await;
    harness.wait_written(&first, &[A]).await;
    harness.exit(&first, 1).await;
    harness
        .message_becomes(A, |s| matches!(s, MessageStatus::Failed { .. }))
        .await;
    harness
        .provider
        .retry(harness.thread_id.clone(), message_id(A))
        .await
        .unwrap();

    harness
        .provider
        .remove(harness.thread_id.clone(), message_id(A))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;

    assert_eq!(harness.launches().len(), 1, "nothing was left to send");
}

#[tokio::test]
async fn a_cli_that_keeps_dying_is_started_a_bounded_number_of_times_until_the_user_resumes() {
    // Catches a CLI restarted forever (L4), and a user who cannot start it again once they have
    // fixed what stopped it.
    let harness = Harness::start(Options {
        max_restarts: 3,
        ..Options::default()
    })
    .await;
    harness.send(A, "A").await;
    for attempt in 1..=3 {
        let launch = harness.launch(attempt).await;
        harness.wait_written(&launch, &[A]).await;
        harness.exit(&launch, 1).await;
        harness
            .message_becomes(A, |s| matches!(s, MessageStatus::Failed { .. }))
            .await;
        harness
            .provider
            .retry(harness.thread_id.clone(), message_id(A))
            .await
            .unwrap();
    }

    let paused = harness
        .paused_for(|r| matches!(r, PauseReason::Unavailable { .. }))
        .await;

    assert!(
        matches!(&paused, PauseReason::Unavailable { error }
            if error.contains("3 times in a row") && error.contains("Claude exited (code 1)")),
        "{paused:?}"
    );
    assert_eq!(harness.launches().len(), 3, "the fourth start is refused");
    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    let fourth = harness.launch(4).await;
    harness.wait_written(&fourth, &[A]).await;
}

#[tokio::test]
async fn a_start_that_fails_pauses_the_thread_with_the_cli_s_own_error_and_keeps_the_message() {
    // Catches a CLI that cannot start whose error never reaches the user (L8), and a message called
    // failed or lost though it never left the queue.
    let harness = Harness::start(Options::default()).await;
    harness.start_failing(1);

    harness.send(A, "A").await;
    let paused = harness
        .paused_for(|r| matches!(r, PauseReason::Unavailable { .. }))
        .await;

    assert!(
        matches!(&paused, PauseReason::Unavailable { error } if error.contains("Not logged in")),
        "{paused:?}"
    );
    assert_eq!(harness.message(A).await.status, MessageStatus::Queued);
    harness.start_working();
    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    // The starts that failed left no line in the fake's log.
    let launch = harness.launch(1).await;
    harness.wait_written(&launch, &[A]).await;
}

#[tokio::test]
async fn a_resume_that_finds_no_saved_session_says_so_and_never_starts_a_new_one() {
    // Catches a thread whose history is gone that silently continues in a fresh session (L7).
    let harness = Harness::start(Options::default()).await;
    let first = harness.begin(A, 1).await;
    harness.finish(&first, &[A], json!({})).await;
    harness.message_becomes(A, |s| accepted_by(s, A)).await;
    let path = transcript_path(&harness.claude_home(), &harness.folder(), first.session());
    std::fs::remove_file(path).unwrap();
    // Changing the model ends the idle process before the next message, which an exit would not.
    let settings = ThreadSettings {
        model: kratos_protocol::RequiredText::new("claude-other"),
        ..ThreadSettings::default()
    };
    harness
        .provider
        .set_settings(harness.thread_id.clone(), settings)
        .await
        .unwrap();
    harness.wait_ended(&first).await;

    harness.send(B, "B").await;
    let paused = harness
        .paused_for(|r| matches!(r, PauseReason::Unavailable { .. }))
        .await;

    assert!(
        matches!(&paused, PauseReason::Unavailable { error }
            if error.contains("cannot resume") && error.contains("does not start a new session")),
        "{paused:?}"
    );
    assert_eq!(
        harness.launches().len(),
        1,
        "no process starts in its place"
    );
    assert_eq!(harness.message(B).await.status, MessageStatus::Queued);
}

#[tokio::test]
async fn each_completed_assistant_message_is_saved_whole_and_its_text_streams_live() {
    // Catches a reply that is only streamed and never saved (D88), saved text that differs from
    // the streamed text's id so a client cannot replace one with the other, and a tool call lost
    // from the history.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    harness.message_becomes(A, |s| accepted_by(s, A)).await;
    let (_, mut live) = harness.hub.watch_live(&harness.thread_id);

    harness
        .emit(
            &launch,
            json!({ "type": "stream_event", "api_message_id": "msg_1", "session_id": "s",
                    "event": { "type": "content_block_delta", "index": 0,
                               "delta": { "type": "text_delta", "text": "po" } } }),
        )
        .await;
    harness
        .emit(
            &launch,
            json!({ "type": "assistant", "uuid": "u1", "session_id": "s",
                    "message": { "id": "msg_1", "content": [
                        { "type": "text", "text": "pong" },
                        { "type": "tool_use", "id": "toolu_1", "name": "Read", "input": {} } ] } }),
        )
        .await;

    let update = tokio::time::timeout(Duration::from_secs(20), live.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        update,
        LiveUpdate::ReplyDelta {
            turn_id: TurnId::new(A.to_owned()),
            item_id: "msg_1".into(),
            delta: "po".into()
        }
    );
    let items: Vec<AgentItem> = {
        let mut found = Vec::new();
        while found.len() < 2 {
            if let ThreadEventBody::ItemCompleted { item, .. } = harness.next_event().await.event {
                found.push(item);
            }
        }
        found
    };
    assert_eq!(
        items,
        [
            AgentItem::Message {
                id: "msg_1".into(),
                text: "pong".into()
            },
            AgentItem::Other {
                id: "toolu_1".into(),
                provider_type: "tool_use:Read".into()
            },
        ]
    );
}

#[tokio::test]
async fn a_context_limit_is_refused_because_the_cli_cannot_set_its_window() {
    // Catches a setting that is stored and then ignored (D76), which would tell the user their
    // context is limited when it is not.
    let harness = Harness::start(Options::default()).await;
    let settings: ThreadSettings = serde_json::from_value(json!({
        "model": null, "permission": "askMe", "planMode": false, "contextLimitTokens": 50_000,
    }))
    .unwrap();

    let refused = harness
        .provider
        .set_settings(harness.thread_id.clone(), settings)
        .await;

    assert!(
        matches!(refused, Err(ProviderError::Unsupported(_))),
        "{refused:?}"
    );
}
