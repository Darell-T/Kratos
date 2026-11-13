//! The context a Claude thread fills (D76): what each request reports while a turn runs, and
//! what a turn's result says when it ends, over a real store, driving the scripted `fake-claude`.

use kratos_protocol::{ContextUsage, LiveUpdate, ThreadEventBody, TurnId};
use serde_json::{Value, json};

use crate::harness::{A, B, Harness, Options};
use crate::usage::{recorded, second_turn};

#[tokio::test]
async fn a_result_reports_the_last_requests_context_in_the_turn_and_live() {
    // Catches context lost at the turn's end, the cache counted outside the input, or a window
    // taken from the wrong place (D76).
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    let (_, mut live) = harness.hub.watch_live(&harness.thread_id);
    let result = recorded("result");

    harness
        .finish(
            &launch,
            &[A],
            json!({ "usage": result["usage"], "modelUsage": result["modelUsage"] }),
        )
        .await;

    let expected = ContextUsage {
        input_tokens: 22_621,
        cached_input_tokens: 15_768,
        output_tokens: 4,
        reasoning_output_tokens: 0,
        total_tokens: 22_625,
        context_window: Some(1_000_000),
    };
    let update = tokio::time::timeout(crate::harness::WAIT, live.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        update,
        LiveUpdate::Context {
            turn_id: TurnId::new(A.to_owned()),
            usage: expected.clone(),
        }
    );
    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let ThreadEventBody::TurnCompleted { context, .. } = ended else {
        unreachable!("wait_for picked a turn end");
    };
    assert_eq!(context, Some(expected));
}

#[tokio::test]
async fn a_result_without_requests_reports_no_context() {
    // Catches zeros reported for a turn whose result lists no request, which a client would draw
    // as an empty context window.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;

    harness.finish(&launch, &[A], json!({})).await;

    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let ThreadEventBody::TurnCompleted { context, .. } = ended else {
        unreachable!("wait_for picked a turn end");
    };
    assert_eq!(context, None);
}

#[tokio::test]
async fn the_context_is_the_last_request_not_the_first_the_sum_or_the_top_level() {
    // Catches the three ways to read the wrong request: the first of several, the sum over the
    // turn, or the cumulative top-level usage. Only the last request fills the window.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;

    harness
        .finish(
            &launch,
            &[A],
            json!({
                "modelUsage": { "model-a": { "contextWindow": 200_000 } },
                "usage": {
                    "input_tokens": 900, "cache_creation_input_tokens": 800,
                    "cache_read_input_tokens": 700, "output_tokens": 600,
                    "iterations": [
                        { "input_tokens": 10, "cache_creation_input_tokens": 20,
                          "cache_read_input_tokens": 30, "output_tokens": 40 },
                        { "input_tokens": 1, "cache_creation_input_tokens": 2,
                          "cache_read_input_tokens": 3, "output_tokens": 4 },
                    ],
                },
            }),
        )
        .await;

    let ended = harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let ThreadEventBody::TurnCompleted { context, .. } = ended else {
        unreachable!("wait_for picked a turn end");
    };
    assert_eq!(
        context,
        Some(ContextUsage {
            input_tokens: 6,
            cached_input_tokens: 3,
            output_tokens: 4,
            reasoning_output_tokens: 0,
            total_tokens: 10,
            context_window: Some(200_000),
        })
    );
}

#[tokio::test]
async fn a_turn_with_no_reply_does_not_take_the_previous_turns_model_for_its_window() {
    // Catches a window read from the model of an earlier turn: a turn that wrote no reply, with
    // several models in its result, has no answering model and so no window.
    let mut harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    harness
        .emit(
            &launch,
            json!({ "type": "assistant", "uuid": "u1", "session_id": "s",
                    "message": { "id": "msg_1", "model": "model-a",
                                 "content": [{ "type": "text", "text": "hi" }] } }),
        )
        .await;
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    second_turn(&harness, &launch).await;
    harness
        .finish(
            &launch,
            &[B],
            json!({
                "modelUsage": { "model-a": { "contextWindow": 200_000 },
                                "model-b": { "contextWindow": 1_000_000 } },
                "usage": { "iterations": [{ "input_tokens": 1, "output_tokens": 1 }] },
            }),
        )
        .await;

    let ended = harness
        .wait_for(|event| {
            matches!(event, ThreadEventBody::TurnCompleted { turn_id, .. } if turn_id.as_str() == B)
        })
        .await;
    let ThreadEventBody::TurnCompleted { context, .. } = ended else {
        unreachable!("wait_for picked a turn end");
    };
    assert_eq!(context.unwrap().context_window, None);
}

/// A stream event of one request, carrying the tokens it has used so far.
fn request_event(kind: &str, input: i64, cache_read: i64, output: i64) -> Value {
    let usage = json!({ "input_tokens": input, "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": cache_read, "output_tokens": output });
    let event = if kind == "message_start" {
        json!({ "type": kind, "message": { "usage": usage } })
    } else {
        json!({ "type": kind, "delta": {}, "usage": usage })
    };
    json!({ "type": "stream_event", "session_id": "s", "event": event })
}

async fn next_context(live: &mut tokio::sync::broadcast::Receiver<LiveUpdate>) -> ContextUsage {
    loop {
        let update = tokio::time::timeout(crate::harness::WAIT, live.recv())
            .await
            .expect("a live update arrives")
            .expect("the live feed stays open");
        if let LiveUpdate::Context { usage, .. } = update {
            return usage;
        }
    }
}

fn context(input: i64, cached: i64, output: i64, window: Option<i64>) -> ContextUsage {
    ContextUsage {
        input_tokens: input,
        cached_input_tokens: cached,
        output_tokens: output,
        reasoning_output_tokens: 0,
        total_tokens: input + output,
        context_window: window,
    }
}

#[tokio::test]
async fn each_request_reports_its_context_live_before_the_turn_ends() {
    // Catches context shown only when a turn ends, a request's cache counted outside its input,
    // and a window guessed before any turn reported one or forgotten after one did (D76).
    let harness = Harness::start(Options::default()).await;
    let launch = harness.begin(A, 1).await;
    let (_, mut live) = harness.hub.watch_live(&harness.thread_id);

    harness
        .emit(&launch, request_event("message_start", 10, 1_000, 1))
        .await;
    assert_eq!(
        next_context(&mut live).await,
        context(1_010, 1_000, 1, None)
    );
    harness
        .emit(&launch, request_event("message_delta", 10, 1_000, 40))
        .await;
    assert_eq!(
        next_context(&mut live).await,
        context(1_010, 1_000, 40, None)
    );

    let result = recorded("result");
    harness
        .finish(
            &launch,
            &[A],
            json!({ "usage": result["usage"], "modelUsage": result["modelUsage"] }),
        )
        .await;
    assert_eq!(
        next_context(&mut live).await.context_window,
        Some(1_000_000)
    );
    harness.send(B, B).await;
    harness.wait_written(&launch, &[A, B]).await;
    harness.accept(&launch, B).await;
    harness
        .emit(&launch, request_event("message_start", 20, 2_000, 1))
        .await;

    assert_eq!(
        next_context(&mut live).await,
        context(2_020, 2_000, 1, Some(1_000_000))
    );
}
