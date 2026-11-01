//! What agents hand back. The schemas both CLIs force on a final reply must stay inside the strict
//! subset they accept, and a reply that cannot be used must become an error the agent can act on,
//! without a panic.

use kratos_protocol::TurnChanges;
use kratos_teams::reports::{
    FinalReply, FindingId, Report, ReportKind, Reported, SchemaError, Verdict, parse_report,
    report_json_schema, strict_json_schema,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

const KINDS: [ReportKind; 5] = [
    ReportKind::Plan,
    ReportKind::Implementer,
    ReportKind::Review,
    ReportKind::Answer,
    ReportKind::Rulings,
];

fn changes() -> TurnChanges {
    TurnChanges {
        files: vec![],
        edited_since_command: false,
        ran_command: true,
    }
}

fn objects(node: &Value) -> Vec<&serde_json::Map<String, Value>> {
    let Some(map) = node.as_object() else {
        return node
            .as_array()
            .map(|items| items.iter().flat_map(objects).collect())
            .unwrap_or_default();
    };
    let inner: Vec<_> = map.values().flat_map(objects).collect();
    if map.contains_key("properties") {
        std::iter::once(map).chain(inner).collect()
    } else {
        inner
    }
}

#[test]
fn every_report_schema_requires_every_property_and_allows_no_extras() {
    for kind in KINDS {
        let schema = report_json_schema(kind);
        let all = objects(schema);
        assert!(!all.is_empty(), "{kind:?} has no object");
        for object in all {
            assert_eq!(object["additionalProperties"], json!(false), "{kind:?}");
            let properties: Vec<&String> =
                object["properties"].as_object().unwrap().keys().collect();
            let required: Vec<&str> = object["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap())
                .collect();
            let mut missing: Vec<_> = properties
                .iter()
                .filter(|name| !required.contains(&name.as_str()))
                .collect();
            missing.sort();
            assert!(missing.is_empty(), "{kind:?} leaves {missing:?} optional");
        }
        let text = schema.to_string();
        for banned in [
            "anyOf",
            "oneOf",
            "allOf",
            "$ref",
            "$defs",
            "minLength",
            "pattern",
            "format",
            "$schema",
            "minimum",
        ] {
            assert!(
                !text.contains(&format!("\"{banned}\"")),
                "{kind:?} uses {banned}"
            );
        }
    }
}

#[test]
fn a_schema_outside_the_strict_subset_is_refused_with_the_reason() {
    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct HasOptional {
        _note: Option<String>,
    }
    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct HasMinLength {
        #[schemars(length(min = 1))]
        _note: String,
    }
    #[derive(Deserialize, JsonSchema)]
    enum Either {
        _File { _name: String },
        _Url { _name: String },
    }
    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct HasEither {
        _source: Either,
    }
    #[derive(Deserialize, JsonSchema)]
    struct AllowsExtras {
        _note: String,
    }

    assert!(matches!(
        strict_json_schema::<HasOptional>(),
        Err(SchemaError::OptionalProperties { properties, .. }) if properties == ["_note"]
    ));
    assert!(matches!(
        strict_json_schema::<HasMinLength>(),
        Err(SchemaError::BannedKeyword { keyword, .. }) if keyword == "minLength"
    ));
    assert!(matches!(
        strict_json_schema::<HasEither>(),
        Err(SchemaError::BannedKeyword { .. })
    ));
    assert!(matches!(
        strict_json_schema::<AllowsExtras>(),
        Err(SchemaError::ExtraProperties { .. })
    ));
}

fn plan_json(status: &str) -> Value {
    json!({
        "status": status,
        "summary": "Read the code.",
        "build": "A login page.",
        "done_means": ["A signed-in user lands on the dashboard."],
        "files_in_scope": ["src/login/**"],
        "ask": {"question": "", "plan_change_reason": "", "plan_change_scope": []}
    })
}

fn parse(kind: ReportKind, value: Value) -> Reported {
    parse_report(kind, &FinalReply::Json(value), &changes())
}

fn invalid(reported: &Reported) -> Option<&str> {
    let error = match reported {
        Reported::Plan {
            report: Report::Invalid { error, .. },
        } => error,
        Reported::Implementer {
            report: Report::Invalid { error, .. },
            ..
        } => error,
        Reported::Review {
            report: Report::Invalid { error, .. },
        } => error,
        Reported::Answer {
            report: Report::Invalid { error, .. },
        } => error,
        Reported::Rulings {
            report: Report::Invalid { error, .. },
        } => error,
        _ => return None,
    };
    Some(error)
}

#[test]
fn a_reply_that_is_missing_or_not_json_becomes_an_error_that_keeps_what_was_sent() {
    let missing = parse_report(ReportKind::Implementer, &FinalReply::Missing, &changes());
    let broken = parse_report(
        ReportKind::Implementer,
        &FinalReply::Text("{\"status\": \"done\"".to_owned()),
        &changes(),
    );

    assert_eq!(invalid(&missing), Some("The turn ended without a report."));
    assert_eq!(invalid(&broken), Some("The report is not valid JSON."));
    assert!(matches!(
        broken,
        Reported::Implementer { report: Report::Invalid { raw, .. }, .. } if raw == "{\"status\": \"done\""
    ));
}

#[test]
fn the_turns_changes_travel_with_an_implementer_report_even_when_the_reply_is_unusable() {
    let edited = TurnChanges {
        edited_since_command: true,
        ..changes()
    };

    let reported = parse_report(ReportKind::Implementer, &FinalReply::Missing, &edited);

    assert!(matches!(reported, Reported::Implementer { changes, .. } if changes == edited));
}

#[test]
fn a_reply_with_a_wrong_or_missing_field_names_it() {
    let mut extra = plan_json("done");
    extra["surprise"] = json!(1);
    let mut missing = plan_json("done");
    missing.as_object_mut().unwrap().remove("build");
    let mut bad_status = plan_json("done");
    bad_status["status"] = json!("finished");

    assert!(
        invalid(&parse(ReportKind::Plan, extra))
            .unwrap()
            .contains("surprise")
    );
    assert!(
        invalid(&parse(ReportKind::Plan, missing))
            .unwrap()
            .contains("build")
    );
    assert!(
        invalid(&parse(ReportKind::Plan, bad_status))
            .unwrap()
            .contains("finished")
    );
}

#[test]
fn blank_text_the_schema_cannot_forbid_is_refused_with_the_field_that_holds_it() {
    let mut blank = plan_json("done");
    blank["summary"] = json!("   ");
    let mut no_build = plan_json("done");
    no_build["build"] = json!("");

    assert_eq!(
        invalid(&parse(ReportKind::Plan, blank)),
        Some("summary: must not be blank")
    );
    assert_eq!(
        invalid(&parse(ReportKind::Plan, no_build)),
        Some("build: must say what to build")
    );
}

#[test]
fn a_finished_plan_needs_checkable_statements_and_a_scope_that_is_a_valid_glob_inside_the_repo() {
    let mut no_statements = plan_json("done");
    no_statements["done_means"] = json!([]);
    let mut no_scope = plan_json("done");
    no_scope["files_in_scope"] = json!([]);
    let mut escaping = plan_json("done");
    escaping["files_in_scope"] = json!(["../outside/**"]);
    let mut broken = plan_json("done");
    broken["files_in_scope"] = json!(["src/[a"]);

    assert!(
        invalid(&parse(ReportKind::Plan, no_statements))
            .unwrap()
            .starts_with("done_means")
    );
    assert!(
        invalid(&parse(ReportKind::Plan, no_scope))
            .unwrap()
            .starts_with("files_in_scope")
    );
    assert!(
        invalid(&parse(ReportKind::Plan, escaping))
            .unwrap()
            .contains("../outside/**")
    );
    assert!(
        invalid(&parse(ReportKind::Plan, broken))
            .unwrap()
            .contains("not a valid glob")
    );
    assert!(matches!(
        parse(ReportKind::Plan, plan_json("done")),
        Reported::Plan {
            report: Report::Finished { .. }
        }
    ));
}

#[test]
fn a_plan_that_is_not_finished_does_not_have_to_fill_in_the_plan() {
    let mut question = json!({
        "status": "question",
        "summary": "Read the code.",
        "build": "",
        "done_means": [],
        "files_in_scope": [],
        "ask": {"question": "Which folders may change?", "plan_change_reason": "", "plan_change_scope": []}
    });

    let reported = parse(ReportKind::Plan, question.clone());
    question["ask"]["question"] = json!("");
    let silent = parse(ReportKind::Plan, question);

    assert!(matches!(
        reported,
        Reported::Plan { report: Report::Question { question, .. } } if question == "Which folders may change?"
    ));
    assert_eq!(invalid(&silent), Some("ask.question: must not be blank"));
}

fn implementer_json(status: &str, ask: Value) -> Value {
    json!({
        "status": status,
        "summary": "Added the page.",
        "checks": [{"command": "cargo test", "exit_code": 0}],
        "responses": [],
        "added_probes": [],
        "ask": ask
    })
}

fn no_ask() -> Value {
    json!({"question": "", "plan_change_reason": "", "plan_change_scope": []})
}

#[test]
fn an_implementer_can_finish_block_ask_or_request_a_plan_change() {
    let blocked = parse(
        ReportKind::Implementer,
        implementer_json("blocked", no_ask()),
    );
    let changed = parse(
        ReportKind::Implementer,
        implementer_json(
            "plan_change",
            json!({"question": "", "plan_change_reason": "Needs the shared session.", "plan_change_scope": ["src/shared/**"]}),
        ),
    );

    assert!(matches!(
        blocked,
        Reported::Implementer { report: Report::Blocked { summary }, .. } if summary == "Added the page."
    ));
    let Reported::Implementer {
        report: Report::PlanChange { request, .. },
        ..
    } = changed
    else {
        panic!("a plan change");
    };
    assert_eq!(request.reason, "Needs the shared session.");
    assert_eq!(request.extra_scope[0].as_str(), "src/shared/**");
}

#[test]
fn a_plan_change_needs_its_reason_and_is_not_available_to_the_plan_author_or_the_reviewer() {
    let no_reason = parse(
        ReportKind::Implementer,
        implementer_json("plan_change", no_ask()),
    );
    let reviewer = parse(
        ReportKind::Review,
        json!({
            "status": "plan_change", "summary": "x", "verdict": "approve", "findings": [],
            "still_open": [], "ask": {"question": "", "plan_change_reason": "Because.", "plan_change_scope": []}
        }),
    );
    let author = parse(ReportKind::Plan, {
        let mut json = plan_json("plan_change");
        json["ask"]["plan_change_reason"] = json!("Because.");
        json
    });

    assert_eq!(
        invalid(&no_reason),
        Some("ask.plan_change_reason: must not be blank")
    );
    assert!(invalid(&reviewer).unwrap().contains("not available"));
    assert!(invalid(&author).unwrap().contains("not available"));
}

#[test]
fn responses_name_findings_by_their_ids_and_a_bad_id_is_an_error_the_agent_can_fix() {
    let mut report = implementer_json("done", no_ask());
    report["responses"] =
        json!([{"finding_id": "F2", "response": "disputed", "reason": "Not reachable."}]);
    let mut bad = report.clone();
    bad["responses"][0]["finding_id"] = json!("two");

    let Reported::Implementer {
        report: Report::Finished { done },
        ..
    } = parse(ReportKind::Implementer, report)
    else {
        panic!("finished");
    };

    assert_eq!(done.responses[0].finding, FindingId::new(2).unwrap());
    assert_eq!(
        invalid(&parse(ReportKind::Implementer, bad)),
        Some("responses.finding_id: `two` is not a finding id like F1")
    );
}

#[test]
fn a_review_carries_a_verdict_findings_and_the_earlier_ones_still_open() {
    let reported = parse(
        ReportKind::Review,
        json!({
            "status": "done", "summary": "1 finding.", "verdict": "changes_requested",
            "findings": [{
                "severity": "important", "kind": "bug", "class": "null-deref",
                "location": {"path": "src/a.rs", "line": 3}, "problem": "Crashes.",
                "evidence": "cargo test", "fixed_when": "cargo test passes.", "probes": ["probes/a.rs"]
            }],
            "still_open": [{"finding_id": "F1", "diagnosis": "Still crashes."}],
            "ask": no_ask()
        }),
    );

    let Reported::Review {
        report: Report::Finished { done },
    } = reported
    else {
        panic!("finished");
    };
    assert_eq!(done.verdict, Verdict::ChangesRequested);
    assert_eq!(done.findings[0].probes, ["probes/a.rs"]);
    assert_eq!(done.still_open[0].finding, FindingId::new(1).unwrap());
}

#[test]
fn a_review_that_is_blocked_is_a_stop_sign_and_a_negative_line_number_is_an_invalid_report() {
    let blocked = parse(
        ReportKind::Review,
        json!({"status": "blocked", "summary": "The task contradicts AGENTS.md.", "verdict": "approve",
               "findings": [], "still_open": [], "ask": no_ask()}),
    );
    let negative = parse(
        ReportKind::Review,
        json!({
            "status": "done", "summary": "1.", "verdict": "changes_requested",
            "findings": [{"severity": "important", "kind": "bug", "class": "c",
                "location": {"path": "a", "line": -1}, "problem": "p", "evidence": "e",
                "fixed_when": "f", "probes": []}],
            "still_open": [], "ask": no_ask()
        }),
    );

    assert!(matches!(
        blocked,
        Reported::Review {
            report: Report::Blocked { .. }
        }
    ));
    assert!(invalid(&negative).is_some());
}

#[test]
fn a_planners_answer_and_rulings_parse_and_an_empty_answer_is_refused() {
    let answer = parse(
        ReportKind::Answer,
        json!({"status": "done", "summary": "Answered.", "answer": "Use Postgres.", "ask": no_ask()}),
    );
    let empty = parse(
        ReportKind::Answer,
        json!({"status": "done", "summary": "Answered.", "answer": " ", "ask": no_ask()}),
    );
    let rulings = parse(
        ReportKind::Rulings,
        json!({"status": "done", "summary": "Ruled.", "rulings": [
            {"finding_id": "F1", "ruling": "not_bug", "reason": "Unreachable."}], "ask": no_ask()}),
    );

    assert!(matches!(
        answer,
        Reported::Answer { report: Report::Finished { done } } if done.answer == "Use Postgres."
    ));
    assert_eq!(invalid(&empty), Some("answer: must not be blank"));
    assert!(matches!(
        rulings,
        Reported::Rulings { report: Report::Finished { done } } if done.rulings.len() == 1
    ));
}

#[test]
fn a_planner_that_is_unsure_asks_and_can_flag_that_its_answer_would_contradict_the_plan() {
    let asks = parse(
        ReportKind::Answer,
        json!({"status": "question", "summary": "Unsure.", "answer": "", "ask": {
            "question": "Is the legacy API supported?", "plan_change_reason": "", "plan_change_scope": []}}),
    );
    let contradicts = parse(
        ReportKind::Answer,
        json!({"status": "plan_change", "summary": "Contradiction.", "answer": "", "ask": {
            "question": "", "plan_change_reason": "The plan forbids it.", "plan_change_scope": []}}),
    );

    assert!(matches!(
        asks,
        Reported::Answer {
            report: Report::Question { .. }
        }
    ));
    assert!(matches!(
        contradicts,
        Reported::Answer {
            report: Report::PlanChange { .. }
        }
    ));
}

#[test]
fn a_finding_id_is_f_and_a_positive_number_with_no_sign_or_leading_zero() {
    for good in ["F1", "F2", "F10", "F4294967295"] {
        assert_eq!(good.parse::<FindingId>().unwrap().to_string(), good);
    }
    for bad in [
        "",
        "F",
        "F0",
        "F01",
        "f1",
        "F-1",
        "F+1",
        "F1.5",
        "G1",
        " F1",
        "F1 ",
        "F4294967296",
    ] {
        assert!(bad.parse::<FindingId>().is_err(), "{bad:?} parsed");
    }
}

#[test]
fn a_finding_id_is_a_string_in_json_and_works_as_a_map_key() {
    let ids: std::collections::BTreeMap<FindingId, u32> =
        serde_json::from_str(r#"{"F2": 2, "F1": 1}"#).unwrap();

    assert_eq!(serde_json::to_string(&ids).unwrap(), r#"{"F1":1,"F2":2}"#);
}
