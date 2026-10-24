//! The handoff check and the corrections that follow it: invalid reports, unchecked edits, lost
//! turns, and what a resume asks again. Ported from the TypeScript `teams-task.test.ts`.

use kratos_teams::decisions::{Decision, DecisionKind};
use kratos_teams::facts::Outcome;
use kratos_teams::facts::{Ended, Fact, FailedStep};
use kratos_teams::members::Role;
use kratos_teams::next_step::Step;
use kratos_teams::view::{ApprovedBy, Note, Phase, Trouble};

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

fn closed(outcome: Outcome) -> Phase {
    Phase::Closed { outcome }
}

#[test]
fn an_unchecked_edit_is_returned_once_pauses_on_the_second_and_a_resume_asks_with_the_same_note() {
    let mut task = Task::two().script(script(
        vec![implements_unchecked(), implements_unchecked(), implements()],
        vec![approves()],
    ));

    task.run();

    let view = task.view();
    assert_eq!(
        trouble(&view).map(|(why, role, _)| (why, role)),
        Some((Trouble::UncheckedEdits, Role::Implementer))
    );
    assert_eq!(
        runs(&task, Role::Implementer)[1].note,
        Some(Note::UncheckedEdits)
    );
    task.decide(Decision::Resume);
    assert_eq!(
        runs(&task, Role::Implementer)[2].note,
        Some(Note::UncheckedEdits)
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn an_invalid_report_is_returned_once_and_pauses_on_the_second() {
    let mut task = Task::two().script(script(
        vec![
            implementer_invalid("summary: must not be blank"),
            implementer_invalid("Bad JSON."),
        ],
        vec![],
    ));

    task.run();

    assert_eq!(
        runs(&task, Role::Implementer)[1].note,
        Some(Note::InvalidReport {
            error: "summary: must not be blank".to_owned()
        })
    );
    let view = task.view();
    assert_eq!(
        trouble(&view),
        Some((Trouble::InvalidReport, Role::Implementer, "Bad JSON."))
    );
    assert_replayable(&task);
}

#[test]
fn a_blocked_report_pauses_at_once_with_no_correction() {
    let mut task = Task::two().script(script(
        vec![implementer_blocked("The database is down."), implements()],
        vec![approves()],
    ));

    task.run();

    let view = task.view();
    assert_eq!(
        trouble(&view),
        Some((Trouble::Blocked, Role::Implementer, "The database is down."))
    );
    assert_eq!(runs(&task, Role::Implementer).len(), 1);
    task.decide(Decision::Resume);
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
}

#[test]
fn every_open_finding_needs_a_response_after_round_one() {
    let mut script = script(
        vec![implements(), implements(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script);

    task.run();

    assert_eq!(
        runs(&task, Role::Implementer)[2].note,
        Some(Note::InvalidReport {
            error: "Answer every open finding in responses. Missing: F1.".to_owned()
        })
    );
    assert_eq!(task.view().phase, approved(2, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn exactly_one_response_per_open_finding_and_a_reason_for_each_dispute() {
    let mut script = script(
        vec![
            implements(),
            implements_answering(vec![fixed(1), fixed(1)]),
            implements_answering(vec![disputed(1, "  ")]),
        ],
        vec![requests_changes(vec![bug("null-deref", "cmd1")])],
    );
    script_commands(&mut script, "cmd1", vec![fails()]);
    let mut task = Task::two().script(script);

    task.run();

    assert_eq!(
        runs(&task, Role::Implementer)[2].note,
        Some(Note::InvalidReport {
            error: "Answer each finding once. Answered more than once: F1.".to_owned()
        })
    );
    let view = task.view();
    assert_eq!(
        trouble(&view),
        Some((
            Trouble::InvalidReport,
            Role::Implementer,
            "Give a reason for each dispute: F1."
        ))
    );
    assert_replayable(&task);
}

#[test]
fn the_handoff_is_checked_in_order_the_responses_before_the_unchecked_edits() {
    let mut script = script(
        vec![
            implements(),
            implements_with(done(), edited_after_check()),
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

    assert_eq!(
        notes(&runs(&task, Role::Implementer)),
        [None, None, Some("invalid_report".to_owned())]
    );
    assert_eq!(task.view().phase, approved(2, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn an_unchecked_edit_survives_a_turn_that_restates_its_report_without_running_a_command() {
    let mut task = Task::two().script(script(
        vec![
            implementer_invalid_after_edit("summary: must not be blank"),
            implements_without_running(),
            implements(),
        ],
        vec![approves()],
    ));

    task.run();

    assert_eq!(
        notes(&runs(&task, Role::Implementer)),
        [
            None,
            Some("invalid_report".to_owned()),
            Some("unchecked_edits".to_owned())
        ]
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_lost_turn_counts_as_an_unchecked_edit_because_its_change_record_is_gone() {
    let mut task = Task::two().script(script(
        vec![lost(), implements_without_running(), implements()],
        vec![approves()],
    ));

    task.run();

    assert_eq!(
        notes(&runs(&task, Role::Implementer)),
        [
            None,
            Some("lost".to_owned()),
            Some("unchecked_edits".to_owned())
        ]
    );
    assert_replayable(&task);
}

#[test]
fn each_cause_of_trouble_gets_its_own_retry_and_a_second_of_the_same_cause_pauses() {
    let mut task = Task::two().script(script(
        vec![
            implementer_invalid("status: invalid option"),
            implements_unchecked(),
            implementer_invalid("Bad JSON."),
        ],
        vec![],
    ));

    task.run();

    assert_eq!(
        notes(&runs(&task, Role::Implementer)),
        [
            None,
            Some("invalid_report".to_owned()),
            Some("unchecked_edits".to_owned())
        ]
    );
    assert_eq!(
        trouble(&task.view()),
        Some((Trouble::InvalidReport, Role::Implementer, "Bad JSON."))
    );
    assert_replayable(&task);
}

#[test]
fn a_lost_implementer_turn_is_retried_once_then_pauses_and_a_resume_goes_on() {
    let mut task = Task::two().script(script(vec![lost(), lost(), implements()], vec![approves()]));

    task.run();

    assert_eq!(
        trouble(&task.view()).map(|(why, role, _)| (why, role)),
        Some((Trouble::Lost, Role::Implementer))
    );
    task.decide(Decision::Resume);
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    let asked = runs(&task, Role::Implementer);
    assert_eq!(asked.len(), 3);
    assert!(asked.iter().all(|run| run.member == coder()));
    assert_replayable(&task);
}

#[test]
fn a_lost_reviewer_turn_is_retried_once_in_a_new_session() {
    let mut task = Task::two().script(script(
        vec![implements()],
        vec![
            Ended::Lost {
                reason: "The turn timed out.".to_owned(),
            },
            approves(),
        ],
    ));

    task.run();

    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    let reviews = runs(&task, Role::Reviewer);
    assert_eq!(
        reviews[1].follows, None,
        "a lost review starts over, in a new session"
    );
    assert_ne!(reviews[0].message, reviews[1].message);
    assert_replayable(&task);
}

#[test]
fn a_reviewer_is_corrected_in_its_own_session_once_per_cause_and_the_next_round_opens_a_new_one() {
    let mut script = script(
        vec![implements(), fix_f1()],
        vec![
            reviewer_invalid("verdict: invalid option"),
            approves_listing(vec![bug("null-deref", "cmd1")]),
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), fails(), passes()]);
    let mut task = Task::two().script(script);

    task.run();

    let reviews = runs(&task, Role::Reviewer);
    assert_eq!(
        notes(&reviews[..3]),
        [
            None,
            Some("invalid_report".to_owned()),
            Some("verdict_mismatch".to_owned())
        ]
    );
    assert_eq!(reviews[1].follows.as_ref(), Some(&reviews[0].message));
    assert_eq!(reviews[2].follows.as_ref(), Some(&reviews[1].message));
    assert_eq!(reviews[3].follows, None);
    assert_eq!(task.view().phase, approved(2, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_reviewer_that_sends_an_invalid_report_twice_pauses_the_task() {
    let mut task = Task::two().script(script(
        vec![implements()],
        vec![reviewer_invalid("Bad JSON."), reviewer_invalid("Bad JSON.")],
    ));

    task.run();

    assert_eq!(
        trouble(&task.view()).map(|(why, role, _)| (why, role)),
        Some((Trouble::InvalidReport, Role::Reviewer))
    );
    assert_replayable(&task);
}

#[test]
fn a_new_reviewer_session_opens_after_a_lost_turn_in_the_middle_of_a_round() {
    let mut task = Task::two().script(script(
        vec![implements()],
        vec![
            reviewer_invalid("Bad JSON."),
            Ended::Lost {
                reason: "The process exited.".to_owned(),
            },
            approves(),
        ],
    ));

    task.run();

    let reviews = runs(&task, Role::Reviewer);
    assert_eq!(reviews[1].follows.as_ref(), Some(&reviews[0].message));
    assert_eq!(reviews[2].follows, None);
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_failed_evidence_check_resumes_without_asking_the_reviewer_again() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    while !matches!(task.next(), Step::Check { .. }) {
        task.step();
    }
    task.record(Fact::StepFailed {
        step: FailedStep::Check,
        error: "The sandbox crashed.".to_owned(),
    });

    assert_eq!(
        trouble(&task.view()).map(|(why, role, detail)| (why, role, detail.to_owned())),
        Some((
            Trouble::System,
            Role::Reviewer,
            "check: The sandbox crashed.".to_owned()
        ))
    );
    task.decide(Decision::Resume);

    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_eq!(runs(&task, Role::Reviewer).len(), 1);
    assert_replayable(&task);
}

#[test]
fn a_failed_preparation_offers_only_resume_and_cancel_because_no_work_exists_to_direct() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    task.record(Fact::StepFailed {
        step: FailedStep::Prepare,
        error: "git worktree add failed".to_owned(),
    });

    let view = task.view();
    assert_eq!(
        trouble(&view),
        Some((
            Trouble::System,
            Role::Implementer,
            "prepare: git worktree add failed"
        ))
    );
    assert_eq!(
        decisions(&view),
        [DecisionKind::Resume, DecisionKind::Cancel]
    );
    assert_eq!(task.decide(Decision::Resume)[0], "prepare");
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_fact_for_a_step_the_task_is_not_in_is_refused() {
    let task = Task::two().script(script(vec![], vec![]));

    let refused = task.view().accepts(&Fact::StepFailed {
        step: FailedStep::Commit,
        error: "nope".to_owned(),
    });

    assert!(refused.is_err());
}

#[test]
fn accepting_commits_the_work_first_so_the_approval_covers_that_commit_and_later_work_starts_clean()
{
    let mut task = Task::two().script(script(
        vec![
            implementer_blocked("Should the form check emails?"),
            implements(),
        ],
        vec![approves()],
    ));
    task.run();

    assert_eq!(task.decide(Decision::Accept), ["commit"]);

    let view = task.view();
    assert_eq!(view.phase, approved(1, ApprovedBy::User));
    assert_eq!(view.rounds.len(), 1);
    assert_eq!(view.rounds[0].report, None);
    assert!(view.rounds[0].reviews.is_empty());
    task.decide(Decision::Direct {
        text: "Rename the route.".to_owned(),
    });
    assert_eq!(task.view().phase, approved(2, ApprovedBy::Reviewer));
    assert_eq!(
        task.decide(Decision::Finish {
            outcome: kratos_teams::decisions::Finish::Rejected
        }),
        ["cleanup"]
    );
    assert_eq!(task.view().phase, closed(Outcome::Rejected));
    assert_replayable(&task);
}

#[test]
fn an_approved_task_finishes_with_the_users_outcome_cleans_up_and_then_accepts_nothing() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    task.run();
    assert_eq!(
        decisions(&task.view()),
        [
            DecisionKind::Direct,
            DecisionKind::Finish,
            DecisionKind::Cancel
        ]
    );

    let ran = task.decide(Decision::Finish {
        outcome: kratos_teams::decisions::Finish::Merged,
    });

    assert_eq!(ran, ["cleanup"]);
    let view = task.view();
    assert_eq!(view.phase, closed(Outcome::Merged));
    assert_eq!(kratos_teams::decisions::allowed_decisions(&view), []);
    assert_replayable(&task);
}

#[test]
fn the_user_can_cancel_at_every_point_where_an_agent_or_kratos_is_working() {
    let mut script_ = script(
        vec![implements(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script_, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script_);
    task.run();

    let mut cancelled = 0;
    for n in 1..=task.log().len() {
        let mut earlier = ScriptedTask::restart_from(&task.log()[..n], Script::default());
        if matches!(earlier.next(), Step::Wait { .. }) {
            continue;
        }
        assert_eq!(
            decisions(&earlier.view()),
            [DecisionKind::Cancel],
            "after fact {n}"
        );
        assert_eq!(earlier.decide(Decision::Cancel), ["cleanup"]);
        assert_eq!(earlier.view().phase, closed(Outcome::Cancelled));
        cancelled += 1;
    }

    assert_eq!(cancelled, 9);
}
