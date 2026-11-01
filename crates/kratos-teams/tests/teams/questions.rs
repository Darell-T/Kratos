//! Questions (D31). Any member can ask a question instead of finishing. An implementer's or a
//! reviewer's question goes to the planner, or to the user when there is no planner. A planner's own
//! question goes to the user, because a planner never guesses (D91). The answer goes back to the member
//! who asked, and the same step goes on.

use kratos_teams::decisions::{Decision, DecisionKind};
use kratos_teams::facts::{Ended, Fact};
use kratos_teams::fold::Rejection;
use kratos_teams::members::Role;
use kratos_teams::view::{Answerer, ApprovedBy, Resolution, Trouble, Waiting, Work};

use crate::replay::*;
use crate::support::*;

/// A three-member task whose plan the user approved, ready for the implementer.
fn approved_team(
    planner: Vec<Ended>,
    implementer: Vec<Ended>,
    reviewer: Vec<Ended>,
) -> ScriptedTask {
    let mut plans = vec![plan_ready()];
    plans.extend(planner);
    let mut task = Task::three().script(Script {
        planner: plans.into(),
        implementer: implementer.into(),
        reviewer: reviewer.into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    task
}

#[test]
fn without_a_planner_an_implementer_question_goes_to_the_user_and_the_answer_resumes_the_same_step()
{
    let mut task = Task::two().script(Script {
        implementer: vec![implementer_asks("Which database?"), implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });

    task.run();

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::Answer));
    let question = view.pending_question().expect("the question waits");
    assert_eq!(question.asker, coder());
    assert_eq!(question.role, Role::Implementer);
    assert_eq!(question.summary, "Half done.");
    assert_eq!(question.text, "Which database?");
    assert_eq!(
        decisions(&view),
        [DecisionKind::Answer, DecisionKind::Cancel]
    );

    task.decide(Decision::Answer {
        text: "Postgres.".to_owned(),
    });

    let asked = runs(&task, Role::Implementer);
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[1].step, asked[0].step, "the same step goes on");
    assert_ne!(
        asked[1].message, asked[0].message,
        "each message is sent once"
    );
    assert_eq!(asked[1].follows.as_ref(), Some(&asked[0].message));
    assert_eq!(
        asked[1].resolutions,
        [Resolution::Answered {
            question: "Which database?".to_owned(),
            answer: "Postgres.".to_owned(),
            by: Answerer::User
        }]
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn with_a_planner_an_implementer_question_goes_to_the_planner_and_the_user_hears_nothing() {
    let mut task = approved_team(
        vec![planner_answers("Use Postgres.")],
        vec![implementer_asks("Which database?"), implements()],
        vec![approves()],
    );

    task.run();

    let planner_runs = runs(&task, Role::Planner);
    let answering = planner_runs.last().expect("the planner answers");
    assert!(matches!(answering.work, Work::Answer { .. }));
    assert!(answering.plan_mode, "the planner reads and never writes");
    assert_eq!(answering.member, planner_id());
    let asked = runs(&task, Role::Implementer);
    assert_eq!(asked[1].step, asked[0].step);
    assert_eq!(
        asked[1].resolutions,
        [Resolution::Answered {
            question: "Which database?".to_owned(),
            answer: "Use Postgres.".to_owned(),
            by: Answerer::Planner
        }]
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_reviewer_question_goes_to_the_planner_and_the_review_resumes_in_the_same_session() {
    let mut task = approved_team(
        vec![planner_answers("The helper is meant to be reused.")],
        vec![implements()],
        vec![reviewer_asks("Why reuse the session helper?"), approves()],
    );

    task.run();

    let reviews = runs(&task, Role::Reviewer);
    assert_eq!(reviews.len(), 2);
    assert_eq!(reviews[1].step, reviews[0].step);
    assert_eq!(reviews[1].follows.as_ref(), Some(&reviews[0].message));
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_reviewer_question_goes_to_the_user_when_there_is_no_planner() {
    let mut task = Task::two().script(Script {
        implementer: vec![implements()].into(),
        reviewer: vec![reviewer_asks("Why reuse the helper?"), approves()].into(),
        ..Script::default()
    });

    task.run();

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::Answer));
    assert_eq!(
        view.pending_question().map(|q| q.role),
        Some(Role::Reviewer)
    );
    task.decide(Decision::Answer {
        text: "Because the task says so.".to_owned(),
    });
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
}

#[test]
fn a_planners_own_question_goes_to_the_user_and_the_answer_resumes_the_planners_step_then_the_asker()
 {
    let mut task = approved_team(
        vec![
            planner_asks_user("Is the legacy API still supported?"),
            planner_answers("No, drop it."),
        ],
        vec![implementer_asks("Keep the legacy API?"), implements()],
        vec![approves()],
    );

    task.run();

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::Answer));
    let question = view.pending_question().expect("the planner's question");
    assert_eq!(question.role, Role::Planner);
    assert_eq!(question.text, "Is the legacy API still supported?");

    task.decide(Decision::Answer {
        text: "It is not.".to_owned(),
    });

    let planner = runs(&task, Role::Planner);
    let (asked, resumed) = (planner[1], planner[2]);
    assert_eq!(resumed.step, asked.step, "the planner's step went on");
    assert_eq!(
        resumed.resolutions,
        [Resolution::Answered {
            question: "Is the legacy API still supported?".to_owned(),
            answer: "It is not.".to_owned(),
            by: Answerer::User
        }]
    );
    let implementer = runs(&task, Role::Implementer);
    assert_eq!(
        implementer[1].resolutions,
        [Resolution::Answered {
            question: "Keep the legacy API?".to_owned(),
            answer: "No, drop it.".to_owned(),
            by: Answerer::Planner
        }]
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn the_plan_author_who_asks_gets_its_answer_from_the_user_and_the_plan_step_goes_on() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_question("Which folders may change?"), plan_ready()].into(),
        ..Script::default()
    });

    task.run();
    assert_eq!(waiting(&task.view()), Some(&Waiting::Answer));
    task.decide(Decision::Answer {
        text: "Only src/login.".to_owned(),
    });

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::PlanApproval));
    let planning = runs(&task, Role::Planner);
    assert_eq!(planning[1].step, planning[0].step);
}

#[test]
fn a_question_is_not_a_failure_and_does_not_use_up_the_one_correction() {
    let mut task = Task::two().script(Script {
        implementer: vec![
            implementer_asks("Which database?"),
            implementer_invalid("summary: must not be blank"),
            implements(),
        ]
        .into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();

    task.decide(Decision::Answer {
        text: "Postgres.".to_owned(),
    });

    assert_eq!(
        notes(&runs(&task, Role::Implementer)),
        [None, None, Some("invalid_report".to_owned())]
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
}

#[test]
fn after_three_questions_in_one_step_the_next_one_goes_to_the_user_and_not_the_planner() {
    let mut task = approved_team(
        vec![
            planner_answers("One."),
            planner_answers("Two."),
            planner_answers("Three."),
        ],
        vec![
            implementer_asks("First?"),
            implementer_asks("Second?"),
            implementer_asks("Third?"),
            implementer_asks("Fourth?"),
            implements(),
        ],
        vec![approves()],
    );

    task.run();

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::Answer));
    assert_eq!(
        view.pending_question().map(|q| q.text.as_str()),
        Some("Fourth?")
    );
    assert_eq!(
        runs(&task, Role::Planner).len(),
        4,
        "the plan and three answers"
    );
}

#[test]
fn a_planner_that_cannot_answer_lets_the_user_answer_the_asker_directly() {
    let mut task = approved_team(
        vec![
            planner_answer_invalid("answer: must not be blank"),
            planner_answer_invalid("answer: must not be blank"),
        ],
        vec![implementer_asks("Which database?"), implements()],
        vec![approves()],
    );

    task.run();

    let view = task.view();
    assert_eq!(
        trouble(&view).map(|(why, role, _)| (why, role)),
        Some((Trouble::InvalidReport, Role::Planner))
    );
    assert_eq!(
        decisions(&view),
        [
            DecisionKind::Resume,
            DecisionKind::Answer,
            DecisionKind::Cancel
        ]
    );

    task.decide(Decision::Answer {
        text: "Postgres.".to_owned(),
    });

    let implementer = runs(&task, Role::Implementer);
    assert_eq!(
        implementer[1].resolutions,
        [Resolution::Answered {
            question: "Which database?".to_owned(),
            answer: "Postgres.".to_owned(),
            by: Answerer::User
        }]
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_question_asked_with_an_unchecked_edit_is_not_a_handoff_so_nothing_is_returned() {
    let mut task = Task::two().script(Script {
        implementer: vec![
            kratos_teams::facts::Ended::Reported {
                reported: kratos_teams::reports::Reported::Implementer {
                    report: kratos_teams::reports::Report::Question {
                        summary: "Half done.".to_owned(),
                        question: "Which database?".to_owned(),
                    },
                    changes: edited_after_check(),
                },
            },
            implements(),
        ]
        .into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });

    task.run();
    task.decide(Decision::Answer {
        text: "Postgres.".to_owned(),
    });

    assert_eq!(notes(&runs(&task, Role::Implementer)), [None, None]);
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
}

#[test]
fn a_late_result_for_a_step_that_a_question_suspended_is_stale() {
    let mut task = Task::two().script(Script {
        implementer: vec![implementer_asks("Which database?")].into(),
        ..Script::default()
    });
    task.run();
    let first = runs(&task, Role::Implementer)[0].message.clone();

    let refused = task.view().accepts(&Fact::TurnEnded {
        message: first.clone(),
        ended: Box::new(implements()),
    });

    assert_eq!(
        refused,
        Err(Rejection::Stale {
            got: first,
            waiting: None
        })
    );
}

#[test]
fn an_answer_cannot_be_blank() {
    let mut task = Task::two().script(Script {
        implementer: vec![implementer_asks("Which database?")].into(),
        ..Script::default()
    });
    task.run();

    let refused = kratos_teams::decisions::check_decision(
        &task.view(),
        &Decision::Answer {
            text: "  ".to_owned(),
        },
    );

    assert!(matches!(refused, Err(Rejection::InvalidDecision { .. })));
}

#[test]
fn an_answer_is_refused_when_no_question_waits() {
    let mut task = Task::two().script(Script {
        implementer: vec![implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();

    let refused = kratos_teams::decisions::check_decision(
        &task.view(),
        &Decision::Answer {
            text: "Postgres.".to_owned(),
        },
    );

    assert!(matches!(refused, Err(Rejection::NotAllowed { .. })));
}
