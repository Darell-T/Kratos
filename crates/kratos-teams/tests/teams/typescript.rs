//! Reads a fact log the TypeScript rules wrote (`teams-core`, commit 6741f16) as Rust facts.
//!
//! The two implementations record different facts: the TypeScript names the thread and the report in
//! separate facts, and the Rust names the message a result answers. This turns one into the other, so
//! the same log can run through both `fold`s and the views can be compared. A fact the TypeScript had
//! and Rust replaced on purpose, such as `needsUser`, is refused.

use std::fs;
use std::path::Path;

use kratos_teams::facts::{Fact, FactSeq, StoredFact};
use kratos_teams::fold::fold;
use kratos_teams::view::TaskView;
use serde_json::{Map, Value, json};

use crate::observe::{camel_to_snake, observe};

/// A fact log the TypeScript rules recorded, with their observable view after every fact.
pub struct Reference {
    pub name: String,
    pub facts: Vec<Value>,
    pub expected: Vec<Value>,
}

pub fn read_reference(path: &Path) -> Result<Reference, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let log: Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let list = |name: &str| {
        log[name]
            .as_array()
            .cloned()
            .ok_or_else(|| format!("{}: {name} is not a list", path.display()))
    };
    Ok(Reference {
        name: log["name"].as_str().unwrap_or("unnamed").to_owned(),
        facts: list("facts")?,
        expected: list("expected")?,
    })
}

/// The two members every reference log ran with. The TypeScript Team has an implementer on Codex and a
/// reviewer on Claude.
fn two_members() -> Value {
    json!({
        "planner": null,
        "implementer": {"id": "codex-1", "provider": "codex"},
        "reviewer": {"id": "claude-1", "provider": "claude"},
    })
}

/// One fact of a reference log run through the Rust fold: what the TypeScript recorded, what Rust makes
/// of it, and the Rust view behind it.
pub struct Folded {
    pub fact: usize,
    pub kind: String,
    pub view: TaskView,
    pub observed: Value,
}

/// Runs a reference log through the Rust fold, one entry for each TypeScript fact, and returns the
/// Rust facts it translated the log to.
pub fn fold_reference(log: &Reference) -> Result<(Vec<Folded>, Vec<StoredFact>), String> {
    let two = two_members();
    let mut view: Option<TaskView> = None;
    let mut folded = Vec::new();
    let mut translated = Vec::new();
    let mut applied = 0;
    for (index, stored) in log.facts.iter().enumerate() {
        let fail = |error: String| format!("{}: fact {}: {error}", log.name, index + 1);
        if let Some(fact) = translate(&stored["fact"], view.as_ref(), &two).map_err(fail)? {
            applied += 1;
            let next = StoredFact {
                seq: FactSeq(applied),
                fact,
            };
            translated.push(next.clone());
            let result = match view.take() {
                None => fold(&[next]),
                Some(earlier) => earlier.apply(next),
            };
            view = Some(result.map_err(|error| fail(error.to_string()))?);
        }
        let current = view
            .clone()
            .ok_or_else(|| format!("{}: no view after fact {}", log.name, index + 1))?;
        folded.push(Folded {
            fact: index + 1,
            kind: stored["fact"]["kind"].as_str().unwrap_or("").to_owned(),
            observed: observe(&current),
            view: current,
        });
    }
    Ok((folded, translated))
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, String> {
    value.get(name).ok_or_else(|| format!("missing {name}"))
}

fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    field(value, name)?
        .as_str()
        .ok_or_else(|| format!("{name} is not text"))
}

/// The message the task is waiting on, which the result of the TypeScript's next fact answers.
fn waiting_message(view: &TaskView) -> Result<String, String> {
    view.phase
        .waiting_message()
        .map(|message| message.as_str().to_owned())
        .ok_or_else(|| format!("no turn waits while the task is {}", view.phase.name()))
}

fn ended(message: &str, ended: Value) -> Value {
    json!({"kind": "turn_ended", "message": message, "ended": ended})
}

fn reported(kind: &str, report: Value, extra: Value) -> Value {
    let mut map = Map::new();
    map.insert("kind".to_owned(), json!(kind));
    map.insert("report".to_owned(), report);
    if let Value::Object(more) = extra {
        map.extend(more);
    }
    json!({"kind": "reported", "reported": Value::Object(map)})
}

fn invalid_report(parsed: &Value) -> Result<Value, String> {
    Ok(json!({"outcome": "invalid", "error": text(parsed, "error")?, "raw": text(parsed, "raw")?}))
}

fn implementer_report(parsed: &Value) -> Result<Value, String> {
    if field(parsed, "ok")? == &json!(false) {
        return invalid_report(parsed);
    }
    let value = field(parsed, "value")?;
    let summary = text(value, "summary")?;
    match text(value, "status")? {
        "done" => finished_implementer(value, summary),
        "blocked" => Ok(json!({"outcome": "blocked", "summary": summary})),
        other => Err(format!("the status {other} has no Rust equivalent")),
    }
}

fn finished_implementer(value: &Value, summary: &str) -> Result<Value, String> {
    let checks: Vec<Value> = field(value, "checks")?
        .as_array()
        .ok_or("checks is not a list")?
        .iter()
        .map(|check| json!({"command": check["command"], "exit_code": check["exitCode"]}))
        .collect();
    let responses: Vec<Value> = field(value, "responses")?
        .as_array()
        .ok_or("responses is not a list")?
        .iter()
        .map(|response| {
            let outcome = if response["response"] == "fixed" {
                json!({"kind": "fixed"})
            } else {
                json!({"kind": "disputed", "reason": response["reason"].as_str().unwrap_or("")})
            };
            json!({"finding": response["findingId"], "outcome": outcome})
        })
        .collect();
    Ok(json!({"outcome": "finished", "done": {
        "summary": summary, "checks": checks, "responses": responses, "added_probes": []
    }}))
}

fn finding_report(finding: &Value) -> Value {
    let severity = camel_to_snake(finding["severity"].as_str().unwrap_or(""));
    json!({
        "severity": severity,
        "kind": finding["kind"],
        "class": finding["class"],
        "location": finding["location"],
        "problem": finding["problem"],
        "evidence": finding["evidence"],
        "fixed_when": finding["fixedWhen"],
        "probes": [],
    })
}

fn review_report(parsed: &Value) -> Result<Value, String> {
    if field(parsed, "ok")? == &json!(false) {
        return invalid_report(parsed);
    }
    let value = field(parsed, "value")?;
    let summary = text(value, "summary")?;
    let verdict = match text(value, "verdict")? {
        "blocked" => return Ok(json!({"outcome": "blocked", "summary": summary})),
        other => camel_to_snake(other),
    };
    let findings: Vec<Value> = field(value, "findings")?
        .as_array()
        .ok_or("findings is not a list")?
        .iter()
        .map(finding_report)
        .collect();
    let still_open: Vec<Value> = field(value, "stillOpen")?
        .as_array()
        .ok_or("stillOpen is not a list")?
        .iter()
        .map(|entry| json!({"finding": entry["findingId"], "diagnosis": entry["diagnosis"]}))
        .collect();
    Ok(json!({"outcome": "finished", "done": {
        "verdict": verdict, "summary": summary, "findings": findings, "still_open": still_open
    }}))
}

fn evidence_checked(fact: &Value) -> Result<Value, String> {
    let mut checks = Map::new();
    for (id, check) in field(fact, "checks")?
        .as_object()
        .ok_or("checks is not a map")?
    {
        let converted = if check["kind"] == "confirmed" {
            json!({"kind": "confirmed", "inside_new_diff": check["insideNewDiff"], "output": check["output"]})
        } else {
            let why = camel_to_snake(check["reason"]["why"].as_str().unwrap_or(""));
            json!({"kind": "dropped", "reason": {"why": why, "detail": check["reason"]["detail"]}})
        };
        checks.insert(id.clone(), converted);
    }
    Ok(json!({
        "kind": "evidence_checked",
        "checked": {
            "checks": checks,
            "rechecks": field(fact, "rechecks")?,
            "probes": [],
            "security_hits": field(fact, "securityHits")?,
        }
    }))
}

fn decision(decision: &Value) -> Result<Value, String> {
    let converted = match text(decision, "kind")? {
        "approvePlan" => json!({"kind": "approve_plan", "conditions": null}),
        "direct" => json!({"kind": "direct", "text": decision["text"]}),
        "stepBack" => json!({"kind": "step_back"}),
        "rule" => json!({"kind": "rule", "not_bug": decision["notBug"]}),
        "resume" => json!({"kind": "resume"}),
        "accept" => json!({"kind": "accept"}),
        "cancel" => json!({"kind": "cancel"}),
        "finish" => json!({"kind": "finish", "outcome": decision["outcome"]}),
        other => return Err(format!("the decision {other} is not known")),
    };
    Ok(json!({"kind": "user_decided", "decision": converted}))
}

fn task_started(fact: &Value, two_members: Value) -> Result<Value, String> {
    let rules = field(fact, "rules")?;
    let policy = field(fact, "policy")?;
    Ok(json!({"kind": "task_started", "started": {
        "request": fact["text"],
        "constraints": [],
        "scope": [],
        "title": fact["title"],
        "branch": fact["branch"],
        "base": fact["base"],
        "rules": {"agents_md": rules["agentsMd"], "claude_md": rules["claudeMd"], "review_md": rules["reviewMd"]},
        "triggers": fact["triggers"],
        "policy": {
            "plan_first": policy["planFirst"],
            "security_pass": policy["securityPass"],
            "permission": "askMe",
        },
        "bindings": two_members,
    }}))
}

/// One TypeScript fact as the JSON of a Rust fact, or `None` when Rust has no counterpart for it. The
/// view is the Rust view before the fact, which knows the message each result answers.
type Translator = fn(&Value, &TaskView) -> Result<Option<Value>, String>;

const TRANSLATORS: [(&str, Translator); 10] = [
    ("implementer.opened", opened),
    ("plan.drafted", plan_drafted),
    ("implementer.reported", implementer_reported),
    ("reviewer.reported", reviewer_reported),
    ("agent.lost", agent_lost),
    ("committed", committed),
    ("evidence.checked", |fact, _| {
        evidence_checked(fact).map(Some)
    }),
    ("step.failed", step_failed),
    ("user.decided", |fact, _| {
        decision(field(fact, "decision")?).map(Some)
    }),
    ("cleaned", |fact, _| {
        Ok(Some(
            json!({"kind": "cleaned", "leftover": fact["leftover"]}),
        ))
    }),
];

/// The first `implementer.opened` is the worktree being ready. A later one is a replacement thread,
/// which is the engine's business, so Rust has no fact for it.
fn opened(_: &Value, view: &TaskView) -> Result<Option<Value>, String> {
    let is_preparing = view.phase.name() == "preparing";
    Ok((is_preparing && view.rounds.is_empty()).then(|| json!({"kind": "prepared"})))
}

fn plan_drafted(fact: &Value, view: &TaskView) -> Result<Option<Value>, String> {
    let plan = json!({"kind": "reported", "reported": {"kind": "plan", "report": {
        "outcome": "finished",
        "done": {"build": fact["text"], "done_means": ["Done."], "scope": ["**"]}
    }}});
    Ok(Some(ended(&waiting_message(view)?, plan)))
}

fn implementer_reported(fact: &Value, view: &TaskView) -> Result<Option<Value>, String> {
    let report = implementer_report(field(fact, "report")?)?;
    let extra = json!({"changes": field(fact, "changes")?});
    let ending = reported("implementer", report, extra);
    Ok(Some(ended(&waiting_message(view)?, ending)))
}

fn reviewer_reported(fact: &Value, view: &TaskView) -> Result<Option<Value>, String> {
    let report = review_report(field(fact, "report")?)?;
    let ending = reported("review", report, json!({}));
    Ok(Some(ended(&waiting_message(view)?, ending)))
}

fn agent_lost(fact: &Value, view: &TaskView) -> Result<Option<Value>, String> {
    let lost = json!({"kind": "lost", "reason": fact["reason"]});
    Ok(Some(ended(&waiting_message(view)?, lost)))
}

fn committed(fact: &Value, _: &TaskView) -> Result<Option<Value>, String> {
    Ok(Some(json!({
        "kind": "committed", "commit": fact["commit"], "files": ["src/a.ts"],
        "size": {"files": 1, "lines": 1}, "probe_runs": []
    })))
}

fn step_failed(fact: &Value, _: &TaskView) -> Result<Option<Value>, String> {
    Ok(Some(
        json!({"kind": "step_failed", "step": fact["step"], "error": fact["error"]}),
    ))
}

/// The Rust fact for one TypeScript fact, or `None` for a fact Rust has no counterpart for.
pub fn translate(
    fact: &Value,
    view: Option<&TaskView>,
    two_members: &Value,
) -> Result<Option<Fact>, String> {
    let kind = text(fact, "kind")?;
    if kind == "task.started" {
        return from_json(task_started(fact, two_members.clone())?);
    }
    let view = view.ok_or("the first fact is task.started")?;
    let (_, translator) = TRANSLATORS
        .iter()
        .find(|(name, _)| *name == kind)
        .ok_or_else(|| format!("the fact {kind} is not known"))?;
    match translator(fact, view)? {
        Some(json) => from_json(json),
        None => Ok(None),
    }
}

fn from_json(json: Value) -> Result<Option<Fact>, String> {
    serde_json::from_value(json.clone())
        .map(Some)
        .map_err(|error| format!("{error} in {json}"))
}
