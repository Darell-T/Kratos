use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use kratos_protocol::{Seq, ServerMessage, Thread, ThreadEventBody, TurnOutcome, Workspace};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{BenchReport, Client, ClientError, EventSocket, Incoming};

const WAIT: Duration = Duration::from_secs(10);
const THREADS: usize = 10;
const TURNS: usize = 200;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnBenchReport {
    pub threads: usize,
    pub warmup_turns: usize,
    pub dispatch: BenchReport,
    pub completion: BenchReport,
    pub measurement: &'static str,
}

/// Measures 200 sequential turns on ten threads against an engine running fake-codex.
/// Both intervals include local transport, so they upper-bound engine overhead without adding
/// instrumentation to production. All timestamps come from this process's monotonic clock.
pub fn bench_turns(client: &Client, control_file: &Path) -> Result<TurnBenchReport, ClientError> {
    client.get("/api/usage")?.parse::<Value>()?;
    let mut fake = Control::connect(control_file)?;
    let observer = Observer::start(&mut fake)?;
    let threads = create_threads(client)?;
    let mut socket = client.connect_events(&[])?;
    for thread in &threads {
        socket.subscribe(&thread.id, Seq::BEFORE_FIRST)?;
    }
    let mut dispatch = Vec::with_capacity(THREADS + TURNS);
    let mut completion = Vec::with_capacity(THREADS + TURNS);
    for number in 0..THREADS + TURNS {
        let thread = &threads[number % THREADS];
        let (sent, completed) =
            measure_turn(client, thread, number, &observer, &mut fake, &mut socket)?;
        dispatch.push(sent);
        completion.push(completed);
    }
    Ok(TurnBenchReport {
        threads: THREADS,
        warmup_turns: THREADS,
        dispatch: BenchReport::from_latencies(dispatch.split_off(THREADS)),
        completion: BenchReport::from_latencies(completion.split_off(THREADS)),
        measurement: "Upper bounds: client POST to fake turn/start receipt; completion injection to client event. Includes localhost transport; excludes model time and ten warmup turns.",
    })
}

fn create_threads(client: &Client) -> Result<Vec<Thread>, ClientError> {
    let workspace: Workspace = client
        .post(
            "/api/workspaces",
            &json!({ "name": "turn benchmark", "path": std::env::temp_dir() }),
        )?
        .parse()?;
    (0..THREADS)
        .map(|number| {
            client.post("/api/threads", &json!({
                "workspaceId": workspace.id, "title": format!("bench {number}"), "provider": "codex"
            }))?.parse()
        })
        .collect()
}

fn measure_turn(
    client: &Client,
    thread: &Thread,
    number: usize,
    observer: &Observer,
    fake: &mut Control,
    socket: &mut EventSocket,
) -> Result<(Duration, Duration), ClientError> {
    let message_id = format!("bench-{number}");
    let posted_at = Instant::now();
    client
        .post(
            &format!("/api/threads/{}/messages", thread.id),
            &json!({ "messageId": message_id, "text": message_id }),
        )?
        .parse::<Value>()?;
    let (request, received_at) = observer
        .arrivals
        .recv_timeout(WAIT)
        .map_err(|_| ClientError::TimedOut(WAIT))??;
    if request["method"] != "turn/start" || request["params"]["input"][0]["text"] != message_id {
        return Err(ClientError::Bench(format!(
            "unexpected turn request: {request}"
        )));
    }
    let native_id = &request["params"]["threadId"];
    let turn_id = format!("bench-turn-{number}");
    fake.call(
        "test/settleTurn",
        json!({ "method": "turn/start", "threadId": native_id,
        "result": { "turn": { "id": turn_id, "status": "inProgress", "items": [] } } }),
    )?;
    let completion_sent = Instant::now();
    fake.call(
        "test/emit",
        json!({ "method": "turn/completed", "params": {
        "threadId": native_id, "turn": { "id": turn_id, "status": "completed", "items": [] }
    } }),
    )?;
    await_completion(socket, &thread.id, &turn_id)?;
    Ok((
        received_at.duration_since(posted_at),
        completion_sent.elapsed(),
    ))
}

fn await_completion(
    socket: &mut EventSocket,
    thread_id: &kratos_protocol::ThreadId,
    turn_id: &str,
) -> Result<(), ClientError> {
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(Incoming::Text(text)) = socket.next_incoming(remaining)? else {
            return Err(ClientError::TimedOut(WAIT));
        };
        let message: ServerMessage =
            serde_json::from_str(&text).map_err(|source| ClientError::Json {
                what: "benchmark event",
                source,
            })?;
        match message {
            ServerMessage::Event { event } => {
                if let ThreadEventBody::TurnCompleted {
                    turn_id: completed_id,
                    outcome,
                    ..
                } = event.event
                {
                    if &event.thread_id != thread_id
                        || completed_id.as_str() != turn_id
                        || outcome != TurnOutcome::Completed
                    {
                        return Err(ClientError::Bench(format!(
                            "unexpected completion: {completed_id}, {outcome:?}"
                        )));
                    }
                    return Ok(());
                }
            }
            ServerMessage::Error { message, .. } => return Err(ClientError::Engine(message)),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(ClientError::TimedOut(WAIT));
        }
    }
}

struct Control(BufReader<TcpStream>);

impl Control {
    fn connect(control_file: &Path) -> Result<Self, ClientError> {
        let port: u16 = std::fs::read_to_string(control_file)?
            .trim()
            .parse()
            .map_err(|_| ClientError::Bench("fake-codex's control file is not a port".into()))?;
        let stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(WAIT))?;
        stream.set_write_timeout(Some(WAIT))?;
        Ok(Self(BufReader::new(stream)))
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        writeln!(
            self.0.get_mut(),
            "{}",
            json!({ "id": 1, "method": method, "params": params })
        )?;
        let mut line = String::new();
        self.0.read_line(&mut line)?;
        let response: Value = serde_json::from_str(&line).map_err(|source| ClientError::Json {
            what: "fake-codex control response",
            source,
        })?;
        if response.get("error").is_some() || response.get("result").is_none() {
            return Err(ClientError::Bench(format!("{method} failed: {response}")));
        }
        Ok(response["result"].clone())
    }
}

type Arrival = Result<(Value, Instant), ClientError>;

struct Observer {
    stream: TcpStream,
    arrivals: mpsc::Receiver<Arrival>,
}

impl Observer {
    fn start(fake: &mut Control) -> Result<Self, ClientError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        fake.call(
            "test/observeTurns",
            json!({ "port": listener.local_addr()?.port() }),
        )?;
        let (stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(WAIT))?;
        let reader = BufReader::new(stream.try_clone()?);
        let (send, arrivals) = mpsc::channel();
        thread::spawn(move || {
            for line in reader.lines() {
                let received_at = Instant::now();
                let arrival = line.map_err(ClientError::Io).and_then(|line| {
                    serde_json::from_str(&line)
                        .map(|request| (request, received_at))
                        .map_err(|source| ClientError::Json {
                            what: "observed turn request",
                            source,
                        })
                });
                if send.send(arrival).is_err() {
                    break;
                }
            }
        });
        Ok(Self { stream, arrivals })
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}
