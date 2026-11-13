//! R4's controlled perf probe: how long the Claude provider takes to hand a message to the CLI,
//! and to forward a streamed delta to a live subscriber, against the scripted fake. Ignored by
//! default; run with `cargo test -p kratos-engine --test claude perf -- --ignored --nocapture`.
//! The budgets were declared before the first measurement.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kratos_protocol::{LiveUpdate, ThreadEventBody};
use serde_json::json;

use crate::harness::{A, Harness, Options};

const SAMPLES: usize = 200;

fn percentile(samples: &mut [Duration], at: f64) -> Duration {
    samples.sort();
    let index = ((samples.len() as f64 - 1.0) * at).round() as usize;
    samples[index]
}

fn report(name: &str, samples: &mut [Duration], p50_budget: u64, p95_budget: u64) {
    let (p50, p95) = (percentile(samples, 0.50), percentile(samples, 0.95));
    println!(
        "{name}: n={} p50={:.2}ms p95={:.2}ms max={:.2}ms (budget p50<={p50_budget}ms p95<={p95_budget}ms)",
        samples.len(),
        p50.as_secs_f64() * 1e3,
        p95.as_secs_f64() * 1e3,
        samples.last().unwrap().as_secs_f64() * 1e3,
    );
    assert!(
        p50 <= Duration::from_millis(p50_budget),
        "{name} p50 over budget"
    );
    assert!(
        p95 <= Duration::from_millis(p95_budget),
        "{name} p95 over budget"
    );
}

#[tokio::test]
#[ignore = "perf probe, run by hand"]
async fn perf_stream_forwarding_and_dispatch_stay_inside_their_budgets() {
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    let (_, mut live) = harness.hub.watch_live(&harness.thread_id);

    let mut forwarding = Vec::with_capacity(SAMPLES);
    for index in 0..SAMPLES {
        let text = format!("delta-{index} ");
        let sent = Instant::now();
        harness
            .emit(
                &launch,
                json!({ "type": "stream_event", "api_message_id": "msg_1", "session_id": "s",
                        "event": { "type": "content_block_delta", "index": 0,
                                   "delta": { "type": "text_delta", "text": text } } }),
            )
            .await;
        loop {
            if let LiveUpdate::ReplyDelta { delta, .. } = live.recv().await.unwrap()
                && delta == text
            {
                break;
            }
        }
        forwarding.push(sent.elapsed());
    }
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    // Dispatch ends when the fake reads the message, by the fake's own clock, so how often the
    // probe polls for it does not count.
    let mut dispatch = Vec::with_capacity(SAMPLES);
    let mut written = vec![A.to_owned()];
    for index in 0..SAMPLES {
        let id = format!("00000000-0000-4000-8000-{:012}", 1_000 + index);
        let sent = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        harness.send(&id, "next").await;
        written.push(id.clone());
        let expected: Vec<&str> = written.iter().map(String::as_str).collect();
        harness.wait_written(&launch, &expected).await;
        let received = harness.received(&launch).await;
        let arrived = harness.call(&launch, "test/receivedAt", json!({})).await;
        let at = received
            .iter()
            .position(|frame| frame["type"] == "user" && frame["uuid"] == id.as_str())
            .unwrap();
        let micros = arrived[at].as_u64().unwrap();
        dispatch.push(Duration::from_micros(micros) - sent);
        harness.accept(&launch, &id).await;
        harness.finish(&launch, &[&id], json!({})).await;
        harness
            .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { turn_id, .. } if turn_id.as_str() == id))
            .await;
    }

    assert_eq!(harness.launches().len(), 1, "one process served every turn");
    report("stream forwarding", &mut forwarding, 20, 50);
    report("dispatch", &mut dispatch, 50, 100);
}
