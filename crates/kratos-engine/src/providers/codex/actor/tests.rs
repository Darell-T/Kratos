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
use crate::providers::codex::ProtectedFolders;
use crate::providers::codex::threads::{Link, Turn};
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
        loop {
            self.actor.attached(&self.thread_id).await.unwrap();
            let done = tokio::time::timeout(WAIT, self.actor.completions.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(done.work, Work::Update(Done::Attached { .. })) {
                return done;
            }
            done.apply(&mut self.actor).await;
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
async fn an_attach_reply_after_exit_does_not_load_a_dead_process() {
    // Catches a queued attach success loading a thread after its Codex process exited.
    let mut harness = Harness::start().await;
    let old_reply = harness.attached_reply().await;

    harness.exit().await;
    old_reply.apply(&mut harness.actor).await;

    let detached = matches!(
        harness.actor.threads[&harness.thread_id].link,
        Link::Detached
    );
    let thread_id = harness.thread_id.clone();
    let native_id = harness
        .actor
        .store
        .call(move |store| store.provider_thread(&thread_id).unwrap().native_id)
        .await;
    harness.close().await;
    assert!(detached, "the dead process cannot load a thread");
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
async fn exit_pauses_a_message_waiting_for_attachment_without_sending_it() {
    // Catches a stale attach reply being ignored without reporting why queued work cannot run.
    let mut harness = Harness::start().await;
    let message_id = MessageId::new("waiting".into());
    harness
        .actor
        .send(
            &harness.thread_id,
            message_id.clone(),
            vec![MessageInput::Text {
                text: RequiredText::new("work").unwrap(),
            }],
            MessageOrigin::User,
        )
        .await
        .unwrap();
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
    let detached = matches!(
        harness.actor.threads[&harness.thread_id].link,
        Link::Detached
    );
    let down = matches!(harness.actor.server, Server::Down);
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
    assert!(
        detached && down,
        "the stale attach cannot send waiting work"
    );
}

#[tokio::test]
async fn exit_settles_a_pending_start_and_its_stop_without_waiting_for_a_reply() {
    // Catches process fencing leaving a start submitting forever and its stop unanswered.
    let mut harness = Harness::start().await;
    let attached = harness.attached_reply().await;
    attached.apply(&mut harness.actor).await;
    let message_id = MessageId::new("pending".into());
    harness
        .actor
        .send(
            &harness.thread_id,
            message_id.clone(),
            vec![MessageInput::Text {
                text: RequiredText::new("work").unwrap(),
            }],
            MessageOrigin::User,
        )
        .await
        .unwrap();
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
    let idle = matches!(harness.actor.threads[&harness.thread_id].turn, Turn::Idle);
    let stopped = stopped.try_recv();
    harness.close().await;
    assert!(
        matches!(message.status, MessageStatus::Unknown { .. }),
        "{:?}",
        message.status
    );
    assert!(idle, "process exit ends the pending start");
    assert!(matches!(stopped, Ok(Ok(()))), "{stopped:?}");
}

#[tokio::test]
async fn a_start_reply_after_exit_cannot_accept_a_message_or_start_a_turn() {
    // Catches a queued start success accepting delivery and reviving a turn after process exit.
    let mut harness = Harness::start().await;
    let attached = harness.attached_reply().await;
    attached.apply(&mut harness.actor).await;
    let message_id = MessageId::new("pending".into());
    harness
        .actor
        .send(
            &harness.thread_id,
            message_id.clone(),
            vec![MessageInput::Text {
                text: RequiredText::new("work").unwrap(),
            }],
            MessageOrigin::User,
        )
        .await
        .unwrap();
    harness.actor.dispatch(&harness.thread_id).await;
    harness.held("turn/start").await;
    harness.control("test/settleTurn", json!({
        "method": "turn/start", "result": { "turn": { "id": "old-turn", "items": [], "status": "inProgress" } },
    })).await;
    let old_reply = loop {
        let done = tokio::time::timeout(WAIT, harness.actor.completions.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(done.work, Work::Update(Done::StartAnswered { .. })) {
            break done;
        }
        done.apply(&mut harness.actor).await;
    };

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
    harness
        .actor
        .send(
            &harness.thread_id,
            first,
            vec![MessageInput::Text {
                text: RequiredText::new("work").unwrap(),
            }],
            MessageOrigin::User,
        )
        .await
        .unwrap();
    harness.actor.dispatch(&harness.thread_id).await;
    harness.held("turn/start").await;
    harness.control("test/settleTurn", json!({
        "method": "turn/start", "result": { "turn": { "id": "old-turn", "items": [], "status": "inProgress" } },
    })).await;
    while !matches!(
        harness.actor.threads[&harness.thread_id].turn,
        Turn::Running { .. }
    ) {
        let done = tokio::time::timeout(WAIT, harness.actor.completions.recv())
            .await
            .unwrap()
            .unwrap();
        done.apply(&mut harness.actor).await;
    }
    let message_id = MessageId::new("steer".into());
    harness
        .actor
        .send(
            &harness.thread_id,
            message_id.clone(),
            vec![MessageInput::Text {
                text: RequiredText::new("more work").unwrap(),
            }],
            MessageOrigin::User,
        )
        .await
        .unwrap();
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
    let no_steer = harness.actor.threads[&harness.thread_id]
        .steering
        .is_empty();
    harness.close().await;
    assert!(
        matches!(message.status, MessageStatus::Unknown { .. }),
        "{:?}",
        message.status
    );
    assert!(no_steer, "process exit ends the pending steer");
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
async fn a_late_skills_reply_reports_the_exited_process() {
    // Catches a direct skills reply bypassing the actor's process generation.
    let mut harness = Harness::start().await;
    let attached = harness.attached_reply().await;
    attached.apply(&mut harness.actor).await;
    let (reply, listed) = oneshot::channel();
    harness
        .actor
        .on_command(Command::Skills {
            thread_id: harness.thread_id.clone(),
            reply,
        })
        .await;
    let old_reply = loop {
        let done = tokio::time::timeout(WAIT, harness.actor.completions.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(done.work, Work::Reply(Response::SkillsListed { .. })) {
            break done;
        }
        done.apply(&mut harness.actor).await;
    };

    harness.exit().await;
    old_reply.apply(&mut harness.actor).await;

    let result = listed.await.unwrap();
    harness.close().await;
    assert!(
        matches!(result, Err(ProviderError::Unavailable(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_late_command_reply_reports_the_exited_process() {
    // Catches a direct command reply reporting success after its Codex process exited.
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
    let old_reply = loop {
        let done = tokio::time::timeout(WAIT, harness.actor.completions.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(done.work, Work::Reply(Response::CommandRan { .. })) {
            break done;
        }
        done.apply(&mut harness.actor).await;
    };

    harness.exit().await;
    old_reply.apply(&mut harness.actor).await;

    let result = ran.await.unwrap();
    harness.close().await;
    assert!(
        matches!(result, Err(ProviderError::Unavailable(_))),
        "{result:?}"
    );
}
