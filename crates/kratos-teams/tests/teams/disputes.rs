//! Disputes (D31). The implementer can answer a finding with "disputed" and a reason. With a planner,
//! the planner rules on every disputed finding. With none, the user rules. The reviewer never rules on
//! its own finding. A planner's ruling takes effect at once, and its reasons stay in the record.

use kratos_teams::decisions::{Decision, DecisionKind, check_decision};
use kratos_teams::facts::{Ended, Fact};
use kratos_teams::fold::{Rejection, fold};
use kratos_teams::members::Role;
use kratos_teams::reports::RulingVerdict;
use kratos_teams::view::{
    Answerer, ApprovedBy, Brief, ClosedHow, FindingStatus, Phase, Trouble, Waiting, Work,
};

use crate::replay::*;
use crate::support::*;

/// A three-member task in round 2 after the implementer disputed `disputes` findings the reviewer
/// confirmed in round 1. The planner is ready to rule next.
fn disputed_team(
    disputes: &[u32],
    planner: Vec<Ended>,
    implementer: Vec<Ended>,
    reviewer_after: Vec<Ended>,
) -> ScriptedTask {
    let mut plans = vec![plan_ready()];
    plans.extend(planner);
    let findings = vec![bug("null-deref", "cmd1"), bug("race", "cmd2")];
    let answers = [1, 2]
        .iter()
        .map(|id| {
            if disputes.contains(id) {
                disputed(*id, "That cannot happen.")
            } else {
                fixed(*id)
            }
        })
        .collect();
    let mut implementer_turns = vec![implements(), implements_answering(answers)];
    implementer_turns.extend(implementer);
    let mut reviewer = vec![requests_changes(findings)];
    reviewer.extend(reviewer_after);
    let mut script = Script {
        planner: plans.into(),
        implementer: implementer_turns.into(),
        reviewer: reviewer.into(),
        ..Script::default()
    };
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    script_commands(&mut script, "cmd2", vec![fails(), passes()]);
    let mut task = Task::three().script(script);
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    task
}

#[test]
fn with_a_planner_the_planner_rules_and_a_not_a_bug_ruling_continues_the_review_at_once() {
    let task = disputed_team(
        &[1, 2],
        vec![planner_rules(&[
            (1, RulingVerdict::NotBug),
            (2, RulingVerdict::NotBug),
        ])],
        vec![],
        vec![approves()],
    );

    let view = task.view();

    assert_eq!(view.phase, approved(2, ApprovedBy::Reviewer));
    assert_eq!(
        outcomes(&view),
        ["Closed RuledNotBug", "Closed RuledNotBug"]
    );
    let ruling = runs(&task, Role::Planner).pop().expect("the planner ruled");
    assert!(matches!(ruling.work, Work::Rule { .. }));
    assert!(ruling.plan_mode);
    assert_eq!(
        runs(&task, Role::Implementer).len(),
        2,
        "no implementer step runs when every finding is ruled not a bug"
    );
    assert_replayable(&task);
}

#[test]
fn a_planners_ruling_is_recorded_with_its_reasons_and_asks_the_user_to_confirm_nothing() {
    let task = disputed_team(
        &[1, 2],
        vec![planner_rules(&[
            (1, RulingVerdict::NotBug),
            (2, RulingVerdict::Bug),
        ])],
        vec![implements_answering(vec![fixed(2)])],
        vec![approves()],
    );

    let view = task.view();

    assert_eq!(view.rulings.len(), 2);
    assert_eq!(view.rulings[0].finding, fid(1));
    assert_eq!(view.rulings[0].verdict, RulingVerdict::NotBug);
    assert_eq!(view.rulings[0].reason, "Because the evidence says so.");
    assert_eq!(view.rulings[0].by, Answerer::Planner);
    let ruling_message = runs(&task, Role::Planner)
        .last()
        .expect("the planner ruled")
        .message
        .clone();
    let ruled_at = task
        .log()
        .iter()
        .position(|stored| {
            matches!(&stored.fact, Fact::TurnEnded { message, .. } if *message == ruling_message)
        })
        .expect("the ruling was recorded");
    let just_after = fold(&task.log()[..=ruled_at]).expect("the prefix folds");
    assert!(
        matches!(just_after.phase, Phase::Active { .. }),
        "the ruling took effect with no confirmation from the user"
    );
}

#[test]
fn a_bug_ruling_sends_only_those_findings_back_to_the_implementer_with_the_ruling() {
    let task = disputed_team(
        &[1, 2],
        vec![planner_rules(&[
            (1, RulingVerdict::NotBug),
            (2, RulingVerdict::Bug),
        ])],
        vec![implements_answering(vec![fixed(2)])],
        vec![approves()],
    );

    let implementer = runs(&task, Role::Implementer);

    assert_eq!(
        implementer[2].work,
        Work::Implement {
            brief: Brief::Rulings { bugs: vec![fid(2)] }
        }
    );
    let view = task.view();
    assert_eq!(view.phase, approved(3, ApprovedBy::Reviewer));
    assert_eq!(
        view.findings[0].status,
        FindingStatus::Closed {
            how: ClosedHow::RuledNotBug
        }
    );
    assert_replayable(&task);
}

#[test]
fn a_ruling_must_cover_every_disputed_finding_exactly_once_or_it_goes_back() {
    let task = disputed_team(
        &[1, 2],
        vec![
            planner_rules(&[(1, RulingVerdict::NotBug)]),
            planner_rules(&[(1, RulingVerdict::NotBug), (2, RulingVerdict::NotBug)]),
        ],
        vec![],
        vec![approves()],
    );

    let planner = runs(&task, Role::Planner);

    assert_eq!(
        planner[2].note,
        Some(kratos_teams::view::Note::InvalidReport {
            error: "Rule on every disputed finding. Missing: F2.".to_owned()
        })
    );
    assert_eq!(task.view().phase, approved(2, ApprovedBy::Reviewer));
}

#[test]
fn a_finding_ruled_on_twice_sends_the_ruling_back() {
    let task = disputed_team(
        &[1, 2],
        vec![
            planner_rules(&[
                (1, RulingVerdict::NotBug),
                (1, RulingVerdict::Bug),
                (2, RulingVerdict::NotBug),
            ]),
            planner_rules(&[(1, RulingVerdict::NotBug), (2, RulingVerdict::NotBug)]),
        ],
        vec![],
        vec![approves()],
    );

    let planner = runs(&task, Role::Planner);

    assert_eq!(
        planner[2].note,
        Some(kratos_teams::view::Note::InvalidReport {
            error: "Rule on each finding once. Ruled more than once: F1.".to_owned()
        })
    );
    assert_eq!(task.view().phase, approved(2, ApprovedBy::Reviewer));
}

#[test]
fn a_ruling_for_a_finding_nobody_disputed_goes_back_too() {
    let task = disputed_team(
        &[1],
        vec![
            planner_rules(&[(1, RulingVerdict::NotBug), (2, RulingVerdict::NotBug)]),
            planner_rules(&[(1, RulingVerdict::NotBug)]),
        ],
        vec![],
        vec![approves()],
    );

    let planner = runs(&task, Role::Planner);

    assert_eq!(
        planner[2].note,
        Some(kratos_teams::view::Note::InvalidReport {
            error: "Rule only on the disputed findings. Not disputed: F2.".to_owned()
        })
    );
}

#[test]
fn a_planner_that_cannot_rule_lets_the_user_rule_the_same_way_a_team_without_a_planner_does() {
    let mut task = disputed_team(
        &[1],
        vec![planner_rules(&[]), planner_rules(&[])],
        vec![],
        vec![approves()],
    );

    let view = task.view();
    assert_eq!(
        trouble(&view).map(|(why, role, _)| (why, role)),
        Some((Trouble::InvalidReport, Role::Planner))
    );
    assert_eq!(
        decisions(&view),
        [
            DecisionKind::Resume,
            DecisionKind::Rule,
            DecisionKind::Cancel
        ]
    );

    task.decide(Decision::Rule {
        not_bug: vec![fid(1)],
    });

    let view = task.view();
    assert_eq!(view.phase, approved(2, ApprovedBy::Reviewer));
    assert_eq!(view.rulings[0].by, Answerer::User);
    assert_replayable(&task);
}

#[test]
fn a_planner_that_is_unsure_asks_the_user_and_does_not_guess_the_ruling() {
    let mut task = disputed_team(
        &[1],
        vec![
            planner_ruling_question("Does the helper ever return null?"),
            planner_rules(&[(1, RulingVerdict::NotBug)]),
        ],
        vec![],
        vec![approves()],
    );

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::Answer));
    assert_eq!(view.pending_question().map(|q| q.role), Some(Role::Planner));
    task.decide(Decision::Answer {
        text: "Never.".to_owned(),
    });

    let planner = runs(&task, Role::Planner);
    assert_eq!(planner[2].step, planner[1].step, "the ruling step went on");
    assert_eq!(task.view().phase, approved(2, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn the_user_cannot_rule_on_a_finding_nobody_disputed() {
    let task = disputed_team(
        &[1],
        vec![planner_rules(&[]), planner_rules(&[])],
        vec![],
        vec![],
    );

    let refused = check_decision(
        &task.view(),
        &Decision::Rule {
            not_bug: vec![fid(2)],
        },
    );

    assert!(matches!(refused, Err(Rejection::InvalidDecision { .. })));
}

#[test]
fn the_reviewer_is_never_asked_to_rule_on_its_own_finding() {
    let task = disputed_team(
        &[1],
        vec![planner_rules(&[(1, RulingVerdict::NotBug)])],
        vec![],
        vec![approves()],
    );

    let reviewer_work: Vec<_> = runs(&task, Role::Reviewer)
        .iter()
        .map(|run| matches!(run.work, Work::Review { .. }))
        .collect();

    assert!(reviewer_work.iter().all(|is_review| *is_review));
}

#[test]
fn a_dispute_is_not_a_fix_attempt_so_a_finding_ruled_a_bug_still_gets_two() {
    let mut script = Script {
        implementer: vec![
            implements(),
            implements_answering(vec![disputed(1, "That cannot happen.")]),
            implements_answering(vec![fixed(1)]),
            implements_answering(vec![fixed(1)]),
        ]
        .into(),
        reviewer: vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            still_open(1, "Still crashes."),
            still_open(1, "Still crashes."),
        ]
        .into(),
        ..Script::default()
    };
    script_commands(&mut script, "cmd1", vec![fails(), fails(), fails()]);
    let mut task = Task::two().script(script);
    task.run();
    task.decide(Decision::Rule { not_bug: vec![] });

    let view = task.view();

    assert_eq!(
        waiting(&view),
        Some(&Waiting::Findings {
            why: kratos_teams::view::FindingsPause::Stuck,
            ids: vec![fid(1)]
        }),
        "stuck only after the second fix, and the dispute did not count as one"
    );
    assert_eq!(runs(&task, Role::Implementer).len(), 4);
}

#[test]
fn a_commit_that_waited_on_a_ruling_uses_no_round_of_the_cap() {
    let mut script = Script {
        implementer: vec![
            implements(),
            implements_answering(vec![disputed(1, "That cannot happen.")]),
            implements_answering(vec![fixed(1)]),
            implements_answering(vec![fixed(1)]),
        ]
        .into(),
        reviewer: vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            still_open(1, "Still crashes."),
            approves(),
        ]
        .into(),
        ..Script::default()
    };
    script_commands(&mut script, "cmd1", vec![fails(), fails(), passes()]);
    let mut task = Task::two().script(script);
    task.run();

    task.decide(Decision::Rule { not_bug: vec![] });

    assert_eq!(
        task.view().phase,
        approved(4, ApprovedBy::Reviewer),
        "four commits, three reviewed rounds, so the cap of three was not reached"
    );
}
