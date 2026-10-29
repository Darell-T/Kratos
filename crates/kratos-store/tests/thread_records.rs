//! The records R3 adds: the message queue, approvals, questions, continuations, thread state, and
//! the migration backup the plan asks to restore.

use std::fs;
use std::path::Path;

use kratos_protocol::{
    Approval, ApprovalDecision, ApprovalId, ApprovalOutcome, ApprovalRequest, AskReason,
    Continuation, ContinuationId, ContinuationState, CreateThread, CreateWorkspace, ExtraAccess,
    MessageId, MessageInput, MessageOrigin, MessageStatus, PauseReason, Permission, Provider,
    Question, QuestionId, QuestionOutcome, RequiredText, Seq, ThreadEventBody, ThreadId,
    ThreadSettings, TurnChanges, TurnId, TurnOutcome, UsageWait,
};
use kratos_store::{Enqueued, QueuePlace, Settle, Store, StoreError, restore};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

const MAX_PENDING: usize = 10;

fn home() -> TempDir {
    tempfile::tempdir().unwrap()
}

fn store_with_thread(home: &Path) -> (Store, ThreadId) {
    let store = Store::open(home).unwrap();
    let workspace: CreateWorkspace =
        serde_json::from_value(json!({ "name": "repo", "path": "/work/repo" })).unwrap();
    let workspace = store.create_workspace(&workspace).unwrap();
    let request: CreateThread = serde_json::from_value(
        json!({ "workspaceId": workspace.id, "title": "chat", "provider": "codex" }),
    )
    .unwrap();
    let thread = store.create_thread(&request).unwrap();
    (store, thread.id)
}

fn text(value: &str) -> Vec<MessageInput> {
    vec![MessageInput::Text {
        text: RequiredText::new(value).unwrap(),
    }]
}

fn id(value: &str) -> MessageId {
    MessageId::new(value.into())
}

fn enqueue(store: &mut Store, thread: &ThreadId, message: &str, place: QueuePlace) -> Enqueued {
    let mut write = store.write_thread(thread).unwrap();
    let enqueued = write
        .enqueue_message(
            &id(message),
            text(message),
            MessageOrigin::User,
            place,
            MAX_PENDING,
        )
        .unwrap();
    write.commit().unwrap();
    enqueued
}

fn set_status(
    store: &mut Store,
    thread: &ThreadId,
    message: &str,
    status: MessageStatus,
) -> Result<(), StoreError> {
    let mut write = store.write_thread(thread).unwrap();
    write.set_message_status(&id(message), status)?;
    write.commit().unwrap();
    Ok(())
}

fn queue(store: &Store, thread: &ThreadId) -> Vec<String> {
    store
        .pending_messages(thread)
        .unwrap()
        .into_iter()
        .map(|message| message.id.to_string())
        .collect()
}

fn kinds(store: &Store, thread: &ThreadId) -> Vec<String> {
    store
        .events_after(thread, Seq::BEFORE_FIRST, usize::MAX)
        .unwrap()
        .into_iter()
        .map(|event| {
            serde_json::to_value(event.event).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn approval(name: &str) -> Approval {
    Approval {
        id: ApprovalId::new(name.into()),
        turn_id: TurnId::new("turn-1".into()),
        request: ApprovalRequest::Command {
            command: "rm -rf build".into(),
            cwd: None,
            reason: None,
            access: ExtraAccess::default(),
        },
        why: AskReason::Dangerous,
    }
}

#[test]
fn a_full_queue_refuses_a_new_message_and_a_resent_id_stores_nothing_new() {
    // Catches a limit checked outside the write that stores the message, which lets an eleventh
    // message in (D85), and a client retry that queues the same message twice.
    let home = home();
    let (mut store, thread) = store_with_thread(home.path());
    for n in 0..MAX_PENDING {
        assert!(matches!(
            enqueue(&mut store, &thread, &format!("m{n}"), QueuePlace::Back),
            Enqueued::Added(_)
        ));
    }
    let events_before = kinds(&store, &thread).len();

    let full = enqueue(&mut store, &thread, "m10", QueuePlace::Back);
    let again = enqueue(&mut store, &thread, "m3", QueuePlace::Back);

    assert_eq!(
        full,
        Enqueued::Full {
            pending: MAX_PENDING
        }
    );
    assert!(matches!(again, Enqueued::Existing(ref message) if message.id == id("m3")));
    assert_eq!(
        kinds(&store, &thread).len(),
        events_before,
        "nothing was recorded"
    );
    assert_eq!(queue(&store, &thread).len(), MAX_PENDING);
    assert!(store.message(&thread, &id("m10")).unwrap().is_none());
}

#[test]
fn a_delivered_message_leaves_room_and_a_continuation_goes_ahead_of_the_waiting_ones() {
    // Catches a limit that counts delivered messages, which would refuse work forever, and a
    // continuation queued behind the messages written after the turn it carries on.
    let home = home();
    let (mut store, thread) = store_with_thread(home.path());
    for message in ["a", "b", "c"] {
        enqueue(&mut store, &thread, message, QueuePlace::Back);
    }
    set_status(&mut store, &thread, "a", MessageStatus::Submitting).unwrap();
    let accepted = MessageStatus::Accepted {
        turn_id: TurnId::new("turn-1".into()),
    };
    set_status(&mut store, &thread, "a", accepted).unwrap();

    enqueue(&mut store, &thread, "first", QueuePlace::Front);

    assert_eq!(queue(&store, &thread), ["first", "b", "c"]);
}

#[test]
fn a_message_whose_delivery_is_uncertain_is_never_queued_again() {
    // Catches a store that lets a caller resend a message the provider may already have acted on
    // (D11). Only the provider's history or the user may settle it.
    let home = home();
    let (mut store, thread) = store_with_thread(home.path());
    enqueue(&mut store, &thread, "m", QueuePlace::Back);
    set_status(&mut store, &thread, "m", MessageStatus::Submitting).unwrap();
    let unknown = MessageStatus::Unknown {
        error: "Codex exited".into(),
    };
    set_status(&mut store, &thread, "m", unknown).unwrap();
    let events_before = kinds(&store, &thread);

    let resend = set_status(&mut store, &thread, "m", MessageStatus::Queued);
    let send_again = set_status(&mut store, &thread, "m", MessageStatus::Submitting);

    for refused in [resend, send_again] {
        assert!(
            matches!(
                refused,
                Err(StoreError::MessageTransition {
                    from: "unknown",
                    ..
                })
            ),
            "{refused:?}"
        );
    }
    assert_eq!(kinds(&store, &thread), events_before);
    set_status(&mut store, &thread, "m", MessageStatus::Cancelled).unwrap();
    assert!(queue(&store, &thread).is_empty());
}

#[test]
fn a_write_that_fails_halfway_leaves_neither_its_change_nor_its_event() {
    // Catches a compound write that commits its first step before a later one fails, which would
    // leave an event that reports a change the records do not hold, or the reverse.
    let home = home();
    let (mut store, thread) = store_with_thread(home.path());

    let mut write = store.write_thread(&thread).unwrap();
    write
        .enqueue_message(
            &id("m"),
            text("m"),
            MessageOrigin::User,
            QueuePlace::Back,
            MAX_PENDING,
        )
        .unwrap();
    let failed = write.set_message_status(&id("missing"), MessageStatus::Submitting);
    drop(write);

    assert!(matches!(failed, Err(StoreError::UnknownMessage { .. })));
    assert!(kinds(&store, &thread).is_empty());
    assert!(store.message(&thread, &id("m")).unwrap().is_none());
}

#[test]
fn the_first_decision_on_an_approval_counts_and_a_later_one_gets_it_back() {
    // Catches a second device overwriting the first decision, or getting an error instead of the
    // recorded outcome (D83), and a close that reopens a decided request.
    let home = home();
    let (mut store, thread) = store_with_thread(home.path());
    let mut write = store.write_thread(&thread).unwrap();
    write.request_approval(approval("a-1")).unwrap();
    write.commit().unwrap();
    let approval_id = ApprovalId::new("a-1".into());

    let mut settle = |settle| {
        let mut write = store.write_thread(&thread).unwrap();
        let outcome = write.settle_approval(&approval_id, settle).unwrap();
        write.commit().unwrap();
        outcome
    };
    let first = settle(Settle::Decide(ApprovalDecision::Decline));
    let second = settle(Settle::Decide(ApprovalDecision::Accept));
    let close = settle(Settle::Close("The turn ended".into()));

    let declined = ApprovalOutcome::Decided {
        decision: ApprovalDecision::Decline,
    };
    assert_eq!(
        [first, second, close],
        [declined.clone(), declined.clone(), declined.clone()]
    );
    assert_eq!(
        kinds(&store, &thread),
        ["approval.requested", "approval.decided"]
    );
    let stored = store.approval(&approval_id).unwrap().unwrap();
    assert_eq!(
        (stored.thread_id, stored.outcome),
        (thread.clone(), declined)
    );
    assert!(store.waiting_approvals(&thread).unwrap().is_empty());
}

#[test]
fn an_approval_of_another_thread_is_unknown_to_this_one() {
    // Catches a decision routed through the wrong thread, which would record it in that thread's
    // log.
    let home = home();
    let (mut store, thread) = store_with_thread(home.path());
    let workspace = store.list_workspaces().unwrap().remove(0);
    let other: CreateThread = serde_json::from_value(
        json!({ "workspaceId": workspace.id, "title": "other", "provider": "codex" }),
    )
    .unwrap();
    let other = store.create_thread(&other).unwrap().id;
    let mut write = store.write_thread(&thread).unwrap();
    write.request_approval(approval("a-1")).unwrap();
    write.commit().unwrap();

    let mut write = store.write_thread(&other).unwrap();
    let wrong = write.settle_approval(
        &ApprovalId::new("a-1".into()),
        Settle::Decide(ApprovalDecision::Accept),
    );

    assert!(matches!(wrong, Err(StoreError::UnknownApproval(_))));
}

#[test]
fn an_answered_question_keeps_no_answer_text() {
    // Catches answers written to the records, where a secret answer (a password) would stay.
    let home = home();
    let (mut store, thread) = store_with_thread(home.path());
    let question = Question {
        id: QuestionId::new("q-1".into()),
        turn_id: TurnId::new("turn-1".into()),
        items: Vec::new(),
        blocking: true,
    };
    let mut write = store.write_thread(&thread).unwrap();
    write.ask_question(question).unwrap();
    let outcome = write
        .settle_question(&QuestionId::new("q-1".into()), Settle::Decide(()))
        .unwrap();
    write.commit().unwrap();

    assert_eq!(outcome, QuestionOutcome::Answered);
    let database = Connection::open(home.path().join("kratos.db")).unwrap();
    let columns: Vec<String> = database
        .prepare("SELECT name FROM pragma_table_info('questions')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|name| name.unwrap())
        .collect();
    assert!(
        !columns
            .iter()
            .any(|name| name.contains("answer") && name != "answered")
    );
}

fn continuation(name: &str, state: ContinuationState) -> Continuation {
    Continuation {
        id: ContinuationId::new(name.into()),
        turn_id: TurnId::new("turn-1".into()),
        waits_on: UsageWait {
            provider: Provider::Codex,
            account: Some("acct".into()),
            limit_ids: vec!["codex".into()],
        },
        resets_at: None,
        state,
    }
}

#[test]
fn a_thread_has_one_open_continuation_and_sending_it_queues_its_message_once() {
    // Catches two schedulers' timers each planning a continuation for one stop, and a send that
    // queues the message but leaves the continuation open, so a restart would send it again
    // (D39).
    let home = home();
    let (mut store, thread) = store_with_thread(home.path());
    enqueue(&mut store, &thread, "later", QueuePlace::Back);
    let mut write = store.write_thread(&thread).unwrap();
    write
        .pause(PauseReason::UsageLimit {
            turn_id: TurnId::new("turn-1".into()),
            resets_at: None,
        })
        .unwrap();
    write
        .add_continuation(continuation("c-1", ContinuationState::Scheduled))
        .unwrap();
    write.commit().unwrap();

    let mut write = store.write_thread(&thread).unwrap();
    let mut another = continuation("c-2", ContinuationState::Scheduled);
    another.turn_id = TurnId::new("turn-2".into());
    let second = write.add_continuation(another);
    drop(write);
    let send = |store: &mut Store| {
        let mut write = store.write_thread(&thread).unwrap();
        let sent = write
            .send_continuation(
                &ContinuationId::new("c-1".into()),
                &id("continue"),
                text("continue"),
                MAX_PENDING,
            )
            .unwrap();
        write.commit().unwrap();
        sent
    };
    let first_send = send(&mut store);
    let second_send = send(&mut store);

    assert!(matches!(second, Err(StoreError::Sqlite(_))), "{second:?}");
    assert!(matches!(first_send, Some(Enqueued::Added(_))));
    assert_eq!(second_send, None, "a sent continuation sends nothing more");
    assert_eq!(queue(&store, &thread), ["continue", "later"]);
    let message = store.message(&thread, &id("continue")).unwrap().unwrap();
    assert_eq!(
        message.origin,
        MessageOrigin::Continuation {
            continuation_id: ContinuationId::new("c-1".into())
        }
    );
    assert!(store.open_continuations().unwrap().is_empty());
}

#[test]
fn a_sent_or_cancelled_source_turn_cannot_get_another_continuation() {
    for terminal in [
        ContinuationState::Sent {
            message_id: id("already-sent"),
        },
        ContinuationState::Cancelled {
            reason: "user resumed".into(),
        },
    ] {
        let home = home();
        let (mut store, thread) = store_with_thread(home.path());
        let mut write = store.write_thread(&thread).unwrap();
        write
            .pause(PauseReason::UsageLimit {
                turn_id: TurnId::new("turn-1".into()),
                resets_at: None,
            })
            .unwrap();
        assert!(
            write
                .add_continuation(continuation("old", terminal.clone()))
                .unwrap()
        );
        write.commit().unwrap();
        drop(store);

        let mut store = Store::open(home.path()).unwrap();
        assert!(
            store
                .unplanned_usage_stops(Provider::Codex)
                .unwrap()
                .is_empty()
        );
        let mut write = store.write_thread(&thread).unwrap();
        assert!(
            !write
                .add_continuation(continuation("late-duplicate", ContinuationState::Scheduled))
                .unwrap(),
            "{terminal:?}"
        );
        assert!(write.commit().unwrap().is_empty());
        assert!(
            store
                .continuation(&ContinuationId::new("late-duplicate".into()))
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn a_continuation_cannot_send_after_its_source_pause_changes() {
    for pause in [
        None,
        Some(PauseReason::Interrupted),
        Some(PauseReason::UsageLimit {
            turn_id: TurnId::new("another-turn".into()),
            resets_at: None,
        }),
    ] {
        let home = home();
        let (mut store, thread) = store_with_thread(home.path());
        let mut write = store.write_thread(&thread).unwrap();
        write
            .add_continuation(continuation("c-1", ContinuationState::Scheduled))
            .unwrap();
        if let Some(pause) = pause {
            write.pause(pause.clone()).unwrap();
        }
        write.commit().unwrap();
        let before = store.provider_thread(&thread).unwrap().paused;

        let mut write = store.write_thread(&thread).unwrap();
        assert_eq!(
            write
                .send_continuation(
                    &ContinuationId::new("c-1".into()),
                    &id("stale"),
                    text("continue"),
                    MAX_PENDING
                )
                .unwrap(),
            None
        );
        assert!(write.commit().unwrap().is_empty());
        assert!(queue(&store, &thread).is_empty());
        assert_eq!(store.provider_thread(&thread).unwrap().paused, before);
    }
}

#[test]
fn settings_pauses_and_running_turns_survive_reopening() {
    // Catches state kept only in memory: after a restart the engine must know a turn was running
    // (L1), why the queue waits, and what the next turn runs with (D87).
    let home = home();
    let (mut store, thread) = store_with_thread(home.path());
    let settings = ThreadSettings {
        model: Some("gpt-test".into()),
        permission: Permission::FullAccess,
        plan_mode: true,
        context_limit_tokens: Some(100_000),
    };
    let reason = PauseReason::TimedOut {
        turn_id: TurnId::new("turn-0".into()),
    };
    let mut write = store.write_thread(&thread).unwrap();
    write.set_settings(settings.clone()).unwrap();
    assert!(write.pause(reason.clone()).unwrap());
    assert!(
        !write.pause(reason.clone()).unwrap(),
        "the same pause again records nothing"
    );
    write.start_turn(TurnId::new("turn-1".into())).unwrap();
    write.commit().unwrap();
    drop(store);

    let mut store = Store::open(home.path()).unwrap();
    let saved = store.provider_thread(&thread).unwrap();
    assert_eq!(saved.settings, settings);
    assert_eq!(saved.paused, Some(reason));
    assert_eq!(
        store.threads_with_running_turns().unwrap(),
        [(thread.clone(), TurnId::new("turn-1".into()))]
    );

    let mut write = store.write_thread(&thread).unwrap();
    let changes = TurnChanges {
        files: Vec::new(),
        edited_since_command: false,
        ran_command: false,
    };
    write
        .complete_turn(
            TurnId::new("turn-1".into()),
            TurnOutcome::Interrupted,
            changes,
            None,
        )
        .unwrap();
    assert!(write.resume().unwrap());
    assert!(
        !write.resume().unwrap(),
        "resuming a running queue records nothing"
    );
    write.commit().unwrap();

    assert!(store.threads_with_running_turns().unwrap().is_empty());
    assert_eq!(store.provider_thread(&thread).unwrap().paused, None);
    assert_eq!(
        kinds(&store, &thread),
        [
            "settings.changed",
            "thread.paused",
            "turn.started",
            "turn.completed",
            "thread.resumed"
        ]
    );
}

#[test]
fn a_restored_migration_backup_is_the_database_the_typescript_engine_wrote() {
    // The plan asks to restore the backup taken before migration 5 and check that the reference
    // engine opens it. Catches a backup taken after migrating, or one that lost rows, either of
    // which would leave a user who goes back to the TypeScript engine with a database it refuses
    // or that is missing work.
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let home = home();
    let live = home.path().join("kratos.db");
    fs::copy(fixtures.join("typescript-v4.db"), &live).unwrap();
    let written_by_typescript = dump(&fixtures.join("typescript-v4.db"));

    let mut store = Store::open(home.path()).unwrap();
    let backup = store.backup_taken().unwrap().clone();
    let thread = store.list_workspaces().unwrap()[0].id.clone();
    let thread = store.list_threads(&thread).unwrap()[0].id.clone();
    store
        .append_event(
            &thread,
            &ThreadEventBody::UserMessage {
                text: "written by migration 5".into(),
            },
        )
        .unwrap();
    drop(store);
    restore(home.path(), &backup.id).unwrap();

    assert_eq!(dump(&live), written_by_typescript);
}

/// The schema text, user_version, and every row, as text.
fn dump(database: &Path) -> Vec<String> {
    let connection = Connection::open(database).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let mut lines = vec![format!("user_version = {version}")];
    let tables: Vec<(String, Option<String>)> = connection
        .prepare("SELECT name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    for (name, sql) in tables {
        lines.push(format!("{name}: {sql:?}"));
        let Some(sql) = sql else { continue };
        if !sql.starts_with("CREATE TABLE") {
            continue;
        }
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {name}"))
            .unwrap();
        let width = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                (0..width)
                    .map(|index| row.get::<_, rusqlite::types::Value>(index))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap();
        lines.extend(rows.map(|row| format!("{name} row: {:?}", row.unwrap())));
    }
    lines
}
