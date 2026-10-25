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

impl RunningEngine {
    pub fn start(home: &Path) -> RunningEngine {
        RunningEngine::start_on(home, 0)
    }

    pub fn start_on(home: &Path, port: u16) -> RunningEngine {
        Self::from_child(home, spawn(home, port))
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

    pub fn client(&self) -> Client {
        Client::new(&format!("http://127.0.0.1:{}", self.port)).unwrap()
    }

    /// Creates a workspace on the engine's home, which is an existing absolute folder, and a thread
    /// in it.
    pub fn create_thread(&self) -> Thread {
        let client = self.client();
        let workspace_body = json!({ "name": "repo", "path": self.home });
        let workspace: Workspace = client
            .post("/api/workspaces", &workspace_body)
            .unwrap()
            .parse()
            .unwrap();
        let thread_body =
            json!({ "workspaceId": workspace.id, "title": "chat", "provider": "claude" });
        client
            .post("/api/threads", &thread_body)
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Ends the process abruptly, as a crash would, and waits for it to exit.
    pub fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
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
    Command::new(ENGINE)
        .env("KRATOS_HOME", home)
        .env("KRATOS_PORT", port)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
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
