//! What Codex reports during a turn: the log keeps finished items, errors, and how the turn ended,
//! and live watchers get the reply as it is written (D88).

use std::time::Duration;

use kratos_protocol::{AgentItem, LiveUpdate, ReplySoFar, ThreadEventBody, TurnChanges};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::harness::{Harness, Options, WAIT};

const THREAD: &str = "thread-A";

async fn next_live(updates: &mut broadcast::Receiver<LiveUpdate>) -> LiveUpdate {
    tokio::time::timeout(WAIT, updates.recv())
        .await
        .expect("a live update arrives")
        .unwrap()
}

fn delta(text: &str) -> Value {
    json!({ "threadId": THREAD, "turnId": "turn-1", "itemId": "reply-1", "delta": text })
}

fn completed(item: Value) -> Value {
    json!({ "threadId": THREAD, "turnId": "turn-1", "completedAtMs": 0, "item": item })
}

fn command_item(id: &str) -> Value {
    json!({ "type": "commandExecution", "id": id, "command": "cargo test", "commandActions": [],
            "cwd": "/work", "status": "completed", "exitCode": 0, "aggregatedOutput": "ok" })
}

fn patch_item(id: &str, path: &str) -> Value {
    json!({ "type": "fileChange", "id": id, "status": "completed",
            "changes": [{ "path": path, "kind": { "type": "update" }, "diff": "+x" }] })
}

#[tokio::test]
async fn reply_text_streams_live_and_a_late_watcher_gets_it_whole() {
    // Catches deltas written to the log, which D88 keeps out of it, and a watcher who opens the
    // thread mid-reply seeing only the text written after they arrived.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    let (_, mut early) = harness.hub.watch_live(&harness.thread_id);

    harness
        .emit("item/agentMessage/delta", delta("Hello"))
        .await;
    harness
        .emit("item/agentMessage/delta", delta(" world"))
        .await;

    for expected in ["Hello", " world"] {
        let update = next_live(&mut early).await;
        assert!(
            matches!(update, LiveUpdate::ReplyDelta { ref delta, .. } if delta == expected),
            "{update:?}"
        );
    }
    let (snapshot, _) = harness.hub.watch_live(&harness.thread_id);
    let Some(LiveUpdate::Snapshot { replies, .. }) = snapshot else {
        panic!("a running turn's reply comes as a snapshot: {snapshot:?}");
    };
    assert_eq!(
        replies,
        [ReplySoFar {
            item_id: "reply-1".into(),
            text: "Hello world".into()
        }]
    );

    harness.complete(THREAD, "turn-1", "completed").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(
        harness.hub.watch_live(&harness.thread_id).0.is_none(),
        "the log holds the reply now"
    );
}

#[tokio::test]
async fn finished_items_are_logged_and_the_turn_reports_what_it_changed() {
    // Catches a turn end that loses the files it edited, or that reports an edit as checked when
    // no command ran after it.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;

    harness
        .emit(
            "item/completed",
            completed(patch_item("patch-1", "src/a.rs")),
        )
        .await;
    harness
        .emit("item/completed", completed(command_item("cmd-1")))
        .await;
    harness
        .emit(
            "item/completed",
            completed(patch_item("patch-2", "src/b.rs")),
        )
        .await;
    harness
        .emit(
            "item/completed",
            completed(json!({ "type": "agentMessage", "id": "reply-1", "text": "Done." })),
        )
        .await;
    harness.complete(THREAD, "turn-1", "completed").await;

    let mut items = Vec::new();
    let changes = loop {
        match harness.next_event().await.event {
            ThreadEventBody::ItemCompleted { item, .. } => items.push(item),
            ThreadEventBody::TurnCompleted { changes, .. } => break changes,
            _ => {}
        }
    };
    assert_eq!(items.len(), 4);
    assert!(matches!(&items[3], AgentItem::Message { text, .. } if text == "Done."));
    assert_eq!(
        changes,
        TurnChanges {
            files: vec!["src/a.rs".into(), "src/b.rs".into()],
            edited_since_command: true,
            ran_command: true,
        }
    );
}

#[tokio::test]
async fn a_provider_error_is_logged_against_its_turn() {
    // Catches Codex's own retries and failures hidden from the user.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;

    harness
        .emit(
            "error",
            json!({ "threadId": THREAD, "turnId": "turn-1", "willRetry": true,
                    "error": { "message": "stream disconnected" } }),
        )
        .await;

    let error = harness
        .wait_for(|event| matches!(event, ThreadEventBody::ProviderError { .. }))
        .await;
    assert!(matches!(
        error,
        ThreadEventBody::ProviderError { ref message, will_retry: true, .. } if message == "stream disconnected"
    ));
}

#[tokio::test]
async fn context_use_reaches_watchers_live_and_the_log_at_the_turn_s_end() {
    // Catches context figures lost at the turn's end, which the context limit setting needs (D76).
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    let (_, mut updates) = harness.hub.watch_live(&harness.thread_id);
    let breakdown = json!({ "cachedInputTokens": 10, "inputTokens": 100, "outputTokens": 20,
                            "reasoningOutputTokens": 5, "totalTokens": 120 });

    harness
        .emit(
            "thread/tokenUsage/updated",
            json!({ "threadId": THREAD, "turnId": "turn-1",
                    "tokenUsage": { "last": breakdown, "total": breakdown, "modelContextWindow": 200000 } }),
        )
        .await;
    let live = next_live(&mut updates).await;
    assert!(
        matches!(live, LiveUpdate::Context { ref usage, .. } if usage.context_window == Some(200_000))
    );
    harness.complete(THREAD, "turn-1", "completed").await;

    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let ThreadEventBody::TurnCompleted { context, .. } = ended else {
        unreachable!("wait_for picked the turn's end");
    };
    assert_eq!(context.map(|context| context.total_tokens), Some(120));
}

#[tokio::test]
async fn a_completion_repeated_for_a_finished_turn_changes_nothing() {
    // Catches a duplicate completion ending the next turn, which would start a third message while
    // the second still runs.
    let mut harness = Harness::start(Options::default()).await;
    harness.running_turn("a", "turn-1").await;
    harness.complete(THREAD, "turn-1", "completed").await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    harness.running_turn("b", "turn-2").await;

    harness.complete(THREAD, "turn-1", "completed").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        harness
            .details()
            .await
            .running_turn
            .map(|turn| turn.as_str().to_owned()),
        Some("turn-2".to_owned())
    );
}
