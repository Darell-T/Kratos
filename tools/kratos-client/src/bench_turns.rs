use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use fake_agents::codex_control::{self, ControlRequest};
use kratos_api_client::{Client, ClientError, EventSocket, Incoming};
use kratos_protocol::{
    CreateThread, Provider, RequiredText, Seq, ServerMessage, Thread, ThreadEventBody, TurnOutcome,
    routes,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::bench::temp_workspace;
use crate::{BenchError, BenchReport};

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
pub fn bench_turns(client: &Client, control_file: &Path) -> Result<TurnBenchReport, BenchError> {
    client.get(routes::USAGE)?.parse::<Value>()?;
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

fn create_threads(client: &Client) -> Result<Vec<Thread>, BenchError> {
    let workspace = client.create_workspace(&temp_workspace("turn benchmark"))?;
    (0..THREADS)
        .map(|number| {
            client.create_thread(&CreateThread {
                workspace_id: workspace.id.clone(),
                title: RequiredText::new(&format!("bench {number}"))
                    .expect("the title is not blank"),
                provider: Provider::Codex,
                settings: None,
            })
        })
        .collect::<Result<_, _>>()
        .map_err(BenchError::from)
}

fn measure_turn(
    client: &Client,
    thread: &Thread,
    number: usize,
    observer: &Observer,
    fake: &mut Control,
    socket: &mut EventSocket,
) -> Result<(Duration, Duration), BenchError> {
    let message_id = format!("bench-{number}");
    let posted_at = Instant::now();
    client
        .post(
            &routes::thread_messages(&thread.id),
            &json!({ "messageId": message_id, "text": message_id }),
        )?
        .parse::<Value>()?;
    let (request, received_at) = observer
        .arrivals
        .recv_timeout(WAIT)
        .map_err(|_| BenchError::from(ClientError::TimedOut(WAIT)))??;
    let native_id = started_thread(&request, &message_id)?;
    let turn_id = format!("bench-turn-{number}");
    let settled = codex_control::settle_start(&turn_id, "inProgress", Some(native_id));
    fake.call(settled)?;
    let completion_sent = Instant::now();
    let completed = codex_control::complete(native_id, &turn_id, "completed");
    fake.call(completed)?;
    await_completion(socket, &thread.id, &turn_id)?;
    Ok((
        received_at.duration_since(posted_at),
        completion_sent.elapsed(),
    ))
}

fn started_thread<'a>(request: &'a Value, message_id: &str) -> Result<&'a str, BenchError> {
    let expected =
        request["method"] == "turn/start" && request["params"]["input"][0]["text"] == message_id;
    match request["params"]["threadId"].as_str() {
        Some(native_id) if expected => Ok(native_id),
        _ => Err(BenchError::Failed(format!(
            "unexpected turn request: {request}"
        ))),
    }
}

fn await_completion(
    socket: &mut EventSocket,
    thread_id: &kratos_protocol::ThreadId,
    turn_id: &str,
) -> Result<(), BenchError> {
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let text = match socket.next_incoming(remaining)? {
            Some(Incoming::Text(text)) => text,
            Some(Incoming::Closed(code)) => {
                let code = code.map_or("no close code".to_owned(), |code| format!("code {code}"));
                return Err(BenchError::Failed(format!(
                    "the engine closed the event socket ({code}) before turn {turn_id} completed"
                )));
            }
            None => return Err(ClientError::TimedOut(WAIT).into()),
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
                    if (&event.thread_id, completed_id.as_str()) != (thread_id, turn_id)
                        || outcome != TurnOutcome::Completed
                    {
                        return Err(BenchError::Failed(format!(
                            "unexpected completion: {completed_id}, {outcome:?}"
                        )));
                    }
                    return Ok(());
                }
            }
            ServerMessage::Error { message, .. } => {
                return Err(ClientError::Engine(message).into());
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(ClientError::TimedOut(WAIT).into());
        }
    }
}

struct Control(BufReader<TcpStream>);

impl Control {
    fn connect(control_file: &Path) -> Result<Self, BenchError> {
        let port = codex_control::wait_for_port(control_file, WAIT)
            .map_err(|error| BenchError::Failed(error.to_string()))?;
        let stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(WAIT))?;
        stream.set_write_timeout(Some(WAIT))?;
        Ok(Self(BufReader::new(stream)))
    }

    fn call(
        &mut self,
        ControlRequest { method, params }: ControlRequest,
    ) -> Result<Value, BenchError> {
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
            return Err(BenchError::Failed(format!("{method} failed: {response}")));
        }
        Ok(response["result"].clone())
    }
}

type Arrival = Result<(Value, Instant), BenchError>;

struct Observer {
    stream: TcpStream,
    arrivals: mpsc::Receiver<Arrival>,
}

impl Observer {
    fn start(fake: &mut Control) -> Result<Self, BenchError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        fake.call(ControlRequest {
            method: "test/observeTurns",
            params: json!({ "port": listener.local_addr()?.port() }),
        })?;
        let (stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(WAIT))?;
        let reader = BufReader::new(stream.try_clone()?);
        let (send, arrivals) = mpsc::channel();
        thread::spawn(move || {
            for line in reader.lines() {
                let received_at = Instant::now();
                let arrival = line.map_err(BenchError::from).and_then(|line| {
                    serde_json::from_str(&line)
                        .map(|request| (request, received_at))
                        .map_err(|source| {
                            BenchError::from(ClientError::Json {
                                what: "observed turn request",
                                source,
                            })
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

#[cfg(test)]
mod tests {
    use kratos_protocol::ThreadId;

    use super::*;

    #[test]
    fn control_waits_for_a_fake_codex_that_starts_late() {
        // Catches a benchmark that fails when Codex has not yet written its control port, because
        // it read the port file once.
        let dir = tempfile::tempdir().unwrap();
        let control_file = dir.path().join("fake-codex-control");
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let late_file = control_file.clone();
        let starter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            codex_control::write_port(&late_file, port).unwrap();
        });

        let connected = Control::connect(&control_file);

        starter.join().unwrap();
        assert!(connected.is_ok(), "{:?}", connected.err());
    }

    #[test]
    fn a_missing_control_port_reports_its_file_when_the_deadline_expires() {
        // Catches a shared wait returning a raw missing-file error or losing the control path.
        let dir = tempfile::tempdir().unwrap();
        let control_file = dir.path().join("missing-control");

        let error = codex_control::wait_for_port(&control_file, Duration::ZERO).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("missing-control"), "{error}");
    }

    #[test]
    fn a_closed_event_socket_is_reported_as_closed_not_as_a_timeout() {
        // Catches a received close frame being reported as a timeout or with debug Option codes.
        use tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

        for (frame, diagnostic) in [
            (None, "(no close code)"),
            (
                Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "".into(),
                }),
                "(code 1000)",
            ),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                socket.close(frame).unwrap();
                while socket.read().is_ok() {}
            });
            let mut socket = Client::new(&format!("http://{address}"))
                .unwrap()
                .connect_events(&[])
                .unwrap();

            let error = await_completion(&mut socket, &ThreadId::new("thread".to_owned()), "turn")
                .unwrap_err()
                .to_string();

            drop(socket);
            server.join().unwrap();
            assert!(error.contains("closed"), "{error}");
            assert!(!error.contains("nothing arrived"), "{error}");
            assert!(error.contains(diagnostic), "{error}");
        }
    }
}
