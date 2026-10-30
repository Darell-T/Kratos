use std::num::NonZeroUsize;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use kratos_protocol::{
    CreateThread, CreateWorkspace, Provider, RequiredText, Seq, ServerMessage, ThreadEvent,
    ThreadId, routes,
};
use serde::Serialize;
use serde_json::json;

use kratos_api_client::{Client, EventSocket, Incoming};

use crate::BenchError;

const ARRIVAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Post-to-event latency in milliseconds, printed as one JSON line.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchReport {
    pub messages: usize,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// Creates a workspace on the system temp folder and a thread, subscribes a socket to the thread,
/// then posts `messages` messages one after another, each after the previous reply. Each message
/// is timed from just before its POST to the arrival of its event on the socket.
///
/// The run fails, and prints no number, when an event is missing, repeated, or out of order.
pub fn bench_post(client: &Client, messages: NonZeroUsize) -> Result<BenchReport, BenchError> {
    let thread_id = create_thread(client)?;
    let mut socket = client.connect_events(&[])?;
    socket.subscribe(&thread_id, Seq::BEFORE_FIRST)?;
    let (arrivals, arrived) = mpsc::channel();
    // The reader stamps each event on its own thread, so a slow reply to a post never delays a
    // stamp.
    let reader = thread::spawn(move || read_arrivals(socket, messages.get(), &arrivals));

    let starts = post_all(client, &thread_id, messages.get())?;
    let latencies = match_arrivals(&starts, &arrived)?;
    reader
        .join()
        .map_err(|_| BenchError::Failed("the socket reader panicked".into()))?;
    Ok(BenchReport::from_latencies(latencies))
}

fn create_thread(client: &Client) -> Result<ThreadId, BenchError> {
    let workspace = client.create_workspace(&temp_workspace("bench"))?;
    // A Claude thread keeps R2's plain message log, the path this probe measures.
    let thread = client.create_thread(&CreateThread {
        workspace_id: workspace.id,
        title: RequiredText::new("bench").expect("the title is not blank"),
        provider: Provider::Claude,
        settings: None,
    })?;
    Ok(thread.id)
}

/// A workspace named `name` on the system's temporary folder.
pub(crate) fn temp_workspace(name: &str) -> CreateWorkspace {
    CreateWorkspace {
        name: RequiredText::new(name).expect("the name is not blank"),
        path: RequiredText::new(&std::env::temp_dir().to_string_lossy())
            .expect("the temporary folder has a path"),
    }
}

fn post_all(
    client: &Client,
    thread_id: &ThreadId,
    messages: usize,
) -> Result<Vec<Instant>, BenchError> {
    let path = routes::thread_messages(thread_id);
    let mut starts = Vec::with_capacity(messages);
    for number in 1..=messages {
        starts.push(Instant::now());
        let body = json!({ "text": format!("bench {number}") });
        let posted: ThreadEvent = client.post(&path, &body)?.parse()?;
        expect_seq(posted.seq, number)?;
    }
    Ok(starts)
}

fn match_arrivals(
    starts: &[Instant],
    arrived: &Receiver<Arrival>,
) -> Result<Vec<Duration>, BenchError> {
    let mut latencies = Vec::with_capacity(starts.len());
    for (index, start) in starts.iter().enumerate() {
        let silent = || BenchError::Failed(format!("no event arrived within {ARRIVAL_TIMEOUT:?}"));
        let (seq, at) = arrived
            .recv_timeout(ARRIVAL_TIMEOUT)
            .map_err(|_| silent())??;
        expect_seq(seq, index + 1)?;
        latencies.push(at.saturating_duration_since(*start));
    }
    Ok(latencies)
}

fn expect_seq(seq: Seq, expected: usize) -> Result<(), BenchError> {
    if seq.get() == expected as u64 {
        return Ok(());
    }
    Err(BenchError::Failed(format!(
        "event {} arrived where event {expected} belongs",
        seq.get()
    )))
}

type Arrival = Result<(Seq, Instant), BenchError>;

fn read_arrivals(mut socket: EventSocket, expected: usize, arrivals: &Sender<Arrival>) {
    for _ in 0..expected {
        let incoming = socket.next_incoming(ARRIVAL_TIMEOUT);
        let at = Instant::now();
        let arrival = match incoming {
            Ok(Some(Incoming::Text(text))) => match serde_json::from_str(&text) {
                Ok(ServerMessage::Event { event }) => Ok((event.seq, at)),
                _ => Err(BenchError::Failed(format!("expected an event, got {text}"))),
            },
            Ok(other) => Err(BenchError::Failed(format!(
                "expected an event, got {other:?}"
            ))),
            Err(error) => Err(error.into()),
        };
        let failed = arrival.is_err();
        if arrivals.send(arrival).is_err() || failed {
            return;
        }
    }
}

impl BenchReport {
    /// Nearest-rank percentiles of a non-empty list of samples: the p-th percentile of n samples is
    /// the sample at rank `ceil(p / 100 * n)`, counting from 1 in sorted order.
    pub fn from_latencies(mut latencies: Vec<Duration>) -> BenchReport {
        latencies.sort();
        let milliseconds = |percent: usize| {
            let rank = (percent * latencies.len()).div_ceil(100);
            let microseconds = latencies[rank - 1].as_secs_f64() * 1_000_000.0;
            microseconds.round() / 1_000.0
        };
        BenchReport {
            messages: latencies.len(),
            p50_ms: milliseconds(50),
            p90_ms: milliseconds(90),
            p95_ms: milliseconds(95),
            p99_ms: milliseconds(99),
            max_ms: milliseconds(100),
        }
    }
}
