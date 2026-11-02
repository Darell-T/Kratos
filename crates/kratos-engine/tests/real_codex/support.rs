use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use fake_agents::responses::{Reply, Server};
use futures_util::StreamExt;
use kratos_client::Client;
use kratos_command_safety::DangerousCommandPlatform;
use kratos_engine::hub::{EventPages, Hub};
use kratos_engine::providers::codex::{self, CodexProvider, CodexSettings, ProtectedFolders};
use kratos_engine::server::{self, ClientToken, ServerState};
use kratos_engine::store_thread;
use kratos_engine::usage::UsageTracker;
use kratos_protocol::{Seq, Thread, ThreadEventBody, ThreadSettings, TurnOutcome};
use kratos_store::Store;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::oneshot;

pub const WAIT: Duration = Duration::from_secs(180);

pub struct Harness {
    pub root: TempDir,
    pub model: Server,
    pub client: Client,
    pub base_url: String,
    pub thread: Thread,
    pub provider: CodexProvider,
    events: EventPages,
    pending_events: VecDeque<ThreadEventBody>,
    shutdown: Option<oneshot::Sender<()>>,
    serving: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Harness {
    /// The home must be an empty, dedicated test home provisioned with the pinned CLI's
    /// `sandbox setup --elevated --current-user --codex-home <home>`. Run these tests serially.
    pub async fn start(settings: ThreadSettings, replies: Vec<Reply>) -> Self {
        let program = PathBuf::from(
            std::env::var_os("KRATOS_REAL_CODEX_EXE")
                .expect("set KRATOS_REAL_CODEX_EXE to the pinned native Codex binary"),
        );
        let version = std::process::Command::new(&program)
            .arg("--version")
            .output()
            .unwrap();
        assert!(version.status.success());
        assert_eq!(
            String::from_utf8_lossy(&version.stdout).trim(),
            "codex-cli 0.159.1"
        );
        let codex_home = PathBuf::from(
            std::env::var_os("KRATOS_REAL_CODEX_HOME")
                .expect("set KRATOS_REAL_CODEX_HOME to a dedicated, provisioned test home"),
        );
        assert!(codex_home.is_absolute() && codex_home.is_dir());
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("kratos");
        let folder = root.path().join("project");
        let server_folder = root.path().join("server");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&server_folder).unwrap();
        let model = Server::start(replies).await.unwrap();
        let config = [
            "features.memories=false".to_owned(),
            "memories.generate_memories=false".to_owned(),
            "memories.use_memories=false".to_owned(),
            "features.plugins=false".to_owned(),
            "features.apps=false".to_owned(),
            "model=\"gpt-6.1-sol\"".to_owned(),
            "model_provider=\"scripted\"".to_owned(),
            "model_providers.scripted.name=\"Isolated scripted Responses API\"".to_owned(),
            format!(
                "model_providers.scripted.base_url={}",
                json!(model.base_url())
            ),
            "model_providers.scripted.wire_api=\"responses\"".to_owned(),
            "model_providers.scripted.requires_openai_auth=false".to_owned(),
            "model_providers.scripted.supports_websockets=false".to_owned(),
            "features.unified_exec=true".to_owned(),
            "features.responses_websockets=false".to_owned(),
            "features.responses_websockets_v2=false".to_owned(),
            "web_search=\"disabled\"".to_owned(),
        ];
        let extra_args = config
            .into_iter()
            .flat_map(|value| [OsString::from("-c"), value.into()])
            .collect();
        let records = Store::open(&home).unwrap();
        let token = ClientToken::issue(&home).unwrap();
        let (store, jobs) = store_thread::channel();
        // The store runner stops when the server, provider and event stream drop their handles.
        std::thread::spawn(move || jobs.run(records));
        let hub = Hub::new(store.clone());
        let usage = UsageTracker::default();
        let codex_settings = CodexSettings {
            program,
            extra_args,
            env_set: vec![("CODEX_HOME".into(), codex_home.as_os_str().into())],
            server_folder,
            protected: ProtectedFolders::new(home.clone(), Some(codex_home), None),
            turn_timeout: WAIT,
            stop_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            platform: DangerousCommandPlatform::host(),
            mcp: None,
        };
        let (provider, _stops) =
            codex::start(codex_settings, hub.clone(), store.clone(), usage.clone())
                .await
                .unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let state = ServerState {
            store,
            hub: hub.clone(),
            codex: Ok(provider.clone()),
            scheduler: None,
            usage,
            token,
        };
        let (shutdown, stopped) = oneshot::channel();
        let serving = tokio::spawn(server::serve(listener, state, async {
            let _ = stopped.await;
        }));
        let client = Client::new(&base_url).unwrap().with_token_from(&home);
        let workspace = client
            .post(
                "/api/workspaces",
                &json!({ "name": "sandbox test", "path": folder }),
            )
            .unwrap();
        assert_eq!(workspace.status, 201, "{:?}", workspace.body);
        let thread = client.post("/api/threads", &json!({ "workspaceId": workspace.body["id"], "title": "sandbox test", "provider": "codex", "settings": settings })).unwrap();
        assert_eq!(thread.status, 201, "{:?}", thread.body);
        let thread: Thread = thread.parse().unwrap();
        let events = hub
            .follow(thread.id.clone(), Seq::BEFORE_FIRST)
            .await
            .unwrap();
        Self {
            root,
            model,
            client,
            base_url,
            thread,
            provider,
            events,
            pending_events: VecDeque::new(),
            shutdown: Some(shutdown),
            serving,
        }
    }

    pub fn folder(&self) -> PathBuf {
        self.root.path().join("project")
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().join("kratos")
    }

    pub fn send(&self, id: &str) {
        let sent = self
            .client
            .post(
                &format!("/api/threads/{}/messages", self.thread.id),
                &json!({ "messageId": id, "text": "Run the scripted test commands." }),
            )
            .unwrap();
        assert_eq!(sent.status, 201, "{:?}", sent.body);
    }

    pub async fn next_event(&mut self) -> ThreadEventBody {
        if self.pending_events.is_empty() {
            let page = tokio::time::timeout(WAIT, self.events.next())
                .await
                .expect("a real Codex event arrives")
                .expect("event log stays open")
                .unwrap();
            self.pending_events
                .extend(page.into_iter().map(|event| event.event));
        }
        self.pending_events.pop_front().unwrap()
    }

    pub async fn completed(&mut self) {
        loop {
            let event = self.next_event().await;
            match event {
                ThreadEventBody::TurnCompleted { outcome, .. } => {
                    assert_eq!(
                        outcome,
                        TurnOutcome::Completed,
                        "model requests: {:?}",
                        self.model.requests()
                    );
                    return;
                }
                ThreadEventBody::ThreadPaused { reason } => panic!("real Codex paused: {reason:?}"),
                ThreadEventBody::ApprovalRequested { approval } => {
                    panic!("test unexpectedly needs approval: {approval:?}")
                }
                _ => {}
            }
        }
    }

    pub async fn stop(mut self) {
        self.provider.shutdown().await;
        let _ = self.shutdown.take().unwrap().send(());
        self.serving.await.unwrap().unwrap();
    }
}
