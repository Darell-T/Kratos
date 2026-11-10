//! A Claude provider over a real store and hub, driving `fake-claude` in its scripted mode. The
//! fake accepts and finishes nothing by itself: a test emits each frame, as the Claude CLI
//! would, and reads back what the provider wrote to the CLI.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use kratos_claude_client::{SessionId, transcript_path};
use kratos_engine::clock::Clock;
use kratos_engine::hub::{EventPages, Hub};
use kratos_engine::providers::Providers;
use kratos_engine::providers::Sent;
use kratos_engine::providers::claude::{self, ClaudeProvider, ClaudeSettings};
use kratos_engine::scheduler::{self, SchedulerHandle};
use kratos_engine::store_thread::{self, StoreThread};
use kratos_engine::usage::{LimitStop, UsageTracker};
use kratos_protocol::{
    CreateThread, CreateWorkspace, Message, MessageId, MessageInput, MessageOrigin, RequiredText,
    Seq, ThreadDetails, ThreadEvent, ThreadEventBody, ThreadId, ThreadSettings,
};
use kratos_store::Store;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// How long a test waits for something it expects. Generous: CI machines stall.
pub const WAIT: Duration = Duration::from_secs(20);

pub const A: &str = "00000000-0000-4000-8000-00000000000a";
pub const B: &str = "00000000-0000-4000-8000-00000000000b";
pub const C: &str = "00000000-0000-4000-8000-00000000000c";
pub const D: &str = "00000000-0000-4000-8000-00000000000d";
pub const E: &str = "00000000-0000-4000-8000-00000000000e";

/// The terminal result of a turn the user stopped.
pub fn interrupted() -> Value {
    json!({
        "subtype": "error_during_execution",
        "is_error": true,
        "errors": ["Interrupted by user"],
        "terminal_reason": "aborted_streaming",
    })
}

pub struct Options {
    pub tools: bool,
    pub stop_timeout: Duration,
    pub idle_close: Duration,
    pub control_timeout: Duration,
    pub restart_wait: Duration,
    pub max_restarts: u32,
    pub settings: ThreadSettings,
    /// The time the account's usage is judged at, when a test moves it by hand.
    pub clock: Option<Arc<dyn Clock>>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            tools: false,
            stop_timeout: Duration::from_secs(5),
            idle_close: Duration::from_secs(30 * 60),
            control_timeout: Duration::from_secs(10),
            restart_wait: Duration::from_millis(1),
            max_restarts: 5,
            settings: ThreadSettings::default(),
            clock: None,
        }
    }
}

/// One process the provider started: where the test reaches it, and how it was launched.
#[derive(Clone, Debug)]
pub struct Launch {
    pub pid: u32,
    pub port: u16,
    pub arguments: Vec<String>,
    pub cwd: String,
}

impl Launch {
    pub fn has(&self, argument: &str) -> bool {
        self.arguments.iter().any(|given| given == argument)
    }

    /// The value of a `--name=value` argument.
    pub fn value(&self, name: &str) -> Option<&str> {
        let prefix = format!("{name}=");
        self.arguments
            .iter()
            .find_map(|given| given.strip_prefix(&prefix))
    }

    /// The session the process was started to create or resume.
    pub fn session(&self) -> SessionId {
        let id = self
            .value("--session-id")
            .or_else(|| self.value("--resume"))
            .expect("a launch names its session");
        id.parse().unwrap()
    }
}

pub struct Harness {
    pub runner: Option<kratos_engine::providers::codex::CodexProvider>,
    /// Holds the store's data directory, the thread's folder, Claude's home, and the fake's log.
    pub root: TempDir,
    pub hub: Hub,
    pub store: StoreThread,
    pub provider: ClaudeProvider,
    pub usage: UsageTracker,
    /// Usage-limit stops, until a test starts the scheduler, which takes them.
    pub stops: Option<mpsc::UnboundedReceiver<LimitStop>>,
    pub scheduler: Option<SchedulerHandle>,
    pub thread_id: ThreadId,
    store_runner: std::thread::JoinHandle<()>,
    events: EventPages,
    /// Events read from the log and not yet taken by a test.
    seen: Vec<ThreadEvent>,
    log: PathBuf,
    fail_start: PathBuf,
}

impl Harness {
    pub async fn start(options: Options) -> Harness {
        let root = tempfile::tempdir().unwrap();
        Self::start_in(root, options, None).await
    }

    /// Starts on `root`'s records, as an engine restarted on the same data directory does.
    /// `thread_id` names the thread to follow when the records already hold it.
    pub async fn start_in(root: TempDir, options: Options, thread_id: Option<ThreadId>) -> Harness {
        #[cfg(windows)]
        kratos_process::contain_current_process().unwrap();
        let folder = root.path().join("project");
        std::fs::create_dir_all(&folder).unwrap();
        let store = Store::open(&root.path().join("kratos")).unwrap();
        let thread_id = match thread_id {
            Some(thread_id) => thread_id,
            None => create_thread(&store, &folder, options.settings.clone()),
        };
        let (store_thread, jobs) = store_thread::channel();
        let store_runner = std::thread::spawn(move || jobs.run(store));
        let hub = Hub::new(store_thread.clone());
        let log = root.path().join("fake-claude.log");
        let fail_start = root.path().join("fail-start");
        let env_set: Vec<(OsString, OsString)> = vec![
            ("FAKE_CLAUDE_LOG".into(), log.clone().into()),
            ("FAKE_CLAUDE_FAIL_START".into(), fail_start.clone().into()),
            (
                "FAKE_CLAUDE_PLAN".into(),
                root.path().join("plan").into_os_string(),
            ),
        ];
        let settings = ClaudeSettings {
            tools: None,
            program: fake_agents::executable("fake-claude"),
            claude_home: root.path().join("home"),
            env_set,
            stop_timeout: options.stop_timeout,
            idle_close: options.idle_close,
            control_timeout: options.control_timeout,
            restart_wait: options.restart_wait,
            max_restarts: options.max_restarts,
        };
        let mut settings = settings;
        let usage = options
            .clock
            .map_or_else(UsageTracker::default, UsageTracker::with_clock);
        let (limit_stops, stops) = mpsc::unbounded_channel();
        let runner = if options.tools {
            use kratos_engine::providers::codex::{self, CodexSettings, ProtectedFolders};
            let server_folder = root.path().join("command-server");
            std::fs::create_dir_all(&server_folder).unwrap();
            let protected = ProtectedFolders::new(
                root.path().join("kratos"),
                Some(root.path().join("home/.codex")),
                Some(root.path().join("home")),
            );
            let config = CodexSettings {
                program: fake_agents::executable("fake-codex"),
                extra_args: vec![
                    "--control-file".into(),
                    root.path().join("codex-control").into(),
                ],
                env_set: Vec::new(),
                server_folder,
                protected: protected.clone(),
                stop_timeout: options.stop_timeout,
                request_timeout: options.control_timeout,
                platform: kratos_command_safety::DangerousCommandPlatform::host(),
                mcp: None,
            };
            let runner = codex::start(
                config,
                hub.clone(),
                store_thread.clone(),
                UsageTracker::default(),
                limit_stops.clone(),
            )
            .await
            .unwrap();
            settings.tools = Some(claude::ClaudeTools {
                runner: runner.clone(),
                protected,
            });
            Some(runner)
        } else {
            None
        };
        let provider = claude::start(
            settings,
            hub.clone(),
            store_thread.clone(),
            usage.clone(),
            limit_stops,
        )
        .await
        .unwrap();
        let events = hub
            .follow(thread_id.clone(), Seq::BEFORE_FIRST)
            .await
            .unwrap();
        Harness {
            runner,
            root,
            hub,
            store: store_thread,
            provider,
            usage,
            stops: Some(stops),
            scheduler: None,
            thread_id,
            store_runner,
            events,
            seen: Vec::new(),
            log,
            fail_start,
        }
    }

    /// Starts the usage scheduler on the clock of the harness's tracker (`Options::clock`), as the
    /// engine does after the providers.
    pub async fn start_scheduler(&mut self) -> SchedulerHandle {
        let providers = Providers {
            codex: self
                .runner
                .clone()
                .ok_or_else(|| "Codex is not running in this test".to_owned()),
            claude: Ok(self.provider.clone()),
        };
        self.start_scheduler_with(providers).await
    }

    /// Starts the usage scheduler with the providers a test chooses.
    pub async fn start_scheduler_with(&mut self, providers: Providers) -> SchedulerHandle {
        let stops = self.stops.take().expect("the scheduler starts once");
        let handle = scheduler::start(
            self.hub.clone(),
            self.store.clone(),
            self.usage.clone(),
            providers,
            stops,
        )
        .await
        .unwrap();
        self.scheduler = Some(handle.clone());
        handle
    }

    /// Shuts the provider down and closes the store, then hands back the data directory, for a
    /// restart. The CLIs go without settling anything, as when the engine is killed.
    pub async fn stop(self) -> (TempDir, ThreadId) {
        self.provider.shutdown().await;
        if let Some(runner) = &self.runner {
            runner.shutdown().await;
        }
        let Harness {
            root,
            hub,
            store,
            provider,
            thread_id,
            store_runner,
            events,
            scheduler,
            ..
        } = self;
        drop((hub, store, provider, events, scheduler));
        let deadline = tokio::time::Instant::now() + WAIT;
        while !store_runner.is_finished() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the store never closed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (root, thread_id)
    }

    pub fn folder(&self) -> PathBuf {
        self.root.path().join("project")
    }

    pub async fn command_control(&self, method: &str, params: Value) -> Value {
        let path = self.root.path().join("codex-control");
        let port = tokio::task::spawn_blocking(move || {
            fake_agents::codex_control::wait_for_port(&path, WAIT)
        })
        .await
        .unwrap()
        .unwrap();
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(
                format!(
                    "{}\n",
                    json!({ "id": 1, "method": method, "params": params })
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(WAIT, BufReader::new(reader).read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        reply["result"].clone()
    }

    /// Claude's home folder, where its transcripts live.
    pub fn claude_home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// Makes every start of the fake after this one sign in under the subscription `plan`.
    pub fn set_plan(&self, plan: &str) {
        std::fs::write(self.root.path().join("plan"), plan).unwrap();
    }

    /// Creates another Claude thread with default settings, in a folder of its own named `name`.
    pub async fn another_thread(&self, name: &str) -> ThreadId {
        let folder = self.root.path().join(name);
        std::fs::create_dir_all(&folder).unwrap();
        self.store
            .call(move |store| create_thread(store, &folder, ThreadSettings::default()))
            .await
    }

    /// Makes every start of the fake fail with `code` until [`Self::start_working`].
    pub fn start_failing(&self, code: i32) {
        std::fs::write(&self.fail_start, code.to_string()).unwrap();
    }

    pub fn start_working(&self) {
        std::fs::remove_file(&self.fail_start).unwrap();
    }

    pub async fn send(&self, id: &str, text: &str) -> Sent {
        self.provider
            .send(
                self.thread_id.clone(),
                message_id(id),
                text_input(text),
                MessageOrigin::User,
            )
            .await
            .unwrap()
    }

    // The fake processes.

    /// Every process the fake logged starting, oldest first.
    pub fn launches(&self) -> Vec<Launch> {
        let Ok(log) = std::fs::read_to_string(&self.log) else {
            return Vec::new();
        };
        log.lines()
            .map(|line| {
                let line: Value = serde_json::from_str(line).unwrap();
                Launch {
                    pid: line["pid"].as_u64().unwrap().try_into().unwrap(),
                    port: line["port"].as_u64().unwrap().try_into().unwrap(),
                    arguments: serde_json::from_value(line["arguments"].clone()).unwrap(),
                    cwd: line["cwd"].as_str().unwrap().to_owned(),
                }
            })
            .collect()
    }

    /// The `n`th process the provider started (from 1), waiting for it to start.
    pub async fn launch(&self, n: usize) -> Launch {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(launch) = self.launches().get(n - 1) {
                return launch.clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the provider never started process {n}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A `test/*` request to the process.
    pub async fn call(&self, launch: &Launch, method: &str, params: Value) -> Value {
        let stream = TcpStream::connect(("127.0.0.1", launch.port))
            .await
            .unwrap();
        let (reader, mut writer) = stream.into_split();
        let line = format!(
            "{}\n",
            json!({ "id": 1, "method": method, "params": params })
        );
        writer.write_all(line.as_bytes()).await.unwrap();
        let mut answer = String::new();
        tokio::time::timeout(WAIT, BufReader::new(reader).read_line(&mut answer))
            .await
            .expect("the fake answers its control requests")
            .unwrap();
        let answer: Value = serde_json::from_str(&answer).unwrap();
        answer["result"].clone()
    }

    /// Every frame the provider wrote to the process, oldest first.
    pub async fn received(&self, launch: &Launch) -> Vec<Value> {
        self.call(launch, "test/received", json!({}))
            .await
            .as_array()
            .unwrap()
            .clone()
    }

    /// The uuids of the user messages the provider wrote to the process, oldest first.
    pub async fn written(&self, launch: &Launch) -> Vec<String> {
        self.received(launch)
            .await
            .iter()
            .filter(|frame| frame["type"] == "user")
            .map(|frame| frame["uuid"].as_str().unwrap().to_owned())
            .collect()
    }

    /// How many controls of `subtype` the provider sent the process.
    pub async fn controls(&self, launch: &Launch, subtype: &str) -> usize {
        self.received(launch)
            .await
            .iter()
            .filter(|frame| frame["request"]["subtype"] == subtype)
            .count()
    }

    /// Waits until the process has been written exactly `expected`, and returns.
    pub async fn wait_written(&self, launch: &Launch, expected: &[&str]) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let written = self.written(launch).await;
            if written == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "written {written:?}, not {expected:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Asserts, after the provider had time to act, that the process was written exactly `expected`.
    pub async fn assert_written(&self, launch: &Launch, expected: &[&str]) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(self.written(launch).await, expected);
    }

    pub async fn emit(&self, launch: &Launch, message: Value) {
        self.call(launch, "test/emit", json!({ "message": message }))
            .await;
    }

    /// Claude begins a turn.
    pub async fn init(&self, launch: &Launch) {
        let session = launch.session().to_string();
        self.emit(
            launch,
            json!({ "type": "system", "subtype": "init", "model": "claude-test",
                    "session_id": session }),
        )
        .await;
    }

    /// Claude writes the message to its transcript and echoes it back, which is how it accepts it.
    pub async fn accept(&self, launch: &Launch, id: &str) {
        self.record(launch, id);
        let session = launch.session().to_string();
        self.emit(
            launch,
            json!({ "type": "user", "isReplay": true, "uuid": id, "session_id": session,
                    "message": { "role": "user", "content": [] }, "parent_tool_use_id": null }),
        )
        .await;
    }

    /// The transcript holds the message, as it does once Claude has taken it, whether or not the
    /// provider heard of it.
    pub fn record(&self, launch: &Launch, id: &str) {
        let path = transcript_path(&self.claude_home(), &self.folder(), launch.session());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let row = format!("{}\n", json!({ "type": "user", "uuid": id }));
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&row);
        std::fs::write(path, text).unwrap();
    }

    /// Claude ends the turn it ran for `ids`; `outcome` changes or adds fields of the result.
    pub async fn finish(&self, launch: &Launch, ids: &[&str], outcome: Value) {
        let mut result = json!({
            "type": "result", "subtype": "success", "is_error": false, "result": "Done",
            "user_message_uuids": ids,
        });
        if let Value::Object(fields) = outcome {
            result.as_object_mut().unwrap().extend(fields);
        }
        self.emit(launch, result).await;
    }

    /// Sends message `id` and plays Claude taking it up on process `n`: the process starts and is
    /// initialized, and the message is accepted, so its turn runs. Returns the process.
    pub async fn begin(&self, id: &str, n: usize) -> Launch {
        self.send(id, id).await;
        let launch = self.launch(n).await;
        self.wait_written(&launch, &[id]).await;
        self.init(&launch).await;
        self.accept(&launch, id).await;
        launch
    }

    pub async fn interrupts(&self, launch: &Launch, mode: &str) {
        self.call(launch, "test/interrupts", json!({ "mode": mode }))
            .await;
    }

    /// Ends the process with `code`, as a crash does, and waits until it is gone.
    pub async fn exit(&self, launch: &Launch, code: i32) {
        // The fake exits without answering, so only the connection's end is awaited.
        let stream = TcpStream::connect(("127.0.0.1", launch.port))
            .await
            .unwrap();
        let (mut reader, mut writer) = stream.into_split();
        let line = format!(
            "{}\n",
            json!({ "id": 1, "method": "test/exit", "params": { "code": code } })
        );
        writer.write_all(line.as_bytes()).await.unwrap();
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(
            WAIT,
            tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut rest),
        )
        .await;
        self.wait_ended(launch).await;
    }

    /// Waits until the process has ended.
    pub async fn wait_ended(&self, launch: &Launch) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while fake_agents::process_is_running(launch.pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "process {} never ended",
                launch.pid
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub fn is_running(&self, launch: &Launch) -> bool {
        fake_agents::process_is_running(launch.pid)
    }

    // The records.

    /// The next event of the thread, waiting for it.
    pub async fn next_event(&mut self) -> ThreadEvent {
        if self.seen.is_empty() {
            let page = tokio::time::timeout(WAIT, self.events.next())
                .await
                .expect("an event arrives")
                .expect("a follow never ends")
                .unwrap();
            self.seen = page;
        }
        self.seen.remove(0)
    }

    /// Skips events until one that `wanted` picks, and returns it.
    pub async fn wait_for(
        &mut self,
        mut wanted: impl FnMut(&ThreadEventBody) -> bool,
    ) -> ThreadEventBody {
        loop {
            let event = self.next_event().await;
            if wanted(&event.event) {
                return event.event;
            }
        }
    }

    /// Every event stored so far, oldest first.
    pub async fn log(&self) -> Vec<ThreadEventBody> {
        let thread_id = self.thread_id.clone();
        self.store
            .call(move |store| store.events_after(&thread_id, Seq::BEFORE_FIRST, usize::MAX))
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.event)
            .collect()
    }

    pub async fn details(&self) -> ThreadDetails {
        let thread_id = self.thread_id.clone();
        self.store
            .call(move |store| store.thread_details(&thread_id))
            .await
            .unwrap()
    }

    pub async fn message(&self, id: &str) -> Message {
        let (thread_id, id) = (self.thread_id.clone(), message_id(id));
        self.store
            .call(move |store| store.message(&thread_id, &id))
            .await
            .unwrap()
            .expect("the message is stored")
    }

    /// Waits until message `id` is in the status `wanted` names, and returns it.
    pub async fn message_becomes(
        &self,
        id: &str,
        wanted: impl Fn(&kratos_protocol::MessageStatus) -> bool,
    ) -> Message {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let message = self.message(id).await;
            if wanted(&message.status) {
                return message;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "message {id} stays {:?}",
                message.status
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the thread is paused for a reason `wanted` picks, and returns it.
    pub async fn paused_for(
        &self,
        wanted: impl Fn(&kratos_protocol::PauseReason) -> bool,
    ) -> kratos_protocol::PauseReason {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let details = self.details().await;
            if let Some(reason) = details.paused.filter(|reason| wanted(reason)) {
                return reason;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the thread is paused for {:?}",
                self.details().await.paused
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

pub fn message_id(id: &str) -> MessageId {
    MessageId::new(id.to_owned())
}

pub fn text_input(text: &str) -> Vec<MessageInput> {
    vec![MessageInput::Text {
        text: RequiredText::new(text).unwrap(),
    }]
}

fn create_thread(store: &Store, folder: &Path, settings: ThreadSettings) -> ThreadId {
    let workspace: CreateWorkspace =
        serde_json::from_value(json!({ "name": "project", "path": folder })).unwrap();
    let workspace = store.create_workspace(&workspace).unwrap();
    let mut request: CreateThread = serde_json::from_value(
        json!({ "workspaceId": workspace.id, "title": "chat", "provider": "claude" }),
    )
    .unwrap();
    request.settings = Some(settings);
    store.create_thread(&request).unwrap().id
}
