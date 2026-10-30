//! A Codex provider over a real store and hub, driving `fake-codex`, which a test steers through its
//! control port: it settles held turn requests, emits notifications, asks the provider questions,
//! and reads back what the provider sent.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use kratos_command_safety::DangerousCommandPlatform;
use kratos_engine::hub::{EventPages, Hub};
use kratos_engine::providers::codex::{self, CodexProvider, CodexSettings, ProtectedFolders, Sent};
use kratos_engine::store_thread::{self, StoreThread};
use kratos_engine::usage::UsageTracker;
use kratos_engine::usage::scheduler::{self, Clock, LimitStop, SchedulerHandle};
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

pub struct Options {
    pub turn_timeout: Duration,
    pub stop_timeout: Duration,
    pub settings: ThreadSettings,
    /// More arguments for the fake, such as `--state-file`.
    pub fake_args: Vec<OsString>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            turn_timeout: Duration::from_secs(600),
            stop_timeout: Duration::from_secs(5),
            settings: ThreadSettings::default(),
            fake_args: Vec::new(),
        }
    }
}

pub struct Harness {
    /// Holds the store's data directory, the thread's folder, and the fake's control file.
    pub root: TempDir,
    pub hub: Hub,
    pub store: StoreThread,
    pub usage: UsageTracker,
    pub provider: CodexProvider,
    /// Usage-limit stops, until a test starts the scheduler, which takes them.
    pub stops: Option<mpsc::UnboundedReceiver<LimitStop>>,
    pub scheduler: Option<SchedulerHandle>,
    pub thread_id: ThreadId,
    store_runner: std::thread::JoinHandle<()>,
    events: EventPages,
    /// Events read from the log and not yet taken by a test.
    seen: Vec<ThreadEvent>,
    control: Option<Control>,
    control_file: PathBuf,
    next_control_id: u64,
}

struct Control {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl Harness {
    pub async fn start(options: Options) -> Harness {
        let root = tempfile::tempdir().unwrap();
        Self::start_in(root, options, None).await
    }

    /// Starts on `root`'s records, as an engine restarted on the same data directory does.
    /// `thread_id` names the thread to follow when the records already hold it.
    pub async fn start_in(root: TempDir, options: Options, thread_id: Option<ThreadId>) -> Harness {
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
        let usage = UsageTracker::default();
        let control_file = root.path().join(format!("control-{}", uuid_like()));
        let server_folder = root.path().join("server");
        std::fs::create_dir_all(&server_folder).unwrap();
        let mut extra_args = vec!["--control-file".into(), control_file.clone().into()];
        extra_args.extend(options.fake_args);
        let settings = CodexSettings {
            program: fake_agents::executable("fake-codex"),
            extra_args,
            env_set: Vec::new(),
            server_folder,
            protected: ProtectedFolders::new(root.path().join("kratos"), None, None),
            turn_timeout: options.turn_timeout,
            stop_timeout: options.stop_timeout,
            request_timeout: Duration::from_secs(10),
            platform: DangerousCommandPlatform::Posix,
        };
        let (provider, stops) =
            codex::start(settings, hub.clone(), store_thread.clone(), usage.clone())
                .await
                .unwrap();
        let events = hub
            .follow(thread_id.clone(), Seq::BEFORE_FIRST)
            .await
            .unwrap();
        Harness {
            root,
            hub,
            store: store_thread,
            usage,
            provider,
            stops: Some(stops),
            scheduler: None,
            thread_id,
            store_runner,
            events,
            seen: Vec::new(),
            control: None,
            control_file,
            next_control_id: 0,
        }
    }

    /// Shuts the provider down and closes the store, then hands back the data directory, for a
    /// restart. Codex goes without settling anything, as when the engine is killed.
    pub async fn stop(self) -> (TempDir, ThreadId) {
        self.provider.shutdown().await;
        let Harness {
            root,
            hub,
            store,
            provider,
            scheduler,
            thread_id,
            store_runner,
            events,
            ..
        } = self;
        // The store closes once nothing holds it; the scheduler ends with its last handle.
        drop((hub, store, provider, scheduler, events));
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

    pub async fn send(&self, id: &str, text: &str) -> Sent {
        self.send_as(id, text, MessageOrigin::User).await
    }

    pub async fn send_as(&self, id: &str, text: &str, origin: MessageOrigin) -> Sent {
        self.provider
            .send(
                self.thread_id.clone(),
                message_id(id),
                text_input(text),
                origin,
            )
            .await
            .unwrap()
    }

    /// Starts the usage scheduler on `clock`, as the engine does after the provider.
    pub async fn start_scheduler(&mut self, clock: Arc<dyn Clock>) -> SchedulerHandle {
        let stops = self.stops.take().expect("the scheduler starts once");
        let handle = scheduler::start(
            self.hub.clone(),
            self.store.clone(),
            self.usage.clone(),
            self.provider.clone(),
            clock,
            stops,
        )
        .await
        .unwrap();
        self.scheduler = Some(handle.clone());
        handle
    }

    /// Creates another Codex thread with default settings, in a folder of its own named `name`.
    pub async fn another_thread(&self, name: &str) -> ThreadId {
        let folder = self.root.path().join(name);
        std::fs::create_dir_all(&folder).unwrap();
        self.store
            .call(move |store| create_thread(store, &folder, ThreadSettings::default()))
            .await
    }

    /// A `test/*` request to the fake, once it runs. Panics on an error answer.
    pub async fn control(&mut self, method: &str, params: Value) -> Value {
        let answer = self.try_control(method, params).await;
        match answer.get("error") {
            Some(error) => panic!("{method} failed: {error}"),
            None => answer["result"].clone(),
        }
    }

    pub async fn try_control(&mut self, method: &str, params: Value) -> Value {
        self.next_control_id += 1;
        let id = self.next_control_id;
        let control = self.connect().await;
        let line = format!(
            "{}\n",
            json!({ "id": id, "method": method, "params": params })
        );
        control.writer.write_all(line.as_bytes()).await.unwrap();
        let mut answer = String::new();
        tokio::time::timeout(WAIT, control.reader.read_line(&mut answer))
            .await
            .expect("the fake answers its control requests")
            .unwrap();
        serde_json::from_str(&answer).unwrap()
    }

    /// Ends the fake with exit code `code`, as a crash does.
    pub async fn exit_fake(&mut self, code: i32) {
        let control = self.connect().await;
        let line = format!(
            "{}\n",
            json!({ "id": 0, "method": "test/exit", "params": { "code": code } })
        );
        control.writer.write_all(line.as_bytes()).await.unwrap();
        let mut rest = String::new();
        let closed = tokio::time::timeout(WAIT, control.reader.read_line(&mut rest)).await;
        let disconnected = match &closed {
            Ok(Ok(0)) => true,
            Ok(Err(error)) => error.kind() == std::io::ErrorKind::ConnectionReset,
            _ => false,
        };
        assert!(disconnected, "the fake exits without answering: {closed:?}");
        assert!(rest.is_empty(), "the fake unexpectedly answered: {rest}");
        self.control = None;
    }

    async fn connect(&mut self) -> &mut Control {
        if self.control.is_none() {
            let deadline = tokio::time::Instant::now() + WAIT;
            let port = loop {
                if let Ok(port) = std::fs::read_to_string(&self.control_file) {
                    break port;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "fake-codex never started"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            let stream = TcpStream::connect(("127.0.0.1", port.trim().parse::<u16>().unwrap()))
                .await
                .unwrap();
            let (reader, writer) = stream.into_split();
            self.control = Some(Control {
                reader: BufReader::new(reader),
                writer,
            });
        }
        self.control.as_mut().expect("connected above")
    }

    /// The held requests the fake received, oldest first, as `{method, params}`.
    pub async fn turn_requests(&mut self) -> Vec<Value> {
        let requests = self.control("test/turnRequests", json!({})).await;
        requests.as_array().unwrap().clone()
    }

    /// Waits until the fake holds `count` requests of `method`, and returns them.
    pub async fn held(&mut self, method: &str, count: usize) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let matching: Vec<Value> = self
                .turn_requests()
                .await
                .into_iter()
                .filter(|request| request["method"] == method)
                .collect();
            if matching.len() >= count {
                return matching;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fake holds {} {method} requests, not {count}",
                matching.len()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Answers the oldest held `turn/start` with a turn in `status`.
    pub async fn settle_start(&mut self, turn_id: &str, status: &str) {
        let turn = json!({ "id": turn_id, "items": [], "status": status });
        self.control(
            "test/settleTurn",
            json!({ "method": "turn/start", "result": { "turn": turn } }),
        )
        .await;
    }

    pub async fn fail_start(&mut self, code: i64, error: &str) {
        self.control(
            "test/settleTurn",
            json!({ "method": "turn/start", "code": code, "error": error }),
        )
        .await;
    }

    /// Sends message `id` and starts its turn as `turn_id`, which then runs until a test ends it.
    /// The thread must be idle with nothing queued.
    pub async fn running_turn(&mut self, id: &str, turn_id: &str) {
        self.running_turn_as(id, turn_id, MessageOrigin::User).await;
    }

    pub async fn running_turn_as(&mut self, id: &str, turn_id: &str, origin: MessageOrigin) {
        self.send_as(id, "work", origin).await;
        let deadline = tokio::time::Instant::now() + WAIT;
        while !self
            .turn_requests()
            .await
            .iter()
            .any(|request| request["params"]["clientUserMessageId"] == id)
        {
            assert!(tokio::time::Instant::now() < deadline, "{id} never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.settle_start(turn_id, "inProgress").await;
        self.wait_for(|event| matches!(event, ThreadEventBody::TurnStarted { .. }))
            .await;
    }

    /// Makes the fake send the provider the request `method` under Codex's id `request_id`.
    pub async fn ask(&mut self, request_id: &str, method: &str, params: Value) {
        self.control(
            "test/ask",
            json!({ "requestId": request_id, "method": method, "params": params }),
        )
        .await;
    }

    /// Sends a notification to the provider.
    pub async fn emit(&mut self, method: &str, params: Value) {
        self.control("test/emit", json!({ "method": method, "params": params }))
            .await;
    }

    /// Ends `turn_id` of Codex thread `native_id` with `status`.
    pub async fn complete(&mut self, native_id: &str, turn_id: &str, status: &str) {
        self.emit(
            "turn/completed",
            json!({ "threadId": native_id, "turn": { "id": turn_id, "items": [], "status": status } }),
        )
        .await;
    }

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

    /// The answers the provider gave to requests the fake sent it, oldest first.
    pub async fn answers(&mut self) -> Vec<Value> {
        self.control("test/answers", json!({}))
            .await
            .as_array()
            .unwrap()
            .clone()
    }

    /// Waits until the provider answered `count` requests, and returns the answers.
    pub async fn answered(&mut self, count: usize) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let answers = self.answers().await;
            if answers.len() >= count {
                return answers;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} answers, not {count}",
                answers.len()
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
        json!({ "workspaceId": workspace.id, "title": "chat", "provider": "codex" }),
    )
    .unwrap();
    request.settings = Some(settings);
    store.create_thread(&request).unwrap().id
}

/// A file name no other harness in this process uses.
fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
