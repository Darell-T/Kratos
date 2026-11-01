//! The review loop: rounds, findings, the evidence check, stuck findings, the round cap, and the
//! security pass. Ported from the TypeScript `teams-task.test.ts`.

use kratos_teams::decisions::{Decision, DecisionKind};
use kratos_teams::facts::Ended;
use kratos_teams::members::Role;
use kratos_teams::security_triggers::TriggerHit;
use kratos_teams::view::{
    ApprovedBy, Brief, ClosedHow, FindingStatus, FindingsPause, Note, Pass, PassKind, Trouble,
    Waiting, Work,
};

use crate::replay::*;
use crate::support::*;

fn script(implementer: Vec<Ended>, reviewer: Vec<Ended>) -> Script {
    Script {
        implementer: implementer.into(),
        reviewer: reviewer.into(),
        ..Script::default()
    }
}

fn fix_f1() -> Ended {
    implements_answering(vec![fixed(1)])
}

fn spawn_hit() -> TriggerHit {
    TriggerHit::Code {
        pattern: "spawn".to_owned(),
        path: "src/run.ts".to_owned(),
        line: "spawn(command);".to_owned(),
    }
}

#[test]
fn a_clean_review_approves_in_one_round_and_lists_what_cannot_block_without_checking_it() {
    let mut task = Task::two().script(script(
        vec![implements()],
        vec![approves_listing(vec![
            nit("naming"),
            design("layering"),
            pre_existing("old-crash"),
        ])],
    ));

    assert_eq!(task.run(), ["prepare", "run", "commit", "run", "check"]);

    let view = task.view();
    assert_eq!(view.phase, approved(1, ApprovedBy::Reviewer));
    assert_eq!(outcomes(&view), ["Nit", "Design", "PreExisting"]);
    assert_eq!(checks(&task)[0].candidates, []);
    assert_eq!(task.script.commands_run(), 0);
    assert_eq!(runs(&task, Role::Implementer).len(), 1);
    assert_eq!(view.last_reviewed, Some(sha(1)));
    assert_replayable(&task);
}

#[test]
fn a_rerun_that_passes_closes_a_bug_over_the_reviewers_stale_claim_and_the_verdict_is_corrected_in_the_same_session()
 {
    let mut script = script(
        vec![implements(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            still_open(1, "Still crashes."),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script);

    task.run();

    let view = task.view();
    assert_eq!(view.phase, approved(2, ApprovedBy::Reviewer));
    assert_eq!(
        view.findings[0].status,
        FindingStatus::Closed {
            how: ClosedHow::Fixed
        }
    );
    let accepted: Vec<bool> = view.rounds[1].reviews.iter().map(|r| r.accepted).collect();
    assert_eq!(accepted, [false, true]);
    let reviews = runs(&task, Role::Reviewer);
    let (first, second, third) = (reviews[0], reviews[1], reviews[2]);
    assert!(matches!(third.note, Some(Note::VerdictMismatch { .. })));
    assert_eq!(third.follows.as_ref(), Some(&second.message));
    assert_eq!(second.follows, None);
    assert_ne!(first.message, second.message);
    assert_replayable(&task);
}

#[test]
fn a_finding_that_survives_two_fixes_pauses_with_the_reviewers_diagnosis_and_a_direction_goes_on() {
    let mut script = script(
        vec![implements(), fix_f1(), fix_f1(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            still_open(1, "A null session still reaches it."),
            still_open(1, "Still reachable through b()."),
            approves(),
        ],
    );
    script_commands(
        &mut script,
        "cmd1",
        vec![fails(), fails(), fails(), passes()],
    );
    let mut task = Task::two().script(script);

    task.run();

    let view = task.view();
    assert_eq!(
        waiting(&view),
        Some(&Waiting::Findings {
            why: FindingsPause::Stuck,
            ids: vec![fid(1)]
        })
    );
    assert!(matches!(
        &view.findings[0].status,
        FindingStatus::Open { diagnosis: Some(text), .. } if text == "Still reachable through b()."
    ));
    assert_eq!(
        decisions(&view),
        [
            DecisionKind::Direct,
            DecisionKind::StepBack,
            DecisionKind::Accept,
            DecisionKind::Cancel
        ]
    );
    task.decide(Decision::Direct {
        text: "Guard the session in b() as well.".to_owned(),
    });
    let view = task.view();
    assert_eq!(view.phase, approved(4, ApprovedBy::Reviewer));
    assert_eq!(view.leg_start, 3);
    assert_replayable(&task);
}

#[test]
fn a_direction_restores_fix_attempts_so_the_finding_is_not_stuck_again_at_once() {
    let mut script = script(
        vec![implements(), fix_f1(), fix_f1(), fix_f1(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            still_open(1, "Still crashes."),
            still_open(1, "Still crashes."),
            still_open(1, "Still crashes."),
            approves(),
        ],
    );
    script_commands(
        &mut script,
        "cmd1",
        vec![fails(), fails(), fails(), fails(), passes()],
    );
    let mut task = Task::two().script(script);
    task.run();
    assert!(matches!(
        waiting(&task.view()),
        Some(Waiting::Findings {
            why: FindingsPause::Stuck,
            ..
        })
    ));

    task.decide(Decision::Direct {
        text: "Try the other approach.".to_owned(),
    });

    assert_eq!(task.view().phase, approved(5, ApprovedBy::Reviewer));
    assert_eq!(runs(&task, Role::Implementer).len(), 5);
    assert_replayable(&task);
}

#[test]
fn the_task_pauses_at_the_round_cap_when_new_findings_keep_arriving() {
    let mut script = script(
        vec![
            implements(),
            implements_answering(vec![fixed(1)]),
            implements_answering(vec![fixed(2)]),
        ],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            requests_changes(vec![bug("race", "cmd2")]),
            requests_changes(vec![bug("leak", "cmd3")]),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    script_commands(&mut script, "cmd2", vec![fails(), passes()]);
    script_commands(&mut script, "cmd3", vec![fails()]);
    let mut task = Task::two().script(script);

    task.run();

    let view = task.view();
    assert_eq!(
        waiting(&view),
        Some(&Waiting::Findings {
            why: FindingsPause::Cap,
            ids: vec![fid(3)]
        })
    );
    assert_eq!(view.rounds.len(), 3);
    assert_replayable(&task);
}

#[test]
fn the_cap_is_a_setting_and_a_lower_cap_pauses_sooner() {
    let mut script = script(
        vec![implements(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            requests_changes(vec![bug("race", "cmd2")]),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    script_commands(&mut script, "cmd2", vec![fails()]);
    let mut task = Task::two().round_cap(2).script(script);

    task.run();

    let view = task.view();
    assert_eq!(view.rounds.len(), 2);
    assert!(matches!(
        waiting(&view),
        Some(Waiting::Findings {
            why: FindingsPause::Cap,
            ..
        })
    ));
}

#[test]
fn only_the_users_explicit_requests_to_go_on_add_rounds_not_a_resume_or_a_ruling() {
    // A lost turn twice pauses the task in round 2. Resuming is not a request for more rounds, so the
    // count still runs from the start and the cap still pauses the task at round 3.
    let mut script = script(
        vec![
            implements(),
            lost(),
            lost(),
            fix_f1(),
            implements_answering(vec![fixed(2)]),
        ],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            requests_changes(vec![bug("race", "cmd2")]),
            requests_changes(vec![bug("leak", "cmd3")]),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    script_commands(&mut script, "cmd2", vec![fails(), passes()]);
    script_commands(&mut script, "cmd3", vec![fails()]);
    let mut task = Task::two().script(script);
    task.run();
    assert_eq!(
        trouble(&task.view()).map(|(why, _, _)| why),
        Some(Trouble::Lost)
    );

    task.decide(Decision::Resume);

    let view = task.view();
    assert_eq!(view.leg_start, 0);
    assert!(matches!(
        waiting(&view),
        Some(Waiting::Findings {
            why: FindingsPause::Cap,
            ..
        })
    ));
}

#[test]
fn findings_kratos_cannot_confirm_are_kept_with_the_reason_and_never_sent() {
    let mut script = script(
        vec![implements()],
        vec![approves_listing(vec![
            bug("null-deref", "cmd1"),
            requirement("spec", "no such rule anywhere"),
            bug("slow", "cmd2"),
            bug("escape", "cmd3"),
        ])],
    );
    script_commands(&mut script, "cmd1", vec![passes()]);
    script_commands(&mut script, "cmd2", vec![times_out()]);
    script_commands(&mut script, "cmd3", vec![refused()]);
    let mut task = Task::two().script(script);

    task.run();

    let view = task.view();
    assert_eq!(view.phase, approved(1, ApprovedBy::Reviewer));
    assert_eq!(
        outcomes(&view),
        [
            "CommandPassed",
            "Quote",
            "CommandTimedOut",
            "CommandRefused"
        ]
    );
    assert_eq!(runs(&task, Role::Implementer).len(), 1);
    let checked: Vec<_> = view.rounds[0].reviews[0].checks.keys().copied().collect();
    assert_eq!(checked, [fid(1), fid(2), fid(3), fid(4)]);
    assert_replayable(&task);
}

#[test]
fn a_confirmed_finding_outside_the_lines_a_later_round_changed_is_listed_and_sends_nothing() {
    let mut script = script(
        vec![implements(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves_listing(vec![bug("old-code", "cmd2")]),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    script_commands(&mut script, "cmd2", vec![fails()]);
    script.outside_diff = vec!["old-code".to_owned()];
    let mut task = Task::two().script(script);

    task.run();

    let view = task.view();
    assert_eq!(view.phase, approved(2, ApprovedBy::Reviewer));
    assert_eq!(outcomes(&view), ["Closed Fixed", "OutsideDiff"]);
    assert_replayable(&task);
}

#[test]
fn the_first_review_reads_the_whole_change_and_later_ones_rerun_only_the_open_bugs() {
    let mut script = script(
        vec![implements(), implements_answering(vec![fixed(1), fixed(2)])],
        vec![
            requests_changes(vec![
                bug("old-code", "cmd1"),
                requirement("untested", AGENTS_RULE),
            ]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script);

    task.run();

    let all = checks(&task);
    assert_eq!(all[0].ranges_base, None);
    let rechecked: Vec<_> = all[1].recheck.iter().map(|c| c.id).collect();
    assert_eq!(rechecked, [fid(1)]);
    assert_eq!(outcomes(&task.view()), ["Closed Fixed", "Closed Fixed"]);
    assert_replayable(&task);
}

#[test]
fn the_security_pass_runs_once_on_the_approved_commit_and_its_blocking_finding_costs_a_round() {
    let mut script = script(
        vec![implements(), fix_f1()],
        vec![
            approves(),
            requests_changes(vec![bug("injection", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    script.scans = vec![vec![spawn_hit()], vec![spawn_hit()]].into();
    let mut task = Task::two().security_pass().script(script);

    task.run();

    let view = task.view();
    assert_eq!(view.phase, approved(2, ApprovedBy::Reviewer));
    let jobs: Vec<_> = runs(&task, Role::Reviewer)
        .iter()
        .map(|run| match &run.work {
            Work::Review { job } => job.clone(),
            other => panic!("a reviewer ran {other:?}"),
        })
        .collect();
    let kinds: Vec<_> = jobs.iter().map(|job| job.pass.kind()).collect();
    assert_eq!(
        kinds,
        [PassKind::Standard, PassKind::Security, PassKind::Standard]
    );
    assert_eq!(jobs[1].diff_base, sha(0xba5e));
    assert_eq!(
        jobs[1].pass,
        Pass::Security {
            hits: vec![spawn_hit()]
        }
    );
    assert_eq!(view.rounds.len(), 2);
    assert_eq!(task.script.scans.len(), 1, "the second scan never ran");
    assert_replayable(&task);
}

#[test]
fn no_security_pass_runs_when_no_trigger_matches() {
    let mut script = script(vec![implements()], vec![approves()]);
    script.scans = vec![vec![]].into();
    let mut task = Task::two().security_pass().script(script);

    task.run();

    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_eq!(runs(&task, Role::Reviewer).len(), 1);
    assert_replayable(&task);
}

#[test]
fn the_scan_runs_only_when_the_team_runs_the_security_pass_and_the_project_has_triggers() {
    for task in [Task::two(), Task::two().security_pass().without_triggers()] {
        let mut script = script(vec![implements()], vec![approves()]);
        script.scans = vec![vec![spawn_hit()]].into();
        let mut task = task.script(script);

        task.run();

        assert!(checks(&task)[0].scan.is_none());
        assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
        assert_replayable(&task);
    }
}

#[test]
fn a_direction_after_approval_is_a_new_round_and_a_scan_with_no_hits_keeps_the_security_pass() {
    let mut script = script(
        vec![implements(), implements()],
        vec![approves(), approves(), approves()],
    );
    script.scans = vec![vec![], vec![spawn_hit()]].into();
    let mut task = Task::two().security_pass().script(script);
    task.run();
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert!(!task.view().passes_done.contains(&PassKind::Security));

    task.decide(Decision::Direct {
        text: "Also rename the route.".to_owned(),
    });

    let view = task.view();
    assert_eq!(view.phase, approved(2, ApprovedBy::Reviewer));
    assert!(view.passes_done.contains(&PassKind::Security));
    assert_replayable(&task);
}

#[test]
fn a_dispute_goes_to_the_user_when_there_is_no_planner_and_a_not_a_bug_ruling_continues_the_review()
{
    let mut script = script(
        vec![
            implements(),
            implements_answering(vec![disputed(1, "The session is never null.")]),
        ],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails()]);
    let mut task = Task::two().script(script);

    task.run();

    let view = task.view();
    assert_eq!(
        waiting(&view),
        Some(&Waiting::Ruling {
            disputed: vec![fid(1)],
            commit: sha(2)
        })
    );
    assert_eq!(runs(&task, Role::Reviewer).len(), 1);
    assert_eq!(
        decisions(&view),
        [
            DecisionKind::Rule,
            DecisionKind::Accept,
            DecisionKind::Cancel
        ]
    );
    task.decide(Decision::Rule {
        not_bug: vec![fid(1)],
    });
    let view = task.view();
    assert_eq!(view.phase, approved(2, ApprovedBy::Reviewer));
    assert_eq!(
        view.findings[0].status,
        FindingStatus::Closed {
            how: ClosedHow::RuledNotBug
        }
    );
    assert_replayable(&task);
}

#[test]
fn a_dispute_the_user_rules_a_bug_goes_back_to_the_implementer() {
    let mut script = script(
        vec![
            implements(),
            implements_answering(vec![disputed(1, "Not reachable.")]),
            fix_f1(),
        ],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script);
    task.run();

    task.decide(Decision::Rule { not_bug: vec![] });

    let implementer = runs(&task, Role::Implementer);
    assert_eq!(
        implementer[2].work,
        Work::Implement {
            brief: Brief::Rulings { bugs: vec![fid(1)] }
        }
    );
    assert_eq!(task.view().phase, approved(3, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_verdict_that_contradicts_the_confirmed_findings_is_returned_once_then_pauses_without_its_findings()
 {
    let mut script = script(
        vec![implements()],
        vec![
            approves_listing(vec![bug("null-deref", "cmd1")]),
            approves_listing(vec![bug("null-deref", "cmd1")]),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails()]);
    let mut task = Task::two().script(script);

    task.run();

    let view = task.view();
    let (why, role, _) = trouble(&view).expect("the task pauses");
    assert_eq!(why, Trouble::VerdictMismatch);
    assert_eq!(role, Role::Reviewer);
    assert_eq!(view.findings, []);
    assert_eq!(
        decisions(&view),
        [
            DecisionKind::Resume,
            DecisionKind::Direct,
            DecisionKind::StepBack,
            DecisionKind::Accept,
            DecisionKind::Cancel
        ]
    );
    assert_replayable(&task);
}

#[test]
fn a_reviewer_who_reports_blocked_pauses_the_task_without_checking_its_findings() {
    let mut task = Task::two().script(script(
        vec![implements()],
        vec![reviewer_blocked("The task contradicts AGENTS.md.")],
    ));

    task.run();

    let view = task.view();
    let (why, role, detail) = trouble(&view).expect("the task pauses");
    assert_eq!(why, Trouble::Blocked);
    assert_eq!(role, Role::Reviewer);
    assert_eq!(detail, "The task contradicts AGENTS.md.");
    assert_eq!(checks(&task).len(), 0);
    assert_replayable(&task);
}
