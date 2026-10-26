#[cfg(windows)]
use std::io::Write;
use std::io::{BufRead, BufReader, Read};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use kratos_client::{Client, EventSocket, Incoming};
use kratos_protocol::{Seq, ServerMessage, Thread, ThreadEvent, ThreadId, Workspace};
use serde_json::json;

const ENGINE: &str = env!("CARGO_BIN_EXE_kratos-engine");

/// How long a test waits for the engine to start, answer, or exit, and for a socket to deliver.
pub const WAIT: Duration = Duration::from_secs(10);

/// How long a test listens to a socket to show that nothing more arrives.
pub const QUIET: Duration = Duration::from_millis(300);

pub struct RunningEngine {
    child: Child,
    port: u16,
    home: PathBuf,
}

/// Where the fake Codex an engine starts writes its control port.
const FAKE_CODEX_CONTROL: &str = "fake-codex-control";

impl RunningEngine {
    pub fn start(home: &Path) -> RunningEngine {
        RunningEngine::start_on(home, 0)
    }

    pub fn start_on(home: &Path, port: u16) -> RunningEngine {
        Self::from_child(home, spawn(home, port))
    }

    /// Starts with `CODEX_EXE` set to `codex_exe` instead of the fake.
    pub fn start_with_codex(home: &Path, codex_exe: &Path) -> RunningEngine {
        let mut command = command(home, "0");
        command.env("CODEX_EXE", codex_exe);
        Self::from_child(home, command.spawn().unwrap())
    }

    /// Gives this engine its own hidden console so Ctrl+C cannot reach the test runner.
    #[cfg(windows)]
    pub fn start_for_shutdown(home: &Path) -> RunningEngine {
        let child = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                include_str!("windows_shutdown.ps1"),
            ])
            .env("KRATOS_SHUTDOWN_ENGINE", ENGINE)
            .env("KRATOS_HOME", home)
            .env("KRATOS_PORT", "0")
            .env("CODEX_EXE", fake_agents::executable("fake-codex"))
            .env("FAKE_CODEX_CONTROL_FILE", home.join(FAKE_CODEX_CONTROL))
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW for the PowerShell helper.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self::from_child(home, child)
    }

    fn from_child(home: &Path, mut child: Child) -> RunningEngine {
        let stdout = child.stdout.take().expect("the engine's stdout is piped");
        let (announced, listening_port) = mpsc::channel();
        // The reader keeps draining stdout after the listening line, so the engine never blocks on
        // a full pipe.
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(port) = listening_port_of(&line) {
                    let _ = announced.send(port);
                }
            }
        });
        match listening_port.recv_timeout(WAIT) {
            Ok(port) => RunningEngine {
                child,
                port,
                home: home.to_path_buf(),
            },
            Err(error) => {
                if child.stdin.take().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("the engine printed no listening line ({error}); stderr: {stderr}");
            }
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The client token this engine issued at its start.
    pub fn token(&self) -> String {
        std::fs::read_to_string(self.home.join("client-token")).unwrap()
    }

    /// A `test/*` request to the fake Codex this engine started, waiting for it to start. Panics
    /// on an error answer.
    pub fn fake_codex(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let control = self.home.join(FAKE_CODEX_CONTROL);
        let deadline = Instant::now() + WAIT;
        let port = loop {
            if let Ok(port) = std::fs::read_to_string(&control) {
                break port;
            }
            assert!(
                Instant::now() < deadline,
                "the engine never started fake-codex"
            );
            thread::sleep(Duration::from_millis(20));
        };
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", port.trim().parse().unwrap())).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let request = json!({ "id": 1, "method": method, "params": params });
        std::io::Write::write_all(&mut stream, format!("{request}\n").as_bytes()).unwrap();
        let mut answer = String::new();
        BufReader::new(stream).read_line(&mut answer).unwrap();
        let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert!(answer.get("error").is_none(), "{method} failed: {answer}");
        answer["result"].clone()
    }

    /// Waits until the fake Codex holds `count` requests of `method`, and returns them.
    pub fn fake_codex_held(&self, method: &str, count: usize) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + WAIT;
        loop {
            let held: Vec<serde_json::Value> = self
                .fake_codex("test/turnRequests", json!({}))
                .as_array()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == method)
                .cloned()
                .collect();
            if held.len() >= count {
                return held;
            }
            assert!(
                Instant::now() < deadline,
                "fake-codex holds {} {method}, not {count}",
                held.len()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until the engine answered `count` of the requests the fake Codex sent it, and returns
    /// the answers. An answer travels to Codex after the API's reply, so a test waits for it.
    pub fn fake_codex_answers(&self, count: usize) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + WAIT;
        loop {
            let answers = self.fake_codex("test/answers", json!({}));
            let answers = answers.as_array().unwrap();
            if answers.len() >= count {
                return answers.clone();
            }
            assert!(Instant::now() < deadline, "{} answers, not {count}", answers.len());
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Creates a workspace on the engine's home and a thread of `provider` in it.
    pub fn create_thread_of(&self, provider: &str) -> Thread {
        let client = self.client();
        let workspace_body = json!({ "name": "repo", "path": self.home });
        let workspace: Workspace = client
            .post("/api/workspaces", &workspace_body)
            .unwrap()
            .parse()
            .unwrap();
        let thread_body =
            json!({ "workspaceId": workspace.id, "title": "chat", "provider": provider });
        client
            .post("/api/threads", &thread_body)
            .unwrap()
            .parse()
            .unwrap()
    }

    pub fn client(&self) -> Client {
        Client::new(&format!("http://127.0.0.1:{}", self.port))
            .unwrap()
            .with_token_from(&self.home)
    }

    /// Creates a workspace on the engine's home, which is an existing absolute folder, and a
    /// Claude thread in it, which keeps R2's plain message log.
    pub fn create_thread(&self) -> Thread {
        self.create_thread_of("claude")
    }

    /// Ends the process abruptly, as a crash would, and waits for it to exit.
    pub fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    /// Sends Ctrl+C's signal and waits for the engine to exit on its own.
    #[cfg(unix)]
    pub fn interrupt(mut self) -> ExitStatus {
        let sent = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the engine did not stop within {WAIT:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(windows)]
    pub fn shutdown(&mut self) {
        let started = Instant::now();
        self.child
            .stdin
            .take()
            .unwrap()
            .write_all(b"shutdown\n")
            .unwrap();
        // The helper waits on the real engine process handle for at most eight seconds, kills it
        // on timeout, and reports failure. Waiting for the helper cannot turn a hung engine green.
        let status = self.child.wait().unwrap();
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(
            status.success(),
            "the engine did not shut down cleanly: {stderr}"
        );
        assert!(started.elapsed() < WAIT, "shutdown exceeded {WAIT:?}");
    }
}

impl Drop for RunningEngine {
    fn drop(&mut self) {
        // EOF tells a console helper to kill its child as well. Ordinary engines have no stdin.
        if self.child.stdin.take().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

pub fn run_until_exit(home: &Path, port: &str) -> (ExitStatus, String) {
    let mut child = spawn_with_port(home, port);
    let mut pipe = child.stderr.take().expect("the engine's stderr is piped");
    let stderr = thread::spawn(move || {
        let mut text = String::new();
        let _ = pipe.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return (status, stderr.join().unwrap());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the engine did not exit within {WAIT:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn spawn(home: &Path, port: u16) -> Child {
    spawn_with_port(home, &port.to_string())
}

fn spawn_with_port(home: &Path, port: &str) -> Child {
    command(home, port).spawn().unwrap()
}

/// Codex is the fake unless a test says otherwise, so no test reaches a Codex installed on the
/// machine. The fake takes its control file from the variable, which the engine passes on.
fn command(home: &Path, port: &str) -> Command {
    let mut command = Command::new(ENGINE);
    command
        .env("KRATOS_HOME", home)
        .env("KRATOS_PORT", port)
        .env("CODEX_EXE", fake_agents::executable("fake-codex"))
        .env("FAKE_CODEX_CONTROL_FILE", home.join(FAKE_CODEX_CONTROL))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn listening_port_of(line: &str) -> Option<u16> {
    let rest = line.strip_prefix("Kratos engine listening on http://127.0.0.1:")?;
    rest.split_once(' ')?.0.parse().ok()
}

pub fn post(client: &Client, thread: &Thread, text: &str) -> ThreadEvent {
    let path = format!("/api/threads/{}/messages", thread.id);
    client
        .post(&path, &json!({ "text": text }))
        .unwrap()
        .parse()
        .unwrap()
}

pub fn subscribed(engine: &RunningEngine, thread_id: &ThreadId, after: u64) -> EventSocket {
    let mut socket = engine.client().connect_events(&[]).unwrap();
    socket.subscribe(thread_id, Seq::new(after)).unwrap();
    socket
}

pub fn next_text(socket: &mut EventSocket) -> String {
    match socket.next_incoming(WAIT).unwrap() {
        Some(Incoming::Text(text)) => text,
        other => panic!("expected a message, got {other:?}"),
    }
}

pub fn next_event(socket: &mut EventSocket) -> ThreadEvent {
    let text = next_text(socket);
    match serde_json::from_str(&text).unwrap() {
        ServerMessage::Event { event } => event,
        other => panic!("expected an event, got {other:?}"),
    }
}

pub fn next_seqs(socket: &mut EventSocket, count: usize) -> Vec<u64> {
    (0..count).map(|_| next_event(socket).seq.get()).collect()
}

pub fn expect_silence(socket: &mut EventSocket) {
    let incoming = socket.next_incoming(QUIET).unwrap();
    assert_eq!(incoming, None, "the socket should be quiet");
}
