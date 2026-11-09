//! What the client reads of the account's limits and of a turn's context, against synthetic
//! frames in the shapes measured from the CLI and against the recorded launch.

use std::path::Path;

use kratos_claude_client::{
    Control, Initialize, Launch, RateLimitStatus, RateLimitType, RequestError, SessionId,
    SessionStart, StreamMessage, TurnResult, UsageRead, connect,
};
use serde_json::{Value, json};

fn recorded_result() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/s1-launch-and-stream/wire.jsonl");
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["msg"].clone())
        .find(|message| message["type"] == "result")
        .expect("the recording holds a result")
}

fn rate_limit(status: &str, kind: &str) -> StreamMessage {
    StreamMessage::parse(json!({
        "type": "rate_limit_event",
        "rate_limit_info": { "status": status, "rateLimitType": kind, "resetsAt": 1_800_000_000_u64 },
    }))
    .unwrap()
}

#[test]
fn a_rate_limit_status_and_type_the_crate_does_not_know_are_kept_as_text() {
    // Catches a self-updating CLI's new status or limit type failing the whole frame, which would
    // hide a rejection, or being read as one of the known values.
    let StreamMessage::RateLimit(known) = rate_limit("rejected", "seven_day_sonnet") else {
        panic!("a rate limit event");
    };
    assert_eq!(known.rate_limit_info.status, RateLimitStatus::Rejected);
    assert_eq!(
        known.rate_limit_info.rate_limit_type,
        Some(RateLimitType::SevenDaySonnet)
    );
    let StreamMessage::RateLimit(unknown) = rate_limit("throttled", "monthly") else {
        panic!("a rate limit event");
    };
    assert_eq!(
        unknown.rate_limit_info.status,
        RateLimitStatus::Unrecognized("throttled".into())
    );
    assert_eq!(
        unknown.rate_limit_info.rate_limit_type,
        Some(RateLimitType::Unrecognized("monthly".into()))
    );
}

#[test]
fn every_known_status_and_limit_type_maps_to_its_own_variant() {
    // Catches two names mapped to one variant, such as a warning read as a plain allowance.
    let statuses = [
        ("allowed", RateLimitStatus::Allowed),
        ("allowed_warning", RateLimitStatus::AllowedWarning),
        ("rejected", RateLimitStatus::Rejected),
    ];
    for (text, status) in statuses {
        assert_eq!(RateLimitStatus::from(text.to_owned()), status);
    }
    let kinds = [
        ("five_hour", RateLimitType::FiveHour),
        ("seven_day", RateLimitType::SevenDay),
        ("seven_day_opus", RateLimitType::SevenDayOpus),
        ("seven_day_sonnet", RateLimitType::SevenDaySonnet),
        (
            "seven_day_overage_included",
            RateLimitType::SevenDayOverageIncluded,
        ),
        ("overage", RateLimitType::Overage),
    ];
    for (text, kind) in kinds {
        assert_eq!(RateLimitType::from(text.to_owned()), kind);
        assert_eq!(kind.as_str(), text);
    }
}

#[test]
fn a_usage_read_keeps_the_percent_and_the_offset_text_and_ignores_the_other_keys() {
    // Catches a read that fails on the null codename keys, or rewrites the reset text before the
    // engine can convert it.
    let read: UsageRead = serde_json::from_value(json!({
        "subscription_type": "pro",
        "rate_limits_available": true,
        "session": { "total_cost_usd": 0 },
        "behaviors": null,
        "rate_limits": {
            "five_hour": { "utilization": 19, "resets_at": "2030-01-01T05:00:00.123456+00:00", "spare": null },
            "seven_day": { "utilization": 7.5, "resets_at": null },
            "seven_day_opus": null,
            "synthetic_codename": null,
        },
    }))
    .unwrap();

    assert_eq!(read.subscription_type.as_deref(), Some("pro"));
    assert!(read.rate_limits_available);
    let limits = read.rate_limits.unwrap();
    let five = limits.five_hour.unwrap();
    assert_eq!(five.utilization, Some(19.0));
    assert_eq!(
        five.resets_at.as_deref(),
        Some("2030-01-01T05:00:00.123456+00:00")
    );
    assert_eq!(limits.seven_day.unwrap().resets_at, None);
    assert_eq!(limits.seven_day_opus, None);
}

#[test]
fn an_api_retry_frame_is_typed_and_not_a_result() {
    // Catches a retry the CLI makes by itself being read as an unknown frame, where nothing could
    // tell it from silence.
    let message = StreamMessage::parse(json!({
        "type": "system", "subtype": "api_retry", "attempt": 2, "max_retries": 10,
        "retry_delay_ms": 4000, "error_status": 429, "error": "rate_limit",
        "uuid": "00000000-0000-4000-8000-000000000001",
        "session_id": "00000000-0000-4000-8000-000000000002",
    }))
    .unwrap();

    let StreamMessage::ApiRetry(retry) = message else {
        panic!("an api retry");
    };
    assert_eq!((retry.attempt, retry.max_retries), (2, 10));
    assert_eq!(retry.error_status, Some(429));
    assert_eq!(retry.error, "rate_limit");
}

#[test]
fn a_result_reports_the_last_request_and_the_answering_models_window() {
    // Catches context summed over every request of the turn, which would pass the window, or
    // read from the cumulative usage instead of the request that fills it.
    let result = TurnResult::parse(recorded_result()).unwrap();

    let last = result.context.last.unwrap();
    assert_eq!(
        (
            last.input_tokens,
            last.cache_creation_input_tokens,
            last.cache_read_input_tokens,
            last.output_tokens
        ),
        (2, 6851, 15768, 4)
    );
    assert_eq!(result.context.window_of(None), Some(1_000_000));
}

#[test]
fn a_result_without_requests_has_no_context_and_several_models_need_the_answering_one() {
    // Catches zeros reported for a turn that listed no request, and a window guessed from the
    // wrong model when a result names several.
    let none = TurnResult::parse(json!({ "subtype": "success", "is_error": false })).unwrap();
    assert_eq!(none.context.last, None);
    assert_eq!(none.context.window_of(Some("a")), None);

    let two = TurnResult::parse(json!({
        "subtype": "success", "is_error": false,
        "modelUsage": { "model-a": { "contextWindow": 200_000 }, "model-b": { "contextWindow": 1_000_000 } },
    }))
    .unwrap();
    assert_eq!(two.context.window_of(Some("model-a")), Some(200_000));
    assert_eq!(two.context.window_of(Some("model-c")), None);
    assert_eq!(two.context.window_of(None), None);
}

#[tokio::test]
async fn get_usage_asks_without_the_transcript_scan_and_a_refused_control_is_not_a_timeout() {
    // Catches a usage read that scans every transcript of the week, and a CLI's refusal reported
    // as a timeout, which would make the engine retry as if the outcome were unknown.
    #[cfg(windows)]
    kratos_process::contain_current_process().unwrap();
    let id: SessionId = "00000000-0000-4000-8000-000000000001".parse().unwrap();
    let launch = Launch::new(
        fake_agents::executable("fake-claude"),
        std::env::temp_dir(),
        SessionStart::New(id),
    );
    let (claude, _incoming, _) = connect(launch, Initialize::default(), drop).await.unwrap();

    let read = claude.usage().await.unwrap();
    let refused = claude
        .control(Control::Raw(json!({ "subtype": "not_a_control" })))
        .await
        .unwrap_err();
    let inspected = claude
        .control(Control::Raw(json!({ "subtype": "test/inspect" })))
        .await
        .unwrap();

    assert_eq!(read.subscription_type.as_deref(), Some("pro"));
    assert!(matches!(refused, RequestError::Rejected(_)), "{refused:?}");
    let request = inspected["received"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["request"]["subtype"] == "get_usage")
        .unwrap();
    assert_eq!(request["request"]["skip_behaviors"], true);
    claude.close().await;
}

/// A result whose requests differ from each other and from the cumulative top level, so reading
/// the wrong one gives a different number: the first request, the sum, and the top level each
/// differ from the last request.
fn result_with_two_requests() -> Value {
    json!({
        "subtype": "success", "is_error": false,
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
    })
}

#[test]
fn the_context_is_the_last_request_not_the_first_the_sum_or_the_top_level() {
    // Catches the three ways to read the wrong request: the first of several, the sum over the
    // turn, or the cumulative top-level usage. Only the last request fills the window.
    let result = TurnResult::parse(result_with_two_requests()).unwrap();

    let last = result.context.last.unwrap();

    assert_eq!(
        (
            last.input_tokens,
            last.cache_creation_input_tokens,
            last.cache_read_input_tokens,
            last.output_tokens
        ),
        (1, 2, 3, 4)
    );
}

#[test]
fn an_api_retry_frame_that_fails_the_typed_parse_is_kept_as_an_unrecognized_frame() {
    // Catches a self-updating CLI that renames a field of a frame Kratos only watches: the frame
    // would become an error on the running turn instead of being kept as it came.
    let message = StreamMessage::parse(json!({
        "type": "system", "subtype": "api_retry", "attempt": 2, "max_retries": 10,
        "delay_ms": 4000, "error_status": 429, "error": "rate_limit",
    }));

    assert!(
        matches!(&message, Ok(StreamMessage::Unrecognized { kind, .. }) if kind == "system"),
        "{message:?}"
    );
}
