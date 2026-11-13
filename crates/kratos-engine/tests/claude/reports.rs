//! Forced report formats: the schema reaches Claude when its session starts, and the parsed reply
//! becomes the turn's report.

use kratos_protocol::{AgentItem, ReportFormat, ThreadEventBody, ThreadSettings};
use serde_json::{Value, json};

use crate::harness::{A, B, Harness, Options};

fn schema() -> Value {
    json!({ "type": "object", "properties": { "verdict": { "type": "string" } },
            "required": ["verdict"] })
}

fn reporting(plan_mode: bool) -> ThreadSettings {
    ThreadSettings {
        plan_mode,
        report_format: Some(ReportFormat::new(schema().as_object().unwrap().clone())),
        ..ThreadSettings::default()
    }
}

#[tokio::test]
async fn a_report_format_reaches_claude_at_start_and_its_parsed_reply_is_the_turn_s_report() {
    // Catches a schema Claude never receives, and a structured reply dropped or recorded after
    // its turn ended.
    let mut harness = Harness::start(Options {
        settings: reporting(false),
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    assert_eq!(
        harness.received(&launch).await[0]["request"]["jsonSchema"],
        schema()
    );

    harness
        .finish(
            &launch,
            &[A],
            json!({ "structured_output": { "verdict": "approve" } }),
        )
        .await;

    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    let log = harness.log().await;
    let report = log.iter().position(|event| {
        matches!(event, ThreadEventBody::ItemCompleted { item: AgentItem::Report { report, .. }, .. }
            if *report == json!({ "verdict": "approve" }))
    });
    let completed = log
        .iter()
        .position(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }));
    assert!(report.is_some() && report < completed, "{log:?}");
}

#[tokio::test]
async fn a_planning_turn_runs_without_the_report_format() {
    // Catches a plan forced into a report's shape, which Claude cannot write as a plan.
    let harness = Harness::start(Options {
        settings: reporting(true),
        ..Options::default()
    })
    .await;

    let launch = harness.begin(A, 1).await;

    let initialize = &harness.received(&launch).await[0]["request"];
    assert_eq!(initialize["subtype"], "initialize");
    assert!(initialize.get("jsonSchema").is_none(), "{initialize}");
}

#[tokio::test]
async fn a_new_report_format_starts_a_new_process_that_resumes_the_thread() {
    // Catches a changed report format that never reaches Claude, which reads it only at start.
    let mut harness = Harness::start(Options::default()).await;
    let first = harness.begin(A, 1).await;
    harness.finish(&first, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;

    harness
        .provider
        .set_settings(harness.thread_id.clone(), reporting(false))
        .await
        .unwrap();
    harness.send(B, B).await;

    let second = harness.launch(2).await;
    assert_eq!(
        second.value("--resume"),
        Some(first.session().to_string().as_str())
    );
    harness.wait_written(&second, &[B]).await;
    assert_eq!(
        harness.received(&second).await[0]["request"]["jsonSchema"],
        schema()
    );
}
