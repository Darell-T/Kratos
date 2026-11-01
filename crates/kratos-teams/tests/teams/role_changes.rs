//! Role changes (D77). The user can move roles between the Team's own members without losing either
//! member's thread or history. The change becomes pending, the running step finishes, and the change
//! applies right before the next new step. Nothing finished is repeated.

use kratos_teams::decisions::{Decision, DecisionKind, allowed_decisions, check_decision};
use kratos_teams::facts::{Ended, Fact};
use kratos_teams::fold::Rejection;
use kratos_teams::members::{Bindings, Role};
use kratos_teams::next_step::Step;
use kratos_teams::view::{ApprovedBy, FindingStatus, Trouble, Waiting, Work};

use crate::replay::*;
use crate::support::*;

fn script(implementer: Vec<Ended>, reviewer: Vec<Ended>) -> Script {
    Script {
        implementer: implementer.into(),
        reviewer: reviewer.into(),
        ..Script::default()
    }
}

fn swap() -> Decision {
    Decision::ReassignRoles {
        to: swapped_members(),
    }
}

fn until_next_is(task: &mut ScriptedTask, wanted: fn(&Step) -> bool) {
    task.step_until(|task| wanted(&task.next()));
}

fn is_review_run(step: &Step) -> bool {
    matches!(step, Step::Run { run } if run.role == Role::Reviewer)
}

#[test]
fn the_running_step_finishes_under_its_old_holder_and_the_change_applies_before_the_next_new_step()
{
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    until_next_is(&mut task, |step| matches!(step, Step::Run { .. }));

    task.decide_while_working(swap());
    task.run();

    let steps: Vec<&Step> = task.steps.iter().collect();
    let apply = steps
        .iter()
        .position(|step| matches!(step, Step::ApplyRoles { .. }))
        .expect("the change was applied");
    let first_review = steps
        .iter()
        .position(|step| is_review_run(step))
        .expect("the review ran");
    let build = steps
        .iter()
        .position(|step| matches!(step, Step::Run { run } if run.role == Role::Implementer))
        .expect("the implementer ran");
    assert!(build < apply && apply < first_review);
    assert_eq!(runs(&task, Role::Implementer)[0].member, coder());
    let Step::Run { run } = steps[first_review] else {
        panic!("a run");
    };
    assert_eq!(run.member, coder(), "the former implementer now reviews");
    let view = task.view();
    assert_eq!(view.bindings, swapped_members());
    assert_eq!(view.pending_roles, None);
    assert_eq!(view.phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_member_who_now_reviews_gets_a_review_package_and_never_its_own_earlier_conversation() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    until_next_is(&mut task, |step| matches!(step, Step::Run { .. }));
    task.decide_while_working(swap());

    task.run();

    let review = &runs(&task, Role::Reviewer)[0];
    assert_eq!(review.member, coder());
    assert_eq!(review.follows, None, "a review always opens a new session");
}

#[test]
fn nothing_finished_is_repeated_and_the_record_stays_whole() {
    let mut script = script(
        vec![implements(), implements_answering(vec![fixed(1)])],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script);
    until_next_is(&mut task, is_review_run);
    task.decide_while_working(swap());

    task.run();

    let view = task.view();
    assert_eq!(view.rounds.len(), 2);
    assert_eq!(view.phase, approved(2, ApprovedBy::Reviewer));
    assert_eq!(
        task.steps
            .iter()
            .filter(|step| matches!(step, Step::Prepare))
            .count(),
        1,
        "the worktree is not prepared again"
    );
    let builders: Vec<_> = runs(&task, Role::Implementer)
        .iter()
        .map(|run| run.member.clone())
        .collect();
    assert_eq!(
        builders,
        [coder(), checker()],
        "the former reviewer fixes round 2"
    );
    assert_replayable(&task);
}

#[test]
fn a_change_requested_while_a_question_waits_applies_after_the_asking_step_finishes() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready(), planner_answers("Postgres.")].into(),
        implementer: vec![implementer_asks("Which database?"), implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();
    task.decide_while_working(Decision::ApprovePlan { conditions: None });
    until_next_is(
        &mut task,
        |step| matches!(step, Step::Run { run } if run.role == Role::Planner && run.plan_mode && matches!(run.work, kratos_teams::view::Work::Answer { .. })),
    );
    task.decide_while_working(Decision::ReassignRoles {
        to: swap_implementer_and_reviewer(&task.view().bindings),
    });

    task.run();

    let apply = task
        .steps
        .iter()
        .position(|step| matches!(step, Step::ApplyRoles { .. }))
        .expect("the change applied");
    let resumed = task
        .steps
        .iter()
        .rposition(|step| matches!(step, Step::Run { run } if run.role == Role::Implementer))
        .expect("the asking step resumed");
    assert!(
        resumed < apply,
        "the asker finished its step under the old roles before the change applied"
    );
    assert_replayable(&task);
}

#[test]
fn a_change_pending_when_a_question_is_asked_waits_until_the_asking_step_finishes() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready(), planner_answers("Postgres.")].into(),
        implementer: vec![implementer_asks("Which database?"), implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();
    task.decide_while_working(Decision::ApprovePlan { conditions: None });
    task.decide_while_working(Decision::ReassignRoles {
        to: swap_implementer_and_reviewer(&task.view().bindings),
    });

    task.run();

    let position = |wanted: fn(&Step) -> bool| task.steps.iter().position(wanted);
    let answered = position(
        |step| matches!(step, Step::Run { run } if matches!(run.work, Work::Answer { .. })),
    )
    .expect("the planner answered");
    let resumed = task
        .steps
        .iter()
        .rposition(|step| matches!(step, Step::Run { run } if run.role == Role::Implementer))
        .expect("the asking step resumed");
    let applied =
        position(|step| matches!(step, Step::ApplyRoles { .. })).expect("the change applied");
    assert!(
        answered < resumed && resumed < applied,
        "the question is answered and the asking step finishes before the roles change"
    );
    let Step::Run { run } = &task.steps[resumed] else {
        panic!("a run");
    };
    assert_eq!(run.member, coder(), "the asker finishes its own step");
    assert_replayable(&task);
}

#[test]
fn a_change_that_adds_a_member_removes_one_or_changes_nothing_is_refused() {
    let task = Task::two().script(script(vec![], vec![]));
    let view = task.view();
    let stranger = Bindings::two(
        member("codex-1", kratos_protocol::Provider::Codex),
        member("grok-1", kratos_protocol::Provider::Claude),
    )
    .expect("distinct");
    let with_planner = three_members();
    let same = two_members();

    let refuse = |to: Bindings| check_decision(&view, &Decision::ReassignRoles { to });

    assert!(matches!(
        refuse(stranger),
        Err(Rejection::InvalidDecision { .. })
    ));
    assert!(matches!(
        refuse(with_planner),
        Err(Rejection::InvalidDecision { .. })
    ));
    assert!(matches!(
        refuse(same),
        Err(Rejection::InvalidDecision { .. })
    ));
    assert_eq!(refuse(swapped_members()), Ok(()));
}

#[test]
fn a_three_member_team_can_rearrange_every_role() {
    let task = Task::three().script(script(vec![], vec![]));
    let rotated = Bindings::three(
        member("codex-1", kratos_protocol::Provider::Codex),
        member("claude-1", kratos_protocol::Provider::Claude),
        member("claude-plan", kratos_protocol::Provider::Claude),
    )
    .expect("distinct");

    assert_eq!(
        check_decision(&task.view(), &Decision::ReassignRoles { to: rotated }),
        Ok(())
    );
}

#[test]
fn asking_for_the_current_roles_while_a_change_is_pending_takes_the_request_back() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    until_next_is(&mut task, |step| matches!(step, Step::Run { .. }));
    task.decide_while_working(swap());
    assert!(task.view().pending_roles.is_some());

    task.decide_while_working(Decision::ReassignRoles { to: two_members() });

    assert_eq!(task.view().pending_roles, None);
    assert!(matches!(
        task.view().accepts(&Fact::RolesReassigned),
        Err(Rejection::Misfit { .. })
    ));
    task.run();
    assert!(
        task.steps
            .iter()
            .all(|step| !matches!(step, Step::ApplyRoles { .. }))
    );
    assert_eq!(task.view().bindings, two_members());
    assert_replayable(&task);
}

#[test]
fn withdrawal_is_rejected_once_the_role_change_checkpoint_has_started() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    until_next_is(&mut task, |step| matches!(step, Step::Run { .. }));
    task.decide_while_working(swap());
    until_next_is(&mut task, |step| matches!(step, Step::ApplyRoles { .. }));
    let checkpoint = task.next();
    let withdrawal = Decision::ReassignRoles { to: two_members() };

    assert!(!allowed_decisions(&task.view()).contains(&DecisionKind::ReassignRoles));
    assert!(matches!(
        check_decision(&task.view(), &withdrawal),
        Err(Rejection::NotAllowed {
            decision: DecisionKind::ReassignRoles,
            ..
        })
    ));
    assert!(matches!(
        task.append(Fact::UserDecided {
            decision: withdrawal
        }),
        Err(Rejection::NotAllowed {
            decision: DecisionKind::ReassignRoles,
            ..
        })
    ));
    assert_eq!(
        task.next(),
        checkpoint,
        "the rejected withdrawal changes no authorization"
    );
    assert_eq!(task.view().pending_roles, Some(swapped_members()));

    task.run();

    assert_eq!(task.view().bindings, swapped_members());
    assert_eq!(runs(&task, Role::Reviewer)[0].member, coder());
    assert!(matches!(
        task.view().accepts(&Fact::RolesReassigned),
        Err(Rejection::Misfit { .. })
    ));
    assert_replayable(&task);
}

#[test]
fn an_engine_that_cannot_apply_the_change_leaves_the_old_roles_and_pauses_with_the_reason() {
    let mut script = script(vec![implements()], vec![approves()]);
    script.role_change_fails = true;
    let mut task = Task::two().script(script);
    until_next_is(&mut task, |step| matches!(step, Step::Run { .. }));
    task.decide_while_working(swap());

    task.run();

    let view = task.view();
    assert_eq!(view.bindings, two_members());
    assert_eq!(view.pending_roles, None);
    assert_eq!(
        trouble(&view),
        Some((
            Trouble::RoleChange,
            Role::Reviewer,
            "the reviewer is not signed in"
        ))
    );
    assert!(matches!(
        check_decision(&view, &Decision::ReassignRoles { to: two_members() }),
        Err(Rejection::InvalidDecision { .. })
    ));
    assert!(matches!(
        view.accepts(&Fact::RolesReassigned),
        Err(Rejection::Misfit { .. })
    ));
    task.decide(Decision::Resume);
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_eq!(
        runs(&task, Role::Reviewer)[0].member,
        checker(),
        "the old reviewer reviewed"
    );
    assert_replayable(&task);
}

#[test]
fn a_plan_waiting_for_approval_does_not_block_a_request_and_the_change_applies_after_approval() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        implementer: vec![implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();
    assert_eq!(waiting(&task.view()), Some(&Waiting::PlanApproval));

    task.decide(Decision::ReassignRoles {
        to: swap_implementer_and_reviewer(&task.view().bindings),
    });
    assert_eq!(waiting(&task.view()), Some(&Waiting::PlanApproval));
    task.decide(Decision::ApprovePlan { conditions: None });

    let building = &runs(&task, Role::Implementer)[0];
    assert_eq!(
        building.member,
        checker(),
        "the new implementer builds the approved plan"
    );
    assert_replayable(&task);
}

#[test]
fn applying_a_change_gives_the_new_holder_fresh_fix_attempts_but_no_extra_rounds() {
    let mut script = script(
        vec![
            implements(),
            implements_answering(vec![fixed(1)]),
            implements_answering(vec![fixed(1)]),
        ],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            still_open(1, "Still crashes."),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), fails(), passes()]);
    let mut task = Task::two().script(script);
    // Stop while round 2 is being checked, after one fix attempt on F1. The next new step is the
    // implementer's third, and the change applies right before it.
    task.step_until(|task| {
        matches!(task.next(), Step::Check { .. }) && task.view().rounds.len() == 2
    });
    assert!(matches!(
        task.view().findings[0].status,
        FindingStatus::Open { attempts: 1, .. }
    ));
    task.decide_while_working(swap());

    task.run();

    let applied_at = task
        .log()
        .iter()
        .position(|stored| matches!(stored.fact, kratos_teams::facts::Fact::RolesReassigned))
        .expect("the change applied");
    let just_after = kratos_teams::fold::fold(&task.log()[..=applied_at]).expect("folds");
    assert!(matches!(
        just_after.findings[0].status,
        FindingStatus::Open { attempts: 0, .. }
    ));
    assert_eq!(just_after.leg_start, 0, "a role change adds no rounds");
    assert_eq!(just_after.rounds.len(), 2);
    assert_eq!(task.view().phase, approved(3, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_step_keeps_its_id_when_a_role_change_holds_it_back() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    until_next_is(&mut task, |step| matches!(step, Step::Run { .. }));
    task.decide_while_working(swap());
    until_next_is(&mut task, |step| matches!(step, Step::ApplyRoles { .. }));
    let kratos_teams::view::Phase::Active {
        activity: kratos_teams::view::Activity::Rebinding { step, .. },
    } = task.view().phase
    else {
        panic!("the review waits behind the role change");
    };

    task.run();

    assert_eq!(runs(&task, Role::Reviewer)[0].step, step);
}
