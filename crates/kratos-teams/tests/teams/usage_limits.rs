//! Usage limits (D39 to D41). When a provider stops a turn at a limit, the step keeps its identity and
//! goes to the one usage scheduler. The rules never resend the stopped message and never run a timer.
//! A long or unknown wait asks the user, and moving a role to another account needs the user's approval
//! every time.

use kratos_teams::decisions::{Decision, DecisionKind};
use kratos_teams::facts::{Declined, Ended, Fact, HandoffOutcome, MessageId};
use kratos_teams::fold::Rejection;
use kratos_teams::members::Role;
use kratos_teams::next_step::{Step, Wait};
use kratos_teams::view::{ApprovedBy, Hold, UsageLimitAsk, Waiting};

use crate::replay::*;
use crate::support::*;

fn limited_implementer(then: Vec<Ended>) -> Script {
    let mut implementer = vec![hits_usage_limit()];
    implementer.extend(then);
    Script {
        implementer: implementer.into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    }
}

fn run_message(step: &Step) -> Option<&MessageId> {
    match step {
        Step::Run { run } => Some(&run.message),
        _ => None,
    }
}

#[test]
fn a_limit_hands_the_same_step_to_the_scheduler_and_one_continuation_finishes_it_once() {
    let mut task = Task::two().script(limited_implementer(vec![implements()]));

    let ran = task.run();

    assert_eq!(
        ran,
        [
            "prepare",
            "run",
            "hand_to_scheduler",
            "wait",
            "commit",
            "run",
            "check"
        ]
    );
    let view = task.view();
    assert_eq!(view.phase, approved(1, ApprovedBy::Reviewer));
    assert_eq!(view.rounds.len(), 1, "the logical step completed once");
    let stopped = runs(&task, Role::Implementer)[0].message.clone();
    let handed = task
        .steps
        .iter()
        .find_map(|step| match step {
            Step::HandToScheduler { step, stop } => Some((*step, stop.clone())),
            _ => None,
        })
        .expect("the step went to the scheduler");
    assert_eq!(handed.0, runs(&task, Role::Implementer)[0].step);
    assert_eq!(handed.1.message, stopped);
    assert!(handed.1.delivered);
    assert!(!handed.1.user_accepted_wait);
    assert_replayable(&task);
}

#[test]
fn the_rules_never_send_the_stopped_message_again_and_start_no_timer_of_their_own() {
    let mut task = Task::two().script(limited_implementer(vec![implements()]));
    task.run();

    let stopped = runs(&task, Role::Implementer)[0].message.clone();
    let sent: Vec<&MessageId> = task.steps.iter().filter_map(run_message).collect();

    assert_eq!(
        sent.iter().filter(|message| ***message == stopped).count(),
        1
    );
    assert_eq!(
        task.steps
            .iter()
            .filter(|step| matches!(step, Step::Run { run } if run.role == Role::Implementer))
            .count(),
        1,
        "the implementer's step was sent once, and the continuation was the scheduler's"
    );
}

#[test]
fn after_a_restart_the_stopped_step_is_handed_to_the_scheduler_again_without_a_second_run() {
    let mut task = Task::two().script(limited_implementer(vec![implements()]));
    task.step_until(|task| matches!(task.next(), Step::HandToScheduler { .. }));

    let restarted = ScriptedTask::restart_from(task.log(), Script::default());

    assert_eq!(restarted.next(), task.next());
    task.step();
    assert!(matches!(
        task.next(),
        Step::Wait {
            on: Wait::Engine { .. }
        }
    ));
}

#[test]
fn a_late_result_from_the_stopped_turn_is_stale_and_so_is_a_duplicate_of_the_continuations() {
    let mut task = Task::two().script(limited_implementer(vec![implements()]));
    task.step_until(|task| matches!(task.next(), Step::Wait { .. }));
    let view = task.view();
    let stopped = runs(&task, Role::Implementer)[0].message.clone();
    let continuation = view
        .phase
        .waiting_message()
        .cloned()
        .expect("the continuation's message");
    assert_ne!(
        continuation, stopped,
        "the continuation has a message id of its own"
    );

    let late = view.accepts(&Fact::TurnEnded {
        message: stopped.clone(),
        ended: Box::new(implements()),
    });
    assert_eq!(
        late,
        Err(Rejection::Stale {
            got: stopped,
            waiting: Some(continuation.clone())
        })
    );

    task.step();
    let twice = task.view().accepts(&Fact::TurnEnded {
        message: continuation.clone(),
        ended: Box::new(implements()),
    });
    assert!(matches!(twice, Err(Rejection::Stale { .. })));
}

#[test]
fn a_second_limit_on_the_continuation_goes_back_to_the_scheduler_with_its_own_message() {
    let mut task = Task::two().script(limited_implementer(vec![hits_usage_limit(), implements()]));

    task.run();

    let handed: Vec<_> = task
        .steps
        .iter()
        .filter_map(|step| match step {
            Step::HandToScheduler { stop, .. } => Some(stop.message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(handed.len(), 2);
    assert_ne!(handed[0], handed[1]);
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_wait_the_scheduler_will_not_resume_asks_the_user_who_can_keep_waiting_or_cancel() {
    let mut script = limited_implementer(vec![implements()]);
    script.scheduler = vec![Scheduler::Declines(Declined::TooLong), Scheduler::Schedules].into();
    let mut task = Task::two().script(script);

    task.run();

    let view = task.view();
    assert!(matches!(
        waiting(&view),
        Some(Waiting::UsageLimit {
            ask: UsageLimitAsk::LongWait {
                why: Declined::TooLong
            },
            ..
        })
    ));
    assert_eq!(
        decisions(&view),
        [DecisionKind::KeepWaiting, DecisionKind::Cancel]
    );

    task.decide(Decision::KeepWaiting);

    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    let second = task
        .steps
        .iter()
        .filter_map(|step| match step {
            Step::HandToScheduler { stop, .. } => Some(stop.user_accepted_wait),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        second,
        [false, true],
        "the scheduler is told the user chose to wait"
    );
    assert_replayable(&task);
}

#[test]
fn account_handoff_is_off_by_default_so_a_limit_never_offers_it() {
    let mut task = Task::two().script(limited_implementer(vec![implements()]));

    task.run();

    assert!(
        task.steps
            .iter()
            .all(|step| !matches!(step, Step::HandOffAccount { .. }))
    );
}

#[test]
fn with_handoff_allowed_a_limit_asks_the_user_and_an_approval_tries_another_account_once() {
    let mut script = limited_implementer(vec![implements()]);
    script.handoffs = vec![HandoffOutcome::HandedOff].into();
    let mut task = Task::two().account_handoff().script(script);

    task.run();

    let view = task.view();
    assert!(matches!(
        waiting(&view),
        Some(Waiting::UsageLimit {
            ask: UsageLimitAsk::Handoff,
            ..
        })
    ));
    assert_eq!(
        decisions(&view),
        [
            DecisionKind::ApproveHandoff,
            DecisionKind::DeclineHandoff,
            DecisionKind::Cancel
        ]
    );
    assert!(
        task.steps.iter().all(|step| !matches!(
            step,
            Step::HandOffAccount { .. } | Step::HandToScheduler { .. }
        )),
        "nothing moves until the user decides"
    );

    task.decide(Decision::ApproveHandoff);

    assert!(
        task.steps
            .iter()
            .any(|step| matches!(step, Step::HandOffAccount { member, .. } if *member == coder()))
    );
    assert!(
        task.steps
            .iter()
            .all(|step| !matches!(step, Step::HandToScheduler { .. })),
        "a handoff that worked needs no scheduler"
    );
    let asked = runs(&task, Role::Implementer);
    assert_eq!(asked.len(), 2, "the step is sent again after the handoff");
    assert_eq!(asked[1].step, asked[0].step);
    assert_ne!(asked[1].message, asked[0].message);
    assert_eq!(
        asked[1].follows, None,
        "the other account starts a fresh session"
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn declining_the_handoff_waits_for_the_reset_instead() {
    let mut task = Task::two()
        .account_handoff()
        .script(limited_implementer(vec![implements()]));
    task.run();

    task.decide(Decision::DeclineHandoff);

    assert!(
        task.steps
            .iter()
            .all(|step| !matches!(step, Step::HandOffAccount { .. }))
    );
    assert!(
        task.steps
            .iter()
            .any(|step| matches!(step, Step::HandToScheduler { .. }))
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
}

#[test]
fn a_handoff_nobody_could_take_goes_on_to_the_scheduler() {
    let mut script = limited_implementer(vec![implements()]);
    script.handoffs = vec![HandoffOutcome::Unavailable {
        reason: "no other account has room".to_owned(),
    }]
    .into();
    let mut task = Task::two().account_handoff().script(script);
    task.run();

    task.decide(Decision::ApproveHandoff);

    assert!(
        task.steps
            .iter()
            .any(|step| matches!(step, Step::HandToScheduler { .. }))
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
}

#[test]
fn each_handoff_needs_the_users_approval_so_a_second_limit_asks_again() {
    let mut script = limited_implementer(vec![hits_usage_limit(), implements()]);
    script.handoffs = vec![HandoffOutcome::HandedOff, HandoffOutcome::HandedOff].into();
    let mut task = Task::two().account_handoff().script(script);
    task.run();
    task.decide(Decision::ApproveHandoff);

    assert!(matches!(
        waiting(&task.view()),
        Some(Waiting::UsageLimit {
            ask: UsageLimitAsk::Handoff,
            ..
        })
    ));
    task.decide(Decision::ApproveHandoff);
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
}

#[test]
fn the_stopped_turn_is_held_with_the_scheduler_until_it_answers() {
    let mut task = Task::two().script(limited_implementer(vec![implements()]));
    task.step_until(|task| matches!(task.next(), Step::HandToScheduler { .. }));

    let view = task.view();

    assert!(matches!(
        &view.phase,
        kratos_teams::view::Phase::Active {
            activity: kratos_teams::view::Activity::Turn { turn }
        } if matches!(turn.hold, Some(Hold::ToScheduler { .. }))
    ));
}

#[test]
fn a_continuation_that_reuses_the_stopped_message_id_is_refused() {
    let mut task = Task::two().script(limited_implementer(vec![implements()]));
    task.step_until(|task| matches!(task.next(), Step::HandToScheduler { .. }));
    let Step::HandToScheduler { step, stop } = task.next() else {
        panic!("the step goes to the scheduler");
    };

    let refused = task.view().accepts(&Fact::ContinuationScheduled {
        step,
        continuation: kratos_teams::facts::ContinuationId::new("c1"),
        message: stop.message,
        resumes_at: None,
    });

    assert!(matches!(refused, Err(Rejection::InvalidFact(_))));
}

#[test]
fn a_decline_is_refused_for_a_step_the_scheduler_has_not_been_handed() {
    let mut task = Task::two().script(limited_implementer(vec![implements()]));
    task.step_until(|task| matches!(task.next(), Step::Run { .. }));
    let Step::Run { run } = task.next() else {
        panic!("the implementer is about to run");
    };
    let decline = Fact::ContinuationDeclined {
        step: run.step,
        why: Declined::TooLong,
    };

    let while_running = task.view().accepts(&decline);
    task.step_until(|task| {
        matches!(
            task.next(),
            Step::Wait {
                on: Wait::Engine { .. }
            }
        )
    });
    let once_scheduled = task.view().accepts(&decline);

    for refused in [while_running, once_scheduled] {
        assert!(matches!(
            refused,
            Err(Rejection::Misfit {
                fact: "continuation_declined",
                ..
            })
        ));
    }
}
