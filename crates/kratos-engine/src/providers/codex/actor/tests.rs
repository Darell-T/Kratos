//! Controlled delivery of real fake-Codex replies across process exit and replacement.

use std::time::Duration;

use kratos_command_safety::DangerousCommandPlatform;
use kratos_protocol::{
    CreateThread, CreateWorkspace, MessageOrigin, MessageStatus, RequiredText, TurnId,
};
use kratos_store::Store;
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::providers::codex::{ProtectedFolders, queue};
use crate::store_thread;

const WAIT: Duration = Duration::from_secs(20);

struct Harness {
    actor: Actor,
    thread_id: ThreadId,
    root: TempDir,
    store_runner: std::thread::JoinHandle<()>,
}

impl Harness {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("kratos")).unwrap();
        let request: CreateWorkspace = serde_json::from_value(json!({
            "name": "project", "path": root.path(),
        }))
        .unwrap();
        let workspace = store.create_workspace(&request).unwrap();
        let request: CreateThread = serde_json::from_value(json!({
            "workspaceId": workspace.id, "title": "chat", "provider": "codex",
        }))
        .unwrap();
        let thread_id = store.create_thread(&request).unwrap().id;
        let (store_thread, jobs) = store_thread::channel();
        let store_runner = std::thread::spawn(move || jobs.run(store));
        let hub = Hub::new(store_thread.clone());
        let settings = CodexSettings {
            program: fake_agents::executable("fake-codex"),
            extra_args: Vec::new(),
            env_set: Vec::new(),
            server_folder: root.path().to_owned(),
            protected: ProtectedFolders::new(root.path().join("kratos"), None, None),
            turn_timeout: Duration::from_secs(600),
            stop_timeout: Duration::from_secs(5),
            request_timeout: WAIT,
            platform: DangerousCommandPlatform::Posix,
            mcp: None,
        };
        let (limit_stops, _) = mpsc::unbounded_channel();
        let (actor, _) = Actor::recover(
            settings,
            hub,
            store_thread,
            UsageTracker::default(),
            limit_stops,
        )
        .await
        .unwrap();
        Self {
            actor,
            thread_id,
            root,
            store_runner,
        }
    }

    async fn attached_reply(&mut self) -> Completion {
        self.attached_reply_for(&self.thread_id.clone()).await
    }

    async fn attached_reply_for(&mut self, thread_id: &ThreadId) -> Completion {
        loop {
            self.actor.attached(thread_id).await.unwrap();
            let done = self
                .completion(|done| matches!(done, Done::Connected { .. } | Done::Attached { .. }))
                .await;
            if matches!(done.done, Done::Attached { .. }) {
                return done;
            }
            done.apply(&mut self.actor).await;
        }
    }

    async fn send(&mut self, thread_id: &ThreadId, message_id: &MessageId) {
        self.actor
            .send(
                thread_id,
                message_id.clone(),
                vec![MessageInput::Text {
                    text: RequiredText::new("work").unwrap(),
                }],
                MessageOrigin::User,
            )
            .await
            .unwrap();
    }

    async fn completion(&mut self, matches: impl Fn(&Done) -> bool) -> Completion {
        loop {
            let completion = tokio::time::timeout(WAIT, self.actor.completions.recv())
                .await
                .unwrap()
                .unwrap();
            if matches(&completion.done) {
                return completion;
            }
            completion.apply(&mut self.actor).await;
        }
    }

    async fn exit(&mut self) {
        let Server::Up { codex, .. } = &self.actor.server else {
            panic!("the fake is connected");
        };
        codex.close().await;
        loop {
            let incoming = tokio::time::timeout(WAIT, next_incoming(&mut self.actor.server))
                .await
                .unwrap();
            let exited = matches!(incoming, Incoming::Exited(_));
            self.actor.on_incoming(incoming).await;
            if exited {
                return;
            }
        }
    }

    async fn control(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let Server::Up { codex, .. } = &self.actor.server else {
            panic!("the fake is connected");
        };
        codex.request_value(method, params, WAIT).await.unwrap()
    }

    async fn held(&self, method: &str) {
        tokio::time::timeout(WAIT, async {
            loop {
                let held = self.control("test/turnRequests", json!({})).await;
                if held
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|request| request["method"] == method)
                {
                    return;
                }
            }
        })
        .await
        .expect("the fake holds the request");
    }

    async fn close(mut self) {
        self.actor.shutdown().await;
        drop(self.actor);
        tokio::task::spawn_blocking(move || {
            self.store_runner.join().unwrap();
            drop(self.root);
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn resume_and_continuation_name_the_action_for_the_blocking_message() {
    // Catches resume or continuation suggesting actions that cannot settle the blocking message.
    for (status, expected) in [
        (
            MessageStatus::Failed {
                error: "refused".into(),
            },
            "Retry or remove message held",
        ),
        (
            MessageStatus::Unknown {
                error: "lost".into(),
            },
            "Discard message held",
        ),
    ] {
        let mut harness = Harness::start().await;
        let message_id = MessageId::new("held".into());
        harness.send(&harness.thread_id.clone(), &message_id).await;
        harness
            .actor
            .write(&harness.thread_id, move |write| {
                write.set_message_status(&message_id, MessageStatus::Submitting)?;
                write.set_message_status(&message_id, status)
            })
            .await
            .unwrap();

        let result = harness.actor.resume(&harness.thread_id).await;
        let continued = harness
            .actor
            .continue_thread(
                &harness.thread_id,
                kratos_protocol::ContinuationId::new("held-continuation".into()),
            )
            .await
            .unwrap();

        harness.close().await;
        let Err(ProviderError::Conflict(error)) = result else {
            panic!("a held message must refuse resumption")
        };
        assert_eq!(error, format!("{expected} before resuming"));
        assert!(
            matches!(continued, super::super::Continued::NeedsUser(ref why) if why == expected),
            "{continued:?}"
        );
    }
}

#[tokio::test]
async fn an_untouched_thread_waiting_for_a_slot_runs_after_codex_restarts() {
    // Catches exit-time pauses stranding work that lost neither a start nor a running turn.
    let mut harness = Harness::start().await;
    let mut threads = vec![harness.thread_id.clone()];
    let more = harness
        .actor
        .store
        .call(|store| {
            let workspace = store.list_workspaces().unwrap().remove(0).id;
            (0..MAX_STARTING)
                .map(|index| {
                    let request: CreateThread = serde_json::from_value(json!({
                "workspaceId": workspace, "title": format!("chat-{index}"), "provider": "codex",
            })).unwrap();
                    store.create_thread(&request).unwrap().id
                })
                .collect::<Vec<_>>()
        })
        .await;
    threads.extend(more);
    let message_id = MessageId::new("waiting".into());
    for thread_id in &threads {
        harness.send(thread_id, &message_id).await;
        let attached = harness.attached_reply_for(thread_id).await;
        attached.apply(&mut harness.actor).await;
        harness.actor.dispatch(thread_id).await;
    }
    tokio::time::timeout(WAIT, async {
        loop {
            let requests = harness.control("test/turnRequests", json!({})).await;
            if requests
                .as_array()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "turn/start")
                .count()
                == MAX_STARTING
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let waiting_thread = threads.last().unwrap().clone();
    assert_eq!(
        harness
            .actor
            .message(&waiting_thread, &message_id)
            .await
            .unwrap()
            .status,
        MessageStatus::Queued
    );
    harness.actor.restarts.settled_start();

    harness.exit().await;

    let started = tokio::time::timeout(WAIT, async {
        loop {
            if harness
                .actor
                .message(&waiting_thread, &message_id)
                .await
                .unwrap()
                .status
                == MessageStatus::Submitting
            {
                break;
            }
            let done = harness.completion(|_| true).await;
            done.apply(&mut harness.actor).await;
        }
    })
    .await;
    if started.is_ok() {
        harness.held("turn/start").await;
    }
    harness.close().await;
    assert!(
        started.is_ok(),
        "the extra queued thread never reached the replacement Codex"
    );
}

#[tokio::test]
async fn an_unavailable_connection_keeps_an_existing_usage_pause() {
    // Catches reconnect refusal overwriting the usage pause that owns a continuation.
    let mut harness = Harness::start().await;
    let thread_id = harness.thread_id.clone();
    harness
        .send(&thread_id, &MessageId::new("waiting".into()))
        .await;
    let pause = kratos_protocol::PauseReason::UsageLimit {
        turn_id: TurnId::new("limited".into()),
        resets_at: None,
    };
    let expected = pause.clone();
    harness
        .actor
        .write(&thread_id, move |write| write.pause(pause))
        .await
        .unwrap();

    harness
        .actor
        .on_connected(Err(StartError::Connect(
            kratos_codex_client::ConnectError::NotFound("missing-codex".into()),
        )))
        .await;

    let paused = harness
        .actor
        .store
        .call(move |store| store.thread_details(&thread_id).unwrap().paused)
        .await;
    harness.close().await;
    assert_eq!(paused, Some(expected));
}

#[tokio::test]
async fn an_attach_reply_after_exit_does_not_load_a_dead_process() {
    // Catches a queued attach success loading a thread after its Codex process exited.
    let mut harness = Harness::start().await;
    let old_reply = harness.attached_reply().await;

    harness.exit().await;
    old_reply.apply(&mut harness.actor).await;

    let thread_id = harness.thread_id.clone();
    let native_id = harness
        .actor
        .store
        .call(move |store| store.provider_thread(&thread_id).unwrap().native_id)
        .await;
    harness.close().await;
    assert_eq!(
        native_id, None,
        "the dead process cannot begin a provider session"
    );
}

#[tokio::test]
async fn an_old_attach_reply_does_not_detach_a_replacement_process() {
    // Catches an obsolete attach reply detaching a replacement and causing another resume.
    let mut harness = Harness::start().await;
    let old_reply = harness.attached_reply().await;
    harness.exit().await;
    harness.actor.restarts = Default::default();
    let new_reply = harness.attached_reply().await;
    new_reply.apply(&mut harness.actor).await;
    let native_id = harness.actor.threads[&harness.thread_id]
        .native_id()
        .unwrap()
        .to_owned();

    old_reply.apply(&mut harness.actor).await;

    let loaded = harness.actor.threads[&harness.thread_id]
        .native_id()
        .map(str::to_owned);
    let attached = harness.actor.attached(&harness.thread_id).await.unwrap();
    harness.close().await;
    assert_eq!(loaded, Some(native_id.clone()));
    assert_eq!(attached, Some(native_id));
}

#[tokio::test]
async fn a_quick_exit_pauses_waiting_attachment_under_restart_backoff() {
    // Catches an exit inside SETTLED_AFTER leaving queued work without the backoff's pause.
    let mut harness = Harness::start().await;
    let message_id = MessageId::new("waiting".into());
    harness.send(&harness.thread_id.clone(), &message_id).await;
    let old_reply = harness.attached_reply().await;

    harness.exit().await;
    old_reply.apply(&mut harness.actor).await;

    let message = harness
        .actor
        .message(&harness.thread_id, &message_id)
        .await
        .unwrap();
    let thread_id = harness.thread_id.clone();
    let details = harness
        .actor
        .store
        .call(move |store| store.thread_details(&thread_id))
        .await
        .unwrap();
    let thread_id = harness.thread_id.clone();
    let native_id = harness
        .actor
        .store
        .call(move |store| store.provider_thread(&thread_id).unwrap().native_id)
        .await;
    harness.close().await;
    assert_eq!(message.status, MessageStatus::Queued);
    assert!(
        matches!(
            details.paused,
            Some(kratos_protocol::PauseReason::Unavailable { .. })
        ),
        "{:?}",
        details.paused
    );
    assert_eq!(details.running_turn, None);
    assert_eq!(
        native_id, None,
        "the stale attach cannot begin a provider session"
    );
}

#[tokio::test]
async fn exit_settles_a_pending_start_and_its_stop_without_waiting_for_a_reply() {
    // Catches an unanswered start blocking replacement work or leaving its stop unanswered after exit.
    let mut harness = Harness::start().await;
    let attached = harness.attached_reply().await;
    attached.apply(&mut harness.actor).await;
    let message_id = MessageId::new("pending".into());
    harness.send(&harness.thread_id.clone(), &message_id).await;
    harness.actor.dispatch(&harness.thread_id).await;
    harness.held("turn/start").await;
    let (reply, mut stopped) = oneshot::channel();
    harness.actor.stop(&harness.thread_id, reply).await;

    harness.exit().await;

    let message = harness
        .actor
        .message(&harness.thread_id, &message_id)
        .await
        .unwrap();
    let stopped = stopped.try_recv();
    harness
        .actor
        .write(&harness.thread_id, move |write| {
            write.set_message_status(&message_id, MessageStatus::Cancelled)?;
            write.resume()
        })
        .await
        .unwrap();
    harness.actor.restarts = Default::default();
    let replacement = MessageId::new("after-exit".into());
    harness.send(&harness.thread_id.clone(), &replacement).await;
    let attached = harness.attached_reply().await;
    attached.apply(&mut harness.actor).await;
    harness.held("turn/start").await;
    let requests = harness.control("test/turnRequests", json!({})).await;
    harness.close().await;
    assert!(
        matches!(message.status, MessageStatus::Unknown { .. }),
        "{:?}",
        message.status
    );
    assert!(matches!(stopped, Ok(Ok(()))), "{stopped:?}");
    assert!(
        requests
            .as_array()
            .unwrap()
            .iter()
            .any(|request| request["method"] == "turn/start"
                && request["params"]["clientUserMessageId"] == replacement.as_str()),
        "{requests:?}"
    );
}

#[tokio::test]
async fn a_start_reply_after_exit_cannot_accept_a_message_or_start_a_turn() {
    // Catches a queued start success accepting delivery and reviving a turn after process exit.
    let mut harness = Harness::start().await;
    let attached = harness.attached_reply().await;
    attached.apply(&mut harness.actor).await;
    let message_id = MessageId::new("pending".into());
    harness.send(&harness.thread_id.clone(), &message_id).await;
    harness.actor.dispatch(&harness.thread_id).await;
    harness.held("turn/start").await;
    harness.control("test/settleTurn", json!({
        "method": "turn/start", "result": { "turn": { "id": "old-turn", "items": [], "status": "inProgress" } },
    })).await;
    let old_reply = harness
        .completion(|done| matches!(done, Done::StartAnswered { .. }))
        .await;

    harness.exit().await;
    old_reply.apply(&mut harness.actor).await;

    let message = harness
        .actor
        .message(&harness.thread_id, &message_id)
        .await
        .unwrap();
    let thread_id = harness.thread_id.clone();
    let details = harness
        .actor
        .store
        .call(move |store| store.thread_details(&thread_id))
        .await
        .unwrap();
    harness.close().await;
    assert!(
        matches!(message.status, MessageStatus::Unknown { .. }),
        "{:?}",
        message.status
    );
    assert_eq!(details.running_turn, None);
}

async fn pending_steer() -> (Harness, MessageId) {
    let mut harness = Harness::start().await;
    let attached = harness.attached_reply().await;
    attached.apply(&mut harness.actor).await;
    let first = MessageId::new("first".into());
    harness.send(&harness.thread_id.clone(), &first).await;
    harness.actor.dispatch(&harness.thread_id).await;
    harness.held("turn/start").await;
    harness.control("test/settleTurn", json!({
        "method": "turn/start", "result": { "turn": { "id": "old-turn", "items": [], "status": "inProgress" } },
    })).await;
    harness
        .completion(|done| matches!(done, Done::StartAnswered { .. }))
        .await
        .apply(&mut harness.actor)
        .await;
    let message_id = MessageId::new("steer".into());
    harness.send(&harness.thread_id.clone(), &message_id).await;
    harness
        .actor
        .steer(&harness.thread_id, message_id.clone())
        .await
        .unwrap();
    harness.held("turn/steer").await;

    (harness, message_id)
}

#[tokio::test]
async fn exit_settles_a_pending_steer_without_waiting_for_a_reply() {
    // Catches process fencing leaving a steered message submitting and blocking the thread.
    let (mut harness, message_id) = pending_steer().await;

    harness.exit().await;

    let message = harness
        .actor
        .message(&harness.thread_id, &message_id)
        .await
        .unwrap();
    let thread_id = harness.thread_id.clone();
    let running = harness
        .actor
        .store
        .call(move |store| store.thread_details(&thread_id).unwrap().running_turn)
        .await;
    harness.close().await;
    assert!(
        matches!(message.status, MessageStatus::Unknown { .. }),
        "{:?}",
        message.status
    );
    assert_eq!(running, None, "process exit ends the steered turn");
}

#[tokio::test]
async fn exit_settlement_rolls_back_the_turn_if_the_steer_cannot_be_settled() {
    // Catches a failed uncertain-message write leaving an already committed turn end behind.
    let (mut harness, message_id) = pending_steer().await;
    rusqlite::Connection::open(harness.root.path().join("kratos/kratos.db"))
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_uncertain BEFORE UPDATE OF status ON thread_messages
             WHEN NEW.status = 'unknown' BEGIN SELECT RAISE(ABORT, 'refuse uncertain'); END;",
        )
        .unwrap();

    harness.exit().await;

    let thread_id = harness.thread_id.clone();
    let details = harness
        .actor
        .store
        .call(move |store| store.thread_details(&thread_id))
        .await
        .unwrap();
    let message = harness
        .actor
        .message(&harness.thread_id, &message_id)
        .await
        .unwrap();
    harness.close().await;
    assert_eq!(details.running_turn, Some(TurnId::new("old-turn".into())));
    assert_eq!(details.paused, None);
    assert_eq!(message.status, MessageStatus::Submitting);
}

#[tokio::test]
async fn a_finished_command_answers_without_an_actor_update() {
    // Catches a true caller-only result held behind the actor's state-change fence.
    let mut harness = Harness::start().await;
    let attached = harness.attached_reply().await;
    attached.apply(&mut harness.actor).await;
    let (reply, ran) = oneshot::channel();
    harness
        .actor
        .on_command(Command::Run {
            thread_id: harness.thread_id.clone(),
            command: vec!["echo".into(), "work".into()],
            timeout: WAIT,
            reply,
        })
        .await;
    harness.held("command/exec").await;
    harness.control("test/settleTurn", json!({
        "method": "command/exec", "result": { "exitCode": 0, "stdout": "work", "stderr": "" },
    })).await;

    let result = tokio::time::timeout(WAIT, ran).await;
    harness.close().await;

    let result = result
        .expect("a command result does not require an actor update")
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        json!({"kind": "exited", "exitCode": 0, "stdout": "work", "stderr": ""})
    );
}

#[tokio::test]
async fn a_late_steer_cannot_close_a_replacement_process() {
    // Catches a dead process's steer acting on a replacement turn and closing its live Codex.
    let (mut harness, message_id) = pending_steer().await;
    harness
        .control(
            "test/settleTurn",
            json!({ "method": "turn/steer", "result": { "turnId": "old-turn" } }),
        )
        .await;
    let old_reply = harness
        .completion(|done| matches!(done, Done::Steered { .. }))
        .await;
    harness.exit().await;
    harness.actor.restarts = Default::default();
    let settled = message_id.clone();
    harness
        .actor
        .write(&harness.thread_id, move |write| {
            write.set_message_status(&settled, MessageStatus::Cancelled)?;
            write.resume()
        })
        .await
        .unwrap();
    let replacement = MessageId::new("replacement".into());
    harness.send(&harness.thread_id.clone(), &replacement).await;
    let attached = harness.attached_reply().await;
    attached.apply(&mut harness.actor).await;
    harness.held("turn/start").await;
    harness
        .control(
            "test/settleTurn",
            json!({ "method": "turn/start", "result": {
        "turn": { "id": "replacement-turn", "items": [], "status": "inProgress" }
    } }),
        )
        .await;
    harness
        .completion(|done| matches!(done, Done::StartAnswered { .. }))
        .await
        .apply(&mut harness.actor)
        .await;
    let pid = harness
        .control("test/pid", json!({}))
        .await
        .as_u64()
        .unwrap() as u32;
    rusqlite::Connection::open(harness.root.path().join("kratos/kratos.db"))
        .unwrap()
        .execute_batch("DROP TABLE thread_messages")
        .unwrap();

    old_reply.apply(&mut harness.actor).await;

    let running = fake_agents::process_is_running(pid);
    harness.close().await;
    assert!(running, "the old steer closed replacement process {pid}");
}

#[tokio::test]
async fn a_late_stop_answers_its_waiter() {
    // Catches a stop waiter abandoned or answered by the dead process's reply.
    let (mut harness, _) = pending_steer().await;
    let (reply, stopped) = oneshot::channel();
    harness.actor.stop(&harness.thread_id, reply).await;
    harness.held("turn/interrupt").await;
    harness
        .control(
            "test/settleTurn",
            json!({ "method": "turn/interrupt", "code": -32000, "error": "old interrupt refused" }),
        )
        .await;
    let old_reply = harness
        .completion(|done| matches!(done, Done::Interrupted { .. }))
        .await;

    harness.exit().await;
    old_reply.apply(&mut harness.actor).await;

    let result = stopped.await.unwrap();
    harness.close().await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn a_late_delivery_check_cannot_accept_an_uncertain_message() {
    // Catches old history changing the delivery state after exit already settled the message.
    let (mut harness, message_id) = pending_steer().await;
    harness
        .control(
            "test/settleTurn",
            json!({ "method": "turn/steer", "result": { "turnId": "old-turn" } }),
        )
        .await;
    let mut steer = harness
        .completion(|done| matches!(done, Done::Steered { .. }))
        .await;
    let Done::Steered { result, .. } = &mut steer.done else {
        unreachable!("the completion predicate selected a steer")
    };
    *result = Err(RequestError::Rpc {
        code: -32000,
        message: "acknowledgement lost".into(),
    });
    steer.apply(&mut harness.actor).await;
    let old_reply = harness
        .completion(|done| matches!(done, Done::DeliveryChecked { .. }))
        .await;
    assert!(matches!(
        &old_reply.done,
        Done::DeliveryChecked { found: Some(_), .. }
    ));

    harness.exit().await;
    old_reply.apply(&mut harness.actor).await;

    let message = harness
        .actor
        .message(&harness.thread_id, &message_id)
        .await
        .unwrap();
    harness.close().await;
    assert!(
        matches!(message.status, MessageStatus::Unknown { .. }),
        "{:?}",
        message.status
    );
}

#[tokio::test]
async fn a_late_discard_cannot_cancel_an_uncertain_message() {
    // Catches a dead process's history committing a discard after its generation ended.
    let mut harness = Harness::start().await;
    let attached = harness.attached_reply().await;
    attached.apply(&mut harness.actor).await;
    let message_id = MessageId::new("uncertain".into());
    harness.send(&harness.thread_id.clone(), &message_id).await;
    let uncertain = message_id.clone();
    harness
        .actor
        .write(&harness.thread_id, move |write| {
            write.set_message_status(&uncertain, MessageStatus::Submitting)?;
            queue::mark_uncertain(write, uncertain, "lost acknowledgement".into())
        })
        .await
        .unwrap();
    let (reply, discarded) = oneshot::channel();
    harness
        .actor
        .discard(harness.thread_id.clone(), message_id.clone(), reply)
        .await;
    let old_reply = harness
        .completion(|done| matches!(done, Done::HistoryForDiscard { .. }))
        .await;

    harness.exit().await;
    old_reply.apply(&mut harness.actor).await;

    let result = discarded.await.unwrap();
    let message = harness
        .actor
        .message(&harness.thread_id, &message_id)
        .await
        .unwrap();
    harness.close().await;
    assert!(
        matches!(result, Err(ProviderError::Unavailable(_))),
        "{result:?}"
    );
    assert!(
        matches!(message.status, MessageStatus::Unknown { .. }),
        "{:?}",
        message.status
    );
}
