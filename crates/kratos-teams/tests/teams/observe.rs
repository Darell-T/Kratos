//! The part of a task view the TypeScript rules also had, in the TypeScript's names.
//!
//! The reference comparison needs one shape that both implementations print for a fact log: the phase,
//! the findings, the rounds, and the next step. Rust's view holds more (questions, rulings, probes, the
//! planner), so this keeps only what the TypeScript `fold` and `plan` can answer too.

use kratos_teams::decisions::{DecisionKind, allowed_decisions};
use kratos_teams::members::Role;
use kratos_teams::next_step::{Step, next_step};
use kratos_teams::view::{
    Activity, FindingStatus, PassKind, Phase, TaskView, Trouble, Waiting, Work,
};
use serde::Serialize;
use serde_json::{Value, json};

pub fn tag_of(value: &impl Serialize) -> String {
    let value = serde_json::to_value(value).expect("a value serializes");
    let tag = value.as_str().or_else(|| value["kind"].as_str());
    tag.expect("a unit variant or a kind tag").to_owned()
}

pub fn snake_to_camel(name: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for character in name.chars() {
        if character == '_' {
            upper = true;
        } else if upper {
            out.push(character.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(character);
        }
    }
    out
}

pub fn camel_to_snake(name: &str) -> String {
    let mut out = String::new();
    for character in name.chars() {
        if character.is_ascii_uppercase() {
            out.push('_');
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

/// The name of the phase in the TypeScript's words.
fn phase_name(view: &TaskView) -> &'static str {
    match &view.phase {
        Phase::Active { activity } => match activity {
            Activity::Preparing { .. } => "preparing",
            Activity::Turn { turn } => match turn.work {
                Work::Review { .. } => "reviewing",
                _ => "implementing",
            },
            Activity::Committing { .. } => "committing",
            Activity::Checking { .. } => "checking",
            Activity::Rebinding { .. } => "implementing",
        },
        Phase::Waiting { waiting } => match waiting {
            Waiting::PlanApproval => "planning",
            Waiting::Approved { .. } => "approved",
            _ => "paused",
        },
        Phase::Closing { .. } => "closing",
        Phase::Closed { .. } => "closed",
    }
}

fn trouble_name(why: Trouble) -> String {
    snake_to_camel(&tag_of(&why))
}

/// Why the task is paused, in the TypeScript's words, or `null`.
fn pause_name(view: &TaskView) -> Value {
    let Phase::Waiting { waiting } = &view.phase else {
        return Value::Null;
    };
    match waiting {
        Waiting::Findings { why, .. } => json!(format!("findings:{}", tag_of(why))),
        Waiting::Ruling { .. } => json!("disputed"),
        Waiting::Trouble { why, .. } => json!(format!("trouble:{}", trouble_name(*why))),
        _ => Value::Null,
    }
}

fn status_name(status: &FindingStatus) -> String {
    match status {
        FindingStatus::Open { .. } => "open".to_owned(),
        FindingStatus::Closed { how } => format!("closed:{}", snake_to_camel(&tag_of(how))),
        FindingStatus::Dropped { reason } => {
            format!("dropped:{}", snake_to_camel(&tag_of(&reason.why)))
        }
        FindingStatus::Listed { why } => format!("listed:{}", snake_to_camel(&tag_of(why))),
    }
}

/// The decisions the TypeScript knew, in its names.
fn allowed_names(view: &TaskView) -> Vec<String> {
    allowed_decisions(view)
        .into_iter()
        .filter_map(|kind| match kind {
            DecisionKind::ApprovePlan => Some("approvePlan"),
            DecisionKind::Direct => Some("direct"),
            DecisionKind::StepBack => Some("stepBack"),
            DecisionKind::Rule => Some("rule"),
            DecisionKind::Resume => Some("resume"),
            DecisionKind::Accept => Some("accept"),
            DecisionKind::Cancel => Some("cancel"),
            DecisionKind::Finish => Some("finish"),
            _ => None,
        })
        .map(str::to_owned)
        .collect()
}

fn step_name(view: &TaskView) -> &'static str {
    match next_step(view) {
        Step::Wait { .. } => "wait",
        Step::Prepare => "prepare",
        Step::Run { run } if run.role == Role::Reviewer => "askReviewer",
        Step::Run { .. } => "askImplementer",
        Step::Commit { .. } => "commit",
        Step::Check { .. } => "check",
        Step::Cleanup { .. } => "cleanup",
        Step::ApplyRoles { .. } | Step::HandOffAccount { .. } | Step::HandToScheduler { .. } => {
            "engine"
        }
    }
}

pub fn observe(view: &TaskView) -> Value {
    let commit = match &view.phase {
        Phase::Waiting {
            waiting: Waiting::Approved { commit, .. },
        } => json!(commit.as_str()),
        _ => Value::Null,
    };
    let rounds: Vec<Value> = view
        .rounds
        .iter()
        .map(|round| {
            json!({
                "n": round.n,
                "commit": round.commit.as_str(),
                "answered": round.report.is_some(),
                "reviews": round.reviews.iter().map(|r| r.accepted).collect::<Vec<_>>(),
            })
        })
        .collect();
    let findings: Vec<Value> = view
        .findings
        .iter()
        .map(|f| json!({"id": f.id.to_string(), "status": status_name(&f.status)}))
        .collect();
    json!({
        "phase": phase_name(view),
        "pause": pause_name(view),
        "commit": commit,
        "rounds": rounds,
        "findings": findings,
        "dirty": view.dirty,
        "lastReviewed": view.last_reviewed.as_ref().map(|sha| sha.as_str()),
        "securityDone": view.passes_done.contains(&PassKind::Security),
        "next": step_name(view),
        "allowed": allowed_names(view),
    })
}
