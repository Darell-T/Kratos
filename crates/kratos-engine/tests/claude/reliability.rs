//! The scenarios of `packages/engine/test/claude-reliability.test.ts` against the Rust provider:
//! what becomes of a message when Claude exits, what an engine restart leaves, and what a change
//! of settings does to the process.

use kratos_engine::providers::{ProviderError, Sent};
use kratos_protocol::{
    MessageOrigin, MessageStatus, PauseReason, Permission, ThreadEventBody, ThreadSettings,
    TurnOutcome,
};
use serde_json::json;

use crate::harness::{A, B, C, D, E, Harness, Options, message_id, text_input};

fn accepted_by(status: &MessageStatus, turn: &str) -> bool {
    matches!(status, MessageStatus::Accepted { turn_id } if turn_id.as_str() == turn)
}

#[tokio::test]
async fn fails_a_message_the_process_never_started_and_judges_later_losses_by_the_transcript_only_for_messages_the_lost_process_received()
 {
    // Catches a message resent after Claude may have taken it (D11), a message called delivered
    // that its transcript lacks, a message Claude never began a turn for that cannot be sent again,
    // and a queue that runs on past a message the user has not settled.
    let harness = Harness::start(Options::default()).await;
    harness.send(A, "A").await;
    let first = harness.launch(1).await;
    harness.wait_written(&first, &[A]).await;

    harness.exit(&first, 1).await;
    let failed = harness
        .message_becomes(A, |s| matches!(s, MessageStatus::Failed { .. }))
        .await;

    assert!(
        matches!(&failed.status, MessageStatus::Failed { error } if error.contains("exited")),
        "{failed:?}"
    );
    let paused = harness
        .paused_for(|r| matches!(r, PauseReason::Refused { .. }))
        .await;
    assert!(
        matches!(paused, PauseReason::Refused { ref message_id, .. } if message_id.as_str() == A)
    );

    harness
        .provider
        .retry(harness.thread_id.clone(), message_id(A))
        .await
        .unwrap();
    let second = harness.launch(2).await;
    assert_eq!(
        second.value("--session-id"),
        Some(first.session().to_string().as_str())
    );
    harness.wait_written(&second, &[A]).await;
    harness.init(&second).await;
    harness.accept(&second, A).await;
    harness.message_becomes(A, |s| accepted_by(s, A)).await;

    for (id, text) in [(B, "B"), (C, "C"), (D, "D")] {
        harness.send(id, text).await;
    }
    harness
        .provider
        .steer(harness.thread_id.clone(), message_id(B))
        .await
        .unwrap();
    harness
        .provider
        .steer(harness.thread_id.clone(), message_id(C))
        .await
        .unwrap();
    harness.wait_written(&second, &[A, B, C]).await;
    harness.record(&second, B);
    harness.exit(&second, 3).await;
    harness
        .message_becomes(C, |s| matches!(s, MessageStatus::Unknown { .. }))
        .await;

    assert!(
        accepted_by(&harness.message(B).await.status, A),
        "B is in the transcript"
    );
    assert_eq!(harness.message(D).await.status, MessageStatus::Queued);
    let retried = harness
        .provider
        .retry(harness.thread_id.clone(), message_id(C))
        .await;
    assert!(
        matches!(retried, Err(ProviderError::Conflict(_))),
        "{retried:?}"
    );
    let resumed = harness.provider.resume(harness.thread_id.clone()).await;
    assert!(
        matches!(resumed, Err(ProviderError::Conflict(_))),
        "{resumed:?}"
    );

    let discarded = harness
        .provider
        .discard(harness.thread_id.clone(), message_id(C))
        .await
        .unwrap();
    assert_eq!(discarded.status, MessageStatus::Cancelled);
    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    let third = harness.launch(3).await;
    assert_eq!(
        third.value("--resume"),
        Some(first.session().to_string().as_str())
    );
    harness.wait_written(&third, &[D]).await;
    harness.init(&third).await;
    harness.accept(&third, D).await;
    harness.message_becomes(D, |s| accepted_by(s, D)).await;
}

#[tokio::test]
async fn a_message_in_flight_when_claude_exits_stays_unknown_until_the_user_settles_it() {
    // Catches a queue that resumes before a lost message is settled, which would run work behind a
    // message Claude may or may not have taken, and a settled message started again by a resume.
    let harness = Harness::start(Options::default()).await;
    let first = harness.begin(A, 1).await;
    harness.send(E, "E").await;
    harness
        .provider
        .steer(harness.thread_id.clone(), message_id(E))
        .await
        .unwrap();
    harness.wait_written(&first, &[A, E]).await;

    harness.exit(&first, 3).await;
    harness
        .message_becomes(E, |s| matches!(s, MessageStatus::Unknown { .. }))
        .await;
    let resumed = harness.provider.resume(harness.thread_id.clone()).await;

    assert!(
        matches!(resumed, Err(ProviderError::Conflict(_))),
        "{resumed:?}"
    );
    assert_eq!(
        harness.launches().len(),
        1,
        "nothing starts behind the lost message"
    );
    let paused = harness.details().await.paused;
    assert!(
        matches!(paused, Some(PauseReason::DeliveryUncertain { .. })),
        "{paused:?}"
    );
}

#[tokio::test]
async fn a_restart_before_claude_echoes_the_first_message_resumes_that_session() {
    // Catches the thread's first session forgotten when the engine stops after Claude took the
    // first message but before Kratos saw it: the message was called unknown, and the next one
    // started a new conversation without it (L7).
    let harness = Harness::start(Options::default()).await;
    harness.send(A, A).await;
    let first = harness.launch(1).await;
    harness.wait_written(&first, &[A]).await;
    harness.record(&first, A);
    let (root, thread_id) = harness.stop().await;

    let harness = Harness::start_in(root, Options::default(), Some(thread_id)).await;

    let status = harness.message(A).await.status;
    assert!(accepted_by(&status, A), "{status:?}");
    harness.send(B, B).await;
    let second = harness.launch(2).await;
    assert_eq!(
        second.value("--resume"),
        Some(first.session().to_string().as_str())
    );
}

#[tokio::test]
async fn restores_a_thread_after_an_engine_restart_without_resending_delivered_work() {
    // Catches a restart that starts Claude again for a thread that only needs settling, a message
    // in the transcript that is called unknown, one it lacks that is called delivered, and a
    // thread that still shows a turn running with no process (L1).
    let harness = Harness::start(Options::default()).await;
    let thread_id = harness.thread_id.clone();
    let launch = harness.begin(A, 1).await;
    harness.message_becomes(A, |s| accepted_by(s, A)).await;
    for (id, text) in [(B, "B"), (C, "C"), (D, "D")] {
        harness.send(id, text).await;
    }
    harness
        .provider
        .steer(thread_id.clone(), message_id(B))
        .await
        .unwrap();
    harness
        .provider
        .steer(thread_id.clone(), message_id(C))
        .await
        .unwrap();
    harness.wait_written(&launch, &[A, B, C]).await;
    harness.record(&launch, B);
    let before = harness.launches().len();
    let (root, thread_id) = harness.stop().await;

    let harness = Harness::start_in(root, Options::default(), Some(thread_id)).await;

    assert_eq!(
        harness.launches().len(),
        before,
        "settling starts no process"
    );
    assert!(accepted_by(&harness.message(B).await.status, A));
    assert!(matches!(
        harness.message(C).await.status,
        MessageStatus::Unknown { .. }
    ));
    assert_eq!(harness.message(D).await.status, MessageStatus::Queued);
    let details = harness.details().await;
    assert_eq!(details.running_turn, None);
    assert!(
        matches!(details.paused, Some(PauseReason::DeliveryUncertain { .. })),
        "{:?}",
        details.paused
    );
    let log = harness.log().await;
    assert!(log.iter().any(|event| matches!(event,
        ThreadEventBody::TurnCompleted { turn_id, outcome: TurnOutcome::Interrupted, .. } if turn_id.as_str() == A)));

    let discarded = harness
        .provider
        .discard(harness.thread_id.clone(), message_id(C))
        .await
        .unwrap();
    assert_eq!(discarded.status, MessageStatus::Cancelled);
    harness
        .provider
        .resume(harness.thread_id.clone())
        .await
        .unwrap();
    let launch = harness.launch(before + 1).await;
    assert!(launch.value("--resume").is_some());
    harness.wait_written(&launch, &[D]).await;
}

#[tokio::test]
async fn relaunches_an_idle_session_when_its_settings_change_and_never_launches_a_mode_that_skips_the_ask()
 {
    // Catches a changed model that runs in the old process, a settings change that restarts a
    // turn that is running (D87), a launch that bypasses permissions before Kratos's own rules
    // exist, and a plan setting that does not reach the process.
    let mut harness = Harness::start(Options::default()).await;
    let first = harness.begin(A, 1).await;
    harness.finish(&first, &[A], json!({})).await;
    harness.message_becomes(A, |s| accepted_by(s, A)).await;
    // The message is accepted when the turn begins, so only the turn's end shows the thread idle.
    // A settings change before it keeps the process until the next message, as one during a turn
    // must.
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let settings = |model: &str, permission, plan_mode| ThreadSettings {
        model: kratos_protocol::RequiredText::new(model),
        permission,
        plan_mode,
        context_limit_tokens: None,
        report_format: None,
    };

    harness
        .provider
        .set_settings(
            harness.thread_id.clone(),
            settings("claude-opus-test", Permission::AskMe, false),
        )
        .await
        .unwrap();
    harness.wait_ended(&first).await;
    let second = harness.begin(B, 2).await;
    assert_eq!(second.value("--model"), Some("claude-opus-test"));
    assert_eq!(
        second.value("--resume"),
        Some(first.session().to_string().as_str())
    );

    harness
        .provider
        .set_settings(
            harness.thread_id.clone(),
            settings("claude-haiku-test", Permission::FullAccess, false),
        )
        .await
        .unwrap();
    assert!(
        harness.is_running(&second),
        "a turn that runs keeps its process"
    );
    harness.finish(&second, &[B], json!({})).await;
    harness.send(C, "C").await;
    let third = harness.launch(3).await;
    harness.wait_ended(&second).await;
    harness.wait_written(&third, &[C]).await;
    harness.init(&third).await;
    harness.accept(&third, C).await;
    harness.finish(&third, &[C], json!({})).await;

    assert_eq!(third.value("--model"), Some("claude-haiku-test"));
    assert_eq!(
        third.value("--permission-mode"),
        Some("default"),
        "full access does not skip the ask"
    );
    assert!(!third.has("--allow-dangerously-skip-permissions"));
    harness
        .provider
        .set_settings(
            harness.thread_id.clone(),
            settings("claude-haiku-test", Permission::ApproveForMe, true),
        )
        .await
        .unwrap();
    harness.send(D, "D").await;
    let fourth = harness.launch(4).await;
    assert_eq!(fourth.value("--permission-mode"), Some("plan"));
    assert!(harness.details().await.settings.plan_mode);
}

#[tokio::test]
async fn rejects_unknown_threads_shares_duplicate_sends_and_closes_every_thread_at_shutdown() {
    // Catches a message to a thread that does not exist, a duplicate send that queues a second
    // copy, a different message under a taken id, a message id that is not a uuid and is lost on
    // the way to Claude, and a CLI left running after shutdown.
    let harness = Harness::start(Options::default()).await;
    let missing = harness
        .provider
        .send(
            kratos_protocol::ThreadId::new("missing".to_owned()),
            message_id(A),
            text_input("A"),
            MessageOrigin::User,
        )
        .await;
    assert!(
        matches!(missing, Err(ProviderError::UnknownThread(_))),
        "{missing:?}"
    );

    let first = harness.send(A, "A").await;
    let duplicate = harness.send(A, "A").await;
    assert!(matches!(first, Sent::Queued(_)));
    assert!(matches!(duplicate, Sent::Existing(_)));
    let different = harness
        .provider
        .send(
            harness.thread_id.clone(),
            message_id(A),
            text_input("Different"),
            MessageOrigin::User,
        )
        .await;
    assert!(
        matches!(different, Err(ProviderError::Store(_))),
        "{different:?}"
    );
    let launch = harness.launch(1).await;
    harness.wait_written(&launch, &[A]).await;
    harness.init(&launch).await;
    harness.accept(&launch, A).await;

    harness
        .send("message-B", "B by a name that is no uuid")
        .await;
    harness.finish(&launch, &[A], json!({})).await;
    let native = kratos_engine::providers::claude::native_id(&message_id("message-B")).to_string();
    harness.wait_written(&launch, &[A, &native]).await;
    harness.provider.shutdown().await;
    harness.wait_ended(&launch).await;

    let after = harness
        .provider
        .send(
            harness.thread_id.clone(),
            message_id(C),
            text_input("C"),
            MessageOrigin::User,
        )
        .await;
    assert!(matches!(after, Err(ProviderError::Stopped)), "{after:?}");
}

#[tokio::test]
async fn starts_a_session_with_initialize_first_and_registers_only_kratos_hooks() {
    // Catches a prompt written before Claude is initialized, and an initialize that registers
    // hooks Kratos does not answer or SDK MCP servers it does not serve. The plan recorded without the
    // account (D22, D24) is checked by `turns::records_the_plan_that_pays_for_the_session_and_never_the_account`.
    let harness = Harness::start(Options::default()).await;
    harness.send(A, "A").await;
    let launch = harness.launch(1).await;
    harness.wait_written(&launch, &[A]).await;

    let received = harness.received(&launch).await;

    assert_eq!(received[0]["type"], "control_request");
    assert_eq!(received[0]["request"], crate::harness::initialize());
    // The usage read and the message are written together once Claude is up, in either order.
    let mut after: Vec<_> = received[1..]
        .iter()
        .map(|frame| {
            if frame["type"] == "user" {
                "user".to_owned()
            } else {
                frame["request"].to_string()
            }
        })
        .collect();
    after.sort();
    let read = json!({ "subtype": "get_usage", "skip_behaviors": true }).to_string();
    assert_eq!(after, ["user".to_owned(), read], "nothing else was written");
}

#[tokio::test]
async fn a_line_kratos_cannot_read_ends_the_turn_instead_of_leaving_claude_waiting() {
    // Catches a permission request too large to read, or a result Kratos cannot parse, dropped
    // with a note while the CLI waits for an answer and the turn runs forever.
    let limit = 128 * 1024;
    let oversized = json!({ "type": "control_request", "request_id": "big", "request": {
        "subtype": "can_use_tool", "tool_name": "Write",
        "input": { "file_path": "big.txt", "content": "x".repeat(2 * limit) } } });
    let unparsable = json!({ "type": "result", "subtype": "success", "user_message_uuids": [A] });
    for (label, line, reason) in [
        ("oversized", oversized, "more than Kratos reads"),
        ("unparsable", unparsable, "cannot read"),
    ] {
        let mut harness = Harness::start(Options {
            max_line_bytes: limit,
            ..Options::default()
        })
        .await;
        let launch = harness.begin(A, 1).await;

        harness.emit(&launch, line).await;
        harness.wait_ended(&launch).await;

        let ended = harness
            .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
            .await;
        assert!(
            matches!(&ended, ThreadEventBody::TurnCompleted { outcome: TurnOutcome::Failed { error }, .. } if error.contains(reason)),
            "{label}: {ended:?}"
        );
    }
}

#[tokio::test]
async fn a_transcript_with_a_bad_line_in_the_middle_still_lets_the_user_discard() {
    // Catches a hard error on a line torn by a crash and followed by later rows, which answered
    // every discard with 503 and left the uncertain message holding the queue for good.
    let harness = Harness::start(Options::default()).await;
    let first = harness.begin(A, 1).await;
    harness.send(E, "E").await;
    harness
        .provider
        .steer(harness.thread_id.clone(), message_id(E))
        .await
        .unwrap();
    harness.wait_written(&first, &[A, E]).await;
    let path = kratos_claude_client::transcript_path(
        &harness.claude_home(),
        &harness.folder(),
        first.session(),
    );
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"type\":\"user\",\"uu\n{\"type\":\"assistant\"}\n");
    std::fs::write(&path, text).unwrap();

    harness.exit(&first, 3).await;
    harness
        .message_becomes(E, |s| matches!(s, MessageStatus::Unknown { .. }))
        .await;
    let discarded = harness
        .provider
        .discard(harness.thread_id.clone(), message_id(E))
        .await
        .unwrap();

    assert_eq!(discarded.status, MessageStatus::Cancelled);
}

#[tokio::test]
async fn editing_a_large_file_does_not_end_the_session() {
    // Catches a line limit below what an edit sends: the CLI puts the whole file as it was before
    // the change in the post-tool hook, so editing a 3 MB file (measured) ended the session.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    let original = "x".repeat(3 * 1024 * 1024);
    harness
        .call(
            &launch,
            "test/ask",
            json!({ "requestId": "edited", "request": {
        "subtype": "hook_callback", "callback_id": "kratos-post",
        "input": { "hook_event_name": "PostToolUse", "tool_name": "Edit",
            "tool_input": { "file_path": "big.txt", "old_string": "a", "new_string": "b" },
            "tool_response": { "originalFile": original } } } }),
        )
        .await;

    let deadline = tokio::time::Instant::now() + crate::harness::WAIT;
    loop {
        let answered = harness.received(&launch).await.into_iter().any(|frame| {
            frame["response"]["request_id"] == "edited"
                && frame["response"]["response"]["continue"] == true
        });
        if answered {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the edit hook was never answered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    harness.finish(&launch, &[A], json!({})).await;
    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(
        matches!(
            ended,
            ThreadEventBody::TurnCompleted {
                outcome: TurnOutcome::Completed,
                ..
            }
        ),
        "{ended:?}"
    );
}
