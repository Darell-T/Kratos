//! Cancelling, and results that arrive late. The user can cancel whenever Kratos or an agent works or
//! waits for them. A result that arrives after a cancel, a duplicate result, or a result from a step the
//! user replaced is dropped, and it cannot restart anything.

use kratos_teams::decisions::{Decision, DecisionKind};
use kratos_teams::facts::{Declined, Ended, Fact, Outcome};
use kratos_teams::fold::Rejection;
use kratos_teams::members::Role;
use kratos_teams::view::{DirectionStatus, Phase};

use crate::replay::*;
use crate::support::*;

fn script(implementer: Vec<Ended>, reviewer: Vec<Ended>) -> Script {
    Script {
        implementer: implementer.into(),
        reviewer: reviewer.into(),
        ..Script::default()
    }
}

fn closed_as_cancelled(task: &ScriptedTask) -> bool {
    task.view().phase
        == Phase::Closed {
            outcome: Outcome::Cancelled,
        }
}

fn assert_cancellable(mut task: ScriptedTask) {
    assert!(
        decisions(&task.view()).contains(&DecisionKind::Cancel),
        "cancel is offered while the task is {}",
        task.view().phase.name()
    );
    assert_eq!(task.decide(Decision::Cancel), ["cleanup"]);
    assert!(closed_as_cancelled(&task));
}

#[test]
fn the_user_can_cancel_in_every_pause() {
    // Plan approval.
    let mut planning = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        ..Script::default()
    });
    planning.run();
    assert_cancellable(planning);

    // A question for the user.
    let mut asking = Task::two().script(script(vec![implementer_asks("Which database?")], vec![]));
    asking.run();
    assert_cancellable(asking);

    // A dispute with nobody to rule but the user.
    let mut script_ = script(
        vec![implements(), implements_answering(vec![disputed(1, "No.")])],
        vec![requests_changes(vec![bug("null-deref", "cmd1")])],
    );
    script_commands(&mut script_, "cmd1", vec![fails()]);
    let mut ruling = Task::two().script(script_);
    ruling.run();
    assert_cancellable(ruling);

    // A plan-change request.
    let mut changing = Task::two().script(script(
        vec![implementer_wants_plan_change(
            "Needs more files.",
            &["src/**"],
        )],
        vec![],
    ));
    changing.run();
    assert_cancellable(changing);

    // Trouble.
    let mut troubled = Task::two().script(script(vec![lost(), lost()], vec![]));
    troubled.run();
    assert_cancellable(troubled);

    // The round cap.
    let mut script_ = script(
        vec![implements()],
        vec![requests_changes(vec![bug("null-deref", "cmd1")])],
    );
    script_commands(&mut script_, "cmd1", vec![fails()]);
    let mut capped = Task::two().round_cap(1).script(script_);
    capped.run();
    assert_cancellable(capped);

    // A usage limit that waits for the user.
    let mut limited_script = script(vec![hits_usage_limit()], vec![]);
    limited_script.scheduler = vec![Scheduler::Declines(Declined::NoResetTime)].into();
    let mut limited = Task::two().script(limited_script);
    limited.run();
    assert_cancellable(limited);

    let mut handoff = Task::two()
        .account_handoff()
        .script(script(vec![hits_usage_limit()], vec![]));
    handoff.run();
    assert_cancellable(handoff);

    // Approval.
    let mut approved_task = Task::two().script(script(vec![implements()], vec![approves()]));
    approved_task.run();
    assert_cancellable(approved_task);
}

#[test]
fn a_result_that_arrives_after_a_cancel_is_stale_and_cannot_restart_anything() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    while !matches!(task.next(), kratos_teams::next_step::Step::Run { .. }) {
        task.step();
    }
    let kratos_teams::next_step::Step::Run { run } = task.next() else {
        panic!("a run");
    };
    task.decide(Decision::Cancel);

    let late = task.view().accepts(&Fact::TurnEnded {
        message: run.message.clone(),
        ended: Box::new(implements()),
    });

    assert!(matches!(late, Err(Rejection::Stale { got, waiting: None }) if got == run.message));
    assert_eq!(
        task.next(),
        kratos_teams::next_step::Step::Wait {
            on: kratos_teams::next_step::Wait::Done
        }
    );
}

#[test]
fn a_duplicate_of_a_result_that_already_applied_is_stale() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    task.run();
    let message = runs(&task, Role::Implementer)[0].message.clone();

    let again = task.view().accepts(&Fact::TurnEnded {
        message: message.clone(),
        ended: Box::new(implements()),
    });

    assert!(matches!(again, Err(Rejection::Stale { got, .. }) if got == message));
}

#[test]
fn a_result_from_a_step_the_user_replaced_is_stale() {
    let mut task = Task::two().script(script(vec![lost(), lost(), implements()], vec![approves()]));
    task.run();
    let replaced = runs(&task, Role::Implementer)
        .last()
        .expect("the implementer ran")
        .message
        .clone();

    task.decide(Decision::Direct {
        text: "Use the session helper.".to_owned(),
    });
    let late = task.view().accepts(&Fact::TurnEnded {
        message: replaced,
        ended: Box::new(implements()),
    });

    assert!(matches!(late, Err(Rejection::Stale { .. })));
}

#[test]
fn a_cancel_drops_a_pending_role_change_the_steps_set_aside_and_the_messages_nobody_received() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        implementer: vec![implementer_asks("Which database?")].into(),
        ..Script::default()
    });
    task.run();
    task.decide_while_working(Decision::ApprovePlan { conditions: None });
    while !matches!(task.next(), kratos_teams::next_step::Step::Run { run } if run.role == Role::Planner)
    {
        task.step();
    }
    assert_eq!(
        task.view().suspended.len(),
        1,
        "the asking step is set aside"
    );
    task.decide_while_working(Decision::ReassignRoles {
        to: swap_implementer_and_reviewer(&task.view().bindings),
    });
    task.record(Fact::UserMessage {
        delivery: delivery("d1"),
        member: coder(),
        text: "Use tabs.".to_owned(),
    });

    task.decide(Decision::Cancel);

    let view = task.view();
    assert_eq!(view.pending_roles, None);
    assert!(view.suspended.is_empty());
    assert_eq!(view.directions[0].status, DirectionStatus::Dropped);
    assert!(closed_as_cancelled(&task));
}

#[test]
fn a_closed_task_accepts_no_decision_and_no_result() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    task.run();
    task.decide(Decision::Cancel);

    let view = task.view();

    assert_eq!(kratos_teams::decisions::allowed_decisions(&view), []);
    assert!(view.accepts(&Fact::Prepared).is_err());
    assert!(matches!(
        kratos_teams::decisions::check_decision(&view, &Decision::Resume),
        Err(Rejection::NotAllowed { .. })
    ));
}
