//! The Rust fold against the TypeScript fold. `tests/fixtures/reference/` holds 32 two-member fact logs
//! that the TypeScript Teams rules (`teams-core`, commit 6741f16) recorded, each with the TypeScript's
//! view and next step after every fact. Each log runs through the Rust `fold` here, and the views and
//! next steps must match.
//!
//! These logs cover the behavior the Rust rules kept. Behavior that changed on purpose (questions
//! instead of `needsUser`, a blocked review skipping the evidence check, and a round count that only the
//! user's explicit requests restart) has its own tests and no reference log.

use std::fs;
use std::path::PathBuf;

use kratos_teams::fold::fold;

use crate::typescript::{Reference, fold_reference, read_reference};

pub fn reference_logs() -> Vec<Reference> {
    let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/reference");
    let mut files: Vec<PathBuf> = fs::read_dir(&folder)
        .expect("the reference folder exists")
        .map(|entry| entry.expect("a readable entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    files
        .iter()
        .map(|path| read_reference(path).unwrap_or_else(|error| panic!("{error}")))
        .collect()
}

#[test]
fn at_least_thirty_two_member_logs_are_on_file() {
    assert!(reference_logs().len() >= 30);
}

#[test]
fn every_reference_log_folds_to_the_views_and_next_steps_the_typescript_fold_recorded() {
    let mut compared = 0;
    for log in reference_logs() {
        let (rust, translated) = fold_reference(&log).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(rust.len(), log.expected.len(), "{}", log.name);
        for (ours, theirs) in rust.iter().zip(&log.expected) {
            assert_eq!(
                &ours.observed,
                theirs,
                "{}: the views differ after fact {} of {}, a {}",
                log.name,
                ours.fact,
                log.facts.len(),
                ours.kind
            );
            compared += 1;
        }
        let whole = fold(&translated).expect("the translated log folds in one pass");
        assert_eq!(
            Some(&whole),
            rust.last().map(|last| &last.view),
            "{}: folding the whole log differs from applying it fact by fact",
            log.name
        );
    }
    assert!(compared > 250, "only {compared} views were compared");
}

#[test]
fn the_logs_cover_planning_off_and_on_pauses_of_every_kind_and_both_ways_a_task_ends() {
    let mut phases = std::collections::BTreeSet::new();
    let mut pauses = std::collections::BTreeSet::new();
    for log in reference_logs() {
        for observed in &log.expected {
            phases.insert(observed["phase"].as_str().unwrap_or_default().to_owned());
            if let Some(pause) = observed["pause"].as_str() {
                pauses.insert(pause.to_owned());
            }
        }
    }
    for phase in [
        "preparing",
        "implementing",
        "planning",
        "committing",
        "reviewing",
        "checking",
        "paused",
        "approved",
        "closing",
        "closed",
    ] {
        assert!(phases.contains(phase), "no log reaches {phase}");
    }
    for pause in [
        "findings:stuck",
        "findings:cap",
        "disputed",
        "trouble:invalidReport",
        "trouble:lost",
        "trouble:uncheckedEdits",
        "trouble:verdictMismatch",
        "trouble:blocked",
        "trouble:system",
    ] {
        assert!(pauses.contains(pause), "no log pauses as {pause}");
    }
}
