//! Review depth and ranking (D94). A small change gets one pass. A large change also gets a security
//! pass and a regressions pass, each once per task and each from a fresh reviewer session. Findings reach
//! the user ranked by severity.

use kratos_teams::decisions::approval_report;
use kratos_teams::facts::{ChangeSize, Depth, Ended};
use kratos_teams::members::Role;
use kratos_teams::view::{ApprovedBy, FindingsPause, PassKind, Waiting, Work};

use crate::replay::*;
use crate::support::*;

fn script(implementer: Vec<Ended>, reviewer: Vec<Ended>) -> Script {
    Script {
        implementer: implementer.into(),
        reviewer: reviewer.into(),
        ..Script::default()
    }
}

fn passes_run(task: &ScriptedTask) -> Vec<(PassKind, Depth)> {
    runs(task, Role::Reviewer)
        .iter()
        .map(|run| match &run.work {
            Work::Review { job } => (job.pass.kind(), job.depth),
            other => panic!("a reviewer ran {other:?}"),
        })
        .collect()
}

fn size(files: u32, lines: u32) -> ChangeSize {
    ChangeSize { files, lines }
}

fn depth_of_first_review(size: ChangeSize) -> Depth {
    let mut script = script(vec![implements()], vec![approves(), approves(), approves()]);
    script.sizes.insert(1, size);
    let mut task = Task::two().large_change(10, 400).script(script);
    task.run();
    passes_run(&task)[0].1
}

#[test]
fn a_change_below_both_limits_gets_one_pass() {
    assert_eq!(depth_of_first_review(size(9, 399)), Depth::Single);

    let mut script = script(vec![implements()], vec![approves()]);
    script.sizes.insert(1, size(9, 399));
    let mut task = Task::two()
        .security_pass()
        .large_change(10, 400)
        .script(script);
    task.script.scans = vec![vec![]].into();
    task.run();

    assert_eq!(passes_run(&task), [(PassKind::Standard, Depth::Single)]);
}

#[test]
fn a_change_at_either_limit_is_large() {
    assert_eq!(depth_of_first_review(size(10, 0)), Depth::Thorough);
    assert_eq!(depth_of_first_review(size(0, 400)), Depth::Thorough);
}

#[test]
fn a_large_change_runs_correctness_security_and_regressions_in_that_order_each_from_a_new_session()
{
    let mut script = script(vec![implements()], vec![approves(), approves(), approves()]);
    script.sizes.insert(1, size(12, 50));
    script.scans = vec![vec![]].into();
    let mut task = Task::two()
        .security_pass()
        .large_change(10, 400)
        .script(script);

    task.run();

    assert_eq!(
        passes_run(&task),
        [
            (PassKind::Standard, Depth::Thorough),
            (PassKind::Security, Depth::Thorough),
            (PassKind::Regressions, Depth::Thorough)
        ]
    );
    assert!(
        runs(&task, Role::Reviewer)
            .iter()
            .all(|run| run.follows.is_none())
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn the_security_pass_of_a_large_change_runs_even_when_no_trigger_matched_a_line() {
    let mut script = script(vec![implements()], vec![approves(), approves(), approves()]);
    script.sizes.insert(1, size(12, 50));
    script.scans = vec![vec![]].into();
    let mut task = Task::two()
        .security_pass()
        .large_change(10, 400)
        .script(script);

    task.run();

    assert!(
        passes_run(&task)
            .iter()
            .any(|(kind, _)| *kind == PassKind::Security)
    );
}

#[test]
fn a_large_change_in_a_team_with_the_security_pass_off_gets_only_correctness_and_regressions() {
    let mut script = script(vec![implements()], vec![approves(), approves()]);
    script.sizes.insert(1, size(12, 50));
    let mut task = Task::two().large_change(10, 400).script(script);

    task.run();

    assert_eq!(
        passes_run(&task),
        [
            (PassKind::Standard, Depth::Thorough),
            (PassKind::Regressions, Depth::Thorough)
        ]
    );
}

#[test]
fn a_project_that_turned_its_triggers_off_gets_no_security_pass_even_for_a_large_change() {
    let mut script = script(vec![implements()], vec![approves(), approves()]);
    script.sizes.insert(1, size(12, 50));
    let mut task = Task::two()
        .security_pass()
        .without_triggers()
        .large_change(10, 400)
        .script(script);

    task.run();

    assert_eq!(
        passes_run(&task)
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>(),
        [PassKind::Standard, PassKind::Regressions]
    );
}

#[test]
fn each_extra_pass_runs_once_per_task_even_when_a_blocking_finding_costs_a_round() {
    let mut script = script(
        vec![implements(), implements_answering(vec![fixed(1)])],
        vec![
            approves(),
            approves(),
            requests_changes(vec![bug("regression", "cmd1")]),
            approves(),
        ],
    );
    script.sizes.insert(1, size(12, 50));
    script.sizes.insert(2, size(12, 50));
    script.scans = vec![vec![]].into();
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two()
        .security_pass()
        .large_change(10, 400)
        .script(script);

    task.run();

    let kinds: Vec<PassKind> = passes_run(&task).iter().map(|(kind, _)| *kind).collect();
    assert_eq!(
        kinds,
        [
            PassKind::Standard,
            PassKind::Security,
            PassKind::Regressions,
            PassKind::Standard
        ]
    );
    assert_eq!(task.view().phase, approved(2, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn the_extra_passes_read_the_whole_change_from_the_base_and_the_standard_pass_only_what_is_new() {
    let mut script = script(
        vec![implements(), implements_answering(vec![fixed(1)])],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
            approves(),
            approves(),
        ],
    );
    script.sizes.insert(2, size(12, 50));
    script.scans = vec![vec![]].into();
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two()
        .security_pass()
        .large_change(10, 400)
        .script(script);

    task.run();

    let bases: Vec<_> = runs(&task, Role::Reviewer)
        .iter()
        .map(|run| match &run.work {
            Work::Review { job } => job.diff_base.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(bases, [sha(0xba5e), sha(1), sha(0xba5e), sha(0xba5e)]);
}

#[test]
fn a_finding_reaches_the_user_ranked_by_severity_then_by_the_strength_of_its_evidence() {
    let mut task = Task::two().script(script(
        vec![implements()],
        vec![approves_listing(vec![
            nit("naming"),
            pre_existing("old-crash"),
            design("layering"),
        ])],
    ));

    task.run();

    let report = approval_report(&task.view()).expect("approved");
    let order: Vec<_> = report
        .listed
        .iter()
        .map(|f| f.report.class.as_str())
        .collect();
    assert_eq!(order, ["layering", "old-crash", "naming"]);
}

#[test]
fn the_findings_a_pause_shows_are_ranked_with_the_strongest_evidence_first() {
    let mut script = script(
        vec![implements()],
        vec![requests_changes(vec![
            requirement("untested", AGENTS_RULE),
            bug("null-deref", "cmd1"),
        ])],
    );
    script_commands(&mut script, "cmd1", vec![fails()]);
    let mut task = Task::two().round_cap(1).script(script);

    task.run();

    assert_eq!(
        waiting(&task.view()),
        Some(&Waiting::Findings {
            why: FindingsPause::Cap,
            ids: vec![fid(2), fid(1)]
        }),
        "the bug, whose command shows it, comes before the requirement"
    );
}
