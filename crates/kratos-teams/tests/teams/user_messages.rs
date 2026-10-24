//! Messages from the user (D33). The user can message any member at any time. The engine records the
//! message with its member and a delivery id, then records what it did with it. A message never
//! replaces the step that is running.

use kratos_teams::decisions::Decision;
use kratos_teams::facts::{Disposition, Ended, Fact};
use kratos_teams::fold::Rejection;
use kratos_teams::members::Role;
use kratos_teams::next_step::Step;
use kratos_teams::view::{ApprovedBy, DirectionStatus};

use kratos_teams::prompts::Session;

use crate::replay::*;
use crate::support::*;

fn message(id: &str, member: kratos_teams::members::MemberId, text: &str) -> Fact {
    Fact::UserMessage {
        delivery: delivery(id),
        member,
        text: text.to_owned(),
    }
}

fn disposed(id: &str, how: Disposition) -> Fact {
    Fact::MessageDisposed {
        delivery: delivery(id),
        how,
    }
}

/// A two-member task stopped just before the implementer's first turn runs.
fn before_the_first_turn(implementer: Vec<Ended>, reviewer: Vec<Ended>) -> ScriptedTask {
    let mut task = Task::two().script(Script {
        implementer: implementer.into(),
        reviewer: reviewer.into(),
        ..Script::default()
    });
    task.step_until(|task| matches!(task.next(), Step::Run { .. }));
    task
}

fn statuses(task: &ScriptedTask) -> Vec<DirectionStatus> {
    task.view().directions.iter().map(|d| d.status).collect()
}

#[test]
fn a_message_to_a_running_member_leaves_the_active_step_and_its_message_alone() {
    let mut task = before_the_first_turn(vec![implements()], vec![approves()]);
    let before = task.next();

    task.record(message("d1", coder(), "Also add a logout link."));
    task.record(disposed("d1", Disposition::Steered));

    assert_eq!(
        task.next(),
        before,
        "the same step still waits for the same message"
    );
    task.run();
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_eq!(runs(&task, Role::Implementer).len(), 1);
    assert_eq!(statuses(&task), [DirectionStatus::Delivered]);
    assert_replayable(&task);
}

#[test]
fn a_step_cannot_complete_while_a_message_to_its_member_has_no_recorded_fate() {
    let mut task = before_the_first_turn(vec![implements()], vec![approves()]);
    let Step::Run { run } = task.next() else {
        panic!("the implementer is next");
    };
    task.record(message("d1", coder(), "Also add a logout link."));

    let refused = task.view().accepts(&Fact::TurnEnded {
        message: run.message.clone(),
        ended: Box::new(implements()),
    });

    assert!(matches!(refused, Err(Rejection::UndisposedMessages { .. })));
    task.record(disposed("d1", Disposition::Steered));
    assert_eq!(
        task.view().accepts(&Fact::TurnEnded {
            message: run.message,
            ended: Box::new(implements())
        }),
        Ok(())
    );
}

#[test]
fn a_message_for_another_member_does_not_hold_up_this_members_step() {
    let mut task = before_the_first_turn(vec![implements()], vec![approves()]);
    task.record(message("d1", checker(), "Be strict about naming."));
    task.record(disposed("d1", Disposition::Queued));

    task.run();

    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_eq!(runs(&task, Role::Implementer)[0].carried.len(), 0);
    assert_eq!(runs(&task, Role::Reviewer)[0].carried.len(), 1);
}

#[test]
fn a_message_that_missed_the_running_turn_rides_the_members_next_message_once() {
    let mut script = Script {
        implementer: vec![
            implements(),
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
    task.step_until(|task| matches!(task.next(), Step::Run { .. }));
    task.record(message("d1", coder(), "Use tabs."));
    task.record(disposed("d1", Disposition::Queued));

    task.run();

    let asked = runs(&task, Role::Implementer);
    assert_eq!(
        asked[0].carried.len(),
        0,
        "it arrived after the first message was due"
    );
    assert_eq!(asked[1].carried.len(), 1);
    assert_eq!(asked[1].carried[0].text, "Use tabs.");
    assert_eq!(asked[2].carried.len(), 0, "it rode one message and no more");
    assert_eq!(statuses(&task), [DirectionStatus::Delivered]);
    assert_replayable(&task);
}

#[test]
fn a_lost_turn_does_not_count_as_received_so_the_message_goes_again() {
    let mut task = before_the_first_turn(vec![lost(), implements()], vec![approves()]);
    task.record(message("d1", coder(), "Use tabs."));
    task.record(disposed("d1", Disposition::Steered));

    task.run();

    let asked = runs(&task, Role::Implementer);
    assert_eq!(asked[0].carried.len(), 0);
    assert_eq!(
        asked[1].carried.len(),
        1,
        "the retry carries what the lost turn may not have read"
    );
    assert_eq!(statuses(&task), [DirectionStatus::Delivered]);
    assert_replayable(&task);
}

#[test]
fn a_message_is_recorded_for_a_real_member_once() {
    let mut task = before_the_first_turn(vec![implements()], vec![approves()]);
    task.record(message("d1", coder(), "Use tabs."));

    let stranger = task.view().accepts(&message(
        "d2",
        kratos_teams::members::MemberId::new("nobody"),
        "Hello",
    ));
    let again = task.view().accepts(&message("d1", coder(), "Use tabs."));
    let unknown = task.view().accepts(&disposed("d9", Disposition::Queued));

    assert!(matches!(stranger, Err(Rejection::UnknownMember(_))));
    assert!(matches!(again, Err(Rejection::DuplicateDelivery(_))));
    assert!(matches!(unknown, Err(Rejection::UnknownDelivery(_))));
}

#[test]
fn a_message_is_disposed_of_only_once() {
    let mut task = before_the_first_turn(vec![implements()], vec![approves()]);
    task.record(message("d1", coder(), "Use tabs."));
    task.record(disposed("d1", Disposition::Queued));

    let twice = task.view().accepts(&disposed("d1", Disposition::Steered));

    assert!(matches!(twice, Err(Rejection::Misfit { .. })));
}

#[test]
fn a_cancel_drops_the_messages_nobody_received() {
    let mut task = before_the_first_turn(vec![implements()], vec![approves()]);
    task.record(message("d1", coder(), "Use tabs."));
    task.record(disposed("d1", Disposition::Queued));
    task.record(message("d2", checker(), "Be strict."));

    task.decide(Decision::Cancel);

    assert_eq!(
        statuses(&task),
        [DirectionStatus::Dropped, DirectionStatus::Dropped]
    );
    let after = task.view().accepts(&message("d3", coder(), "Too late."));
    assert!(matches!(after, Err(Rejection::Misfit { .. })));
}

#[test]
fn a_message_to_the_planner_is_recorded_with_the_role_it_held() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        ..Script::default()
    });
    task.run();

    task.record(message("d1", planner_id(), "Prefer small files."));
    task.record(disposed("d1", Disposition::Queued));

    let view = task.view();
    assert_eq!(view.directions[0].role, Role::Planner);
    assert_eq!(view.directions[0].member, planner_id());
}

#[test]
fn a_queued_message_rides_a_correction_too_or_it_would_be_marked_delivered_unread() {
    let mut task = Task::two().script(Script {
        implementer: vec![
            implementer_invalid("summary: must not be blank"),
            implements(),
        ]
        .into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.step_until(|task| matches!(task.next(), Step::Run { .. }));
    // The message arrives while the first turn runs and missed it, then the turn ends with a bad report.
    task.record(message("d1", coder(), "Use tabs."));
    task.record(disposed("d1", Disposition::Queued));
    task.step();
    let Step::Run { run } = task.next() else {
        panic!("the correction is next");
    };
    assert!(run.note.is_some());

    let text = text_of(&task, &run, Session::Continuing);

    assert!(text.starts_with("Kratos could not accept your last report"));
    assert!(text.contains("Use tabs."));
}
