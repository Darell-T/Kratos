//! Folds a recorded fact log and prints each view and the next step as JSON lines. It reads no database
//! and starts no engine.
//!
//! ```text
//! cargo run -p kratos-teams --example replay
//! cargo run -p kratos-teams --example replay -- crates/kratos-teams/tests/fixtures/lanes/r6-stuck.jsonl
//! cargo run -p kratos-teams --example replay -- --typescript crates/kratos-teams/tests/fixtures/reference/01-approves-in-one-round-and-lists-what.json
//! ```
//!
//! With no argument it runs every two-member log in `tests/fixtures/reference/`. The TypeScript rules
//! (`teams-core`, commit 6741f16) recorded those logs with their own view after every fact. Each fact
//! prints the Rust view next to the TypeScript one. The known pending-security correction is reported
//! separately, and the exit status is 1 for any unexpected difference.
//!
//! With a path it reads one `StoredFact` per line, the format the engine appends, and prints one line
//! for each fact:
//!
//! - `observed`: the view in the TypeScript's words (phase, pause, rounds, findings, next step)
//! - `next_step`: the step `next_step` names, and `message`, the text of the message when it is a run
//! - `allowed`: the decisions the user could make now
//! - `outside_scope`: the changed files the agreed scope does not cover
//! - `view`: the whole Rust view (left out with `--short`)

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kratos_teams::decisions::allowed_decisions;
use kratos_teams::facts::StoredFact;
use kratos_teams::fold::fold;
use kratos_teams::next_step::{Step, next_step};
use kratos_teams::prompts::{Platform, PromptContext, Session, message};
use kratos_teams::view::TaskView;
use serde_json::{Value, json};

#[path = "../tests/teams/observe.rs"]
mod observe;
#[path = "../tests/teams/typescript.rs"]
mod typescript;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let short = args.iter().any(|arg| arg == "--short");
    let rest: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--short")
        .collect();
    let outcome = match rest.as_slice() {
        [] => replay_every_reference(),
        ["--typescript", path] => replay_typescript(Path::new(path), short).map(|diffs| diffs == 0),
        [path] if !path.starts_with("--") => replay_facts(Path::new(path), short).map(|()| true),
        _ => Err(
            "usage: replay [--short] [--typescript <reference.json> | <facts.jsonl>]".to_owned(),
        ),
    };
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

/// What the rules ask for next, and the text of the message when it is a run. The message is written
/// for a new session on Unix, which is the fullest text a member gets.
fn next_and_message(view: &TaskView) -> (Value, Value) {
    let step = next_step(view);
    let text = match &step {
        Step::Run { run } => {
            let context = PromptContext {
                session: Session::Fresh,
                diff_stat: "",
                platform: Platform::Unix,
            };
            json!(message(view, run, &context))
        }
        _ => Value::Null,
    };
    (serde_json::to_value(&step).unwrap_or(Value::Null), text)
}

fn print(line: &Value) {
    println!("{line}");
}

fn replay_facts(path: &Path, short: bool) -> Result<(), String> {
    let file = fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut view: Option<TaskView> = None;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| error.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let at = index + 1;
        let stored: StoredFact =
            serde_json::from_str(&line).map_err(|error| format!("line {at}: {error}"))?;
        let kind =
            serde_json::to_value(&stored.fact).map_or(Value::Null, |fact| fact["kind"].clone());
        let seq = stored.seq;
        let result = match view.take() {
            None => fold(&[stored]),
            Some(earlier) => earlier.apply(stored),
        };
        let current = result.map_err(|error| format!("line {at}: {error}"))?;
        let (step, text) = next_and_message(&current);
        let mut printed = json!({
            "seq": seq,
            "kind": kind,
            "observed": observe::observe(&current),
            "next_step": step,
            "message": text,
            "allowed": allowed_decisions(&current),
            "outside_scope": current.outside_scope(),
        });
        if !short {
            printed["view"] = serde_json::to_value(&current).map_err(|error| error.to_string())?;
        }
        print(&printed);
        view = Some(current);
    }
    Ok(())
}

/// The raw differences from TypeScript, and how many violate Rust's corrected expectations.
fn differing_facts(log: &typescript::Reference) -> Result<(usize, usize), String> {
    let (folded, _) = typescript::fold_reference(log)?;
    let differing = folded
        .iter()
        .zip(&log.expected)
        .filter(|(ours, theirs)| &ours.observed != *theirs)
        .count();
    let unexpected = folded
        .iter()
        .filter(|ours| ours.observed != log.expected_rust(ours.fact))
        .count();
    Ok((differing, unexpected))
}

/// Prints both recorded and corrected expectations, returning the unexpected differences.
fn replay_typescript(path: &Path, short: bool) -> Result<usize, String> {
    let log = typescript::read_reference(path)?;
    let (folded, _) = typescript::fold_reference(&log)?;
    let mut differing = 0;
    for (ours, theirs) in folded.iter().zip(&log.expected) {
        let equal = &ours.observed == theirs;
        let expected_rust = log.expected_rust(ours.fact);
        let expected = ours.observed == expected_rust;
        differing += usize::from(!expected);
        let (step, text) = next_and_message(&ours.view);
        let mut printed = json!({
            "log": log.name,
            "fact": ours.fact,
            "kind": ours.kind,
            "equal": equal,
            "rust": ours.observed,
            "typescript": theirs,
            "expected_rust": expected_rust,
            "intentional_difference": !equal && expected,
            "next_step": step,
            "message": text,
        });
        if !short {
            printed["view"] =
                serde_json::to_value(&ours.view).map_err(|error| error.to_string())?;
        }
        print(&printed);
    }
    Ok(differing)
}

/// One line for each reference log, then a total. Exits 1 for an unexpected difference.
fn replay_every_reference() -> Result<bool, String> {
    let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/reference");
    let mut files: Vec<PathBuf> = fs::read_dir(&folder)
        .map_err(|error| format!("{}: {error}", folder.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    let (mut facts, mut differing, mut unexpected) = (0, 0, 0);
    for file in &files {
        let log = typescript::read_reference(file)?;
        let (diff, unplanned) = differing_facts(&log)?;
        print(
            &json!({"log": log.name, "facts": log.facts.len(), "differing": diff, "unexpected": unplanned}),
        );
        facts += log.facts.len();
        differing += diff;
        unexpected += unplanned;
    }
    print(
        &json!({"summary": true, "logs": files.len(), "facts": facts, "differing": differing, "unexpected": unexpected}),
    );
    Ok(unexpected == 0)
}
