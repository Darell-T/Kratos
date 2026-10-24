//! Plan-change requests (D32). A member asks to change the agreed plan, most often to touch more files.
//! The request always pauses for the user, even with a planner. The user allows it, declines it, or
//! asks for a new plan, and can add instructions to any of the three. A plan change is only an explicit
//! outcome a member returns. The rules never detect a contradiction on their own.

use kratos_teams::decisions::{Decision, DecisionKind, PlanChangeResponse};
use kratos_teams::facts::{Ended, Fact, StoredFact};
use kratos_teams::fold::fold;
use kratos_teams::members::Role;
use kratos_teams::next_step::{Step, next_step};
use kratos_teams::prompts::{Platform, PromptContext, Session, message};
use kratos_teams::view::{ApprovedBy, Brief, PlanState, Resolution, Waiting, Work};

use crate::replay::*;
use crate::support::*;

fn script(implementer: Vec<Ended>, reviewer: Vec<Ended>) -> Script {
    Script {
        implementer: implementer.into(),
        reviewer: reviewer.into(),
        ..Script::default()
    }
}

/// The implementer's messages that do the work, without the one that wrote the plan.
fn building(task: &ScriptedTask) -> Vec<&kratos_teams::view::Run> {
    runs(task, Role::Implementer)
        .into_iter()
        .filter(|run| matches!(run.work, Work::Implement { .. }))
        .collect()
}

/// A two-member task with planning on, past plan approval, with the scope `src/login/**`.
fn planned_task(implementer: Vec<Ended>, reviewer: Vec<Ended>) -> ScriptedTask {
    let mut turns = vec![plan_ready()];
    turns.extend(implementer);
    let mut task = Task::two().plan_first().script(script(turns, reviewer));
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    task
}

#[test]
fn a_plan_change_request_pauses_for_the_user_even_when_a_planner_exists() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        implementer: vec![implementer_wants_plan_change(
            "The form needs src/shared/session.rs.",
            &["src/shared/**"],
        )]
        .into(),
        ..Script::default()
    });
    task.run();

    task.decide(Decision::ApprovePlan { conditions: None });

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::PlanChange));
    let request = view.pending_plan_change().expect("the request waits");
    assert_eq!(request.reason, "The form needs src/shared/session.rs.");
    assert_eq!(request.extra_scope[0].as_str(), "src/shared/**");
    assert_eq!(
        decisions(&view),
        [DecisionKind::PlanChange, DecisionKind::Cancel]
    );
    assert_eq!(
        runs(&task, Role::Planner).len(),
        1,
        "the planner was not asked"
    );
}

#[test]
fn allowing_a_plan_change_widens_the_scope_and_the_step_goes_on_carrying_the_users_instructions() {
    let mut task = planned_task(
        vec![
            implementer_wants_plan_change("The form needs the shared session.", &["src/shared/**"]),
            implements(),
        ],
        vec![approves()],
    );
    assert_eq!(waiting(&task.view()), Some(&Waiting::PlanChange));

    task.decide(Decision::PlanChange {
        response: PlanChangeResponse::Allow,
        instructions: Some("Touch only session.rs.".to_owned()),
    });

    let view = task.view();
    assert_eq!(view.amendments.len(), 1);
    assert_eq!(view.amendments[0].extra_scope[0].as_str(), "src/shared/**");
    let asked = building(&task);
    assert_eq!(asked[1].step, asked[0].step, "the same step goes on");
    assert_eq!(
        asked[1].resolutions,
        [Resolution::PlanChanged {
            reason: "The form needs the shared session.".to_owned(),
            allowed: true,
            instructions: Some("Touch only session.rs.".to_owned())
        }]
    );
    assert_eq!(view.phase, approved(1, ApprovedBy::Reviewer));
    assert!(view.quote_sources().contains(&"Touch only session.rs."));
    let reviewing = runs(&task, Role::Reviewer)[0];
    assert!(text_of(&task, reviewing, Session::Fresh).contains("Touch only session.rs."));
    assert_replayable(&task);
}

#[test]
fn an_allowed_change_makes_the_new_files_part_of_the_scope() {
    let mut task = Task::two().plan_first().script(Script {
        implementer: vec![
            plan_ready(),
            implementer_wants_plan_change("Needs the shared session.", &["src/shared/**"]),
            implements(),
        ]
        .into(),
        reviewer: vec![approves()].into(),
        files: vec![
            "src/login/form.rs".to_owned(),
            "src/shared/session.rs".to_owned(),
        ],
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    task.decide(Decision::PlanChange {
        response: PlanChangeResponse::Allow,
        instructions: None,
    });

    assert_eq!(task.view().outside_scope(), Vec::<String>::new());
}

#[test]
fn the_recorded_plan_change_conditions_reach_every_later_prompt() {
    let log: Vec<StoredFact> = include_str!("../fixtures/lanes/r6-plan-change.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a saved fact"))
        .collect();
    let approved = log
        .iter()
        .position(|stored| {
            matches!(
                stored.fact,
                Fact::UserDecided {
                    decision: Decision::PlanChange {
                        response: PlanChangeResponse::Allow,
                        ..
                    }
                }
            )
        })
        .expect("an amendment was approved");
    let mut roles = Vec::new();
    for end in approved + 1..=log.len() {
        let view = fold(&log[..end]).expect("the saved prefix folds");
        if let Step::Run { run } = next_step(&view) {
            let text = message(
                &view,
                &run,
                &PromptContext {
                    session: Session::Fresh,
                    diff_stat: "",
                    platform: Platform::Windows,
                },
            );
            assert!(
                text.contains("Touch only session.rs."),
                "{} lost the amendment conditions",
                run.role
            );
            roles.push(run.role);
        }
    }
    assert!(roles.contains(&Role::Implementer));
    assert!(roles.contains(&Role::Reviewer));
}

#[test]
fn amendment_conditions_survive_replanning_and_reach_the_new_plan_author() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready(), plan_with("src/**")].into(),
        implementer: vec![
            implementer_wants_plan_change("Needs the shared session.", &["src/shared/**"]),
            implementer_wants_plan_change("The approach needs a new plan.", &[]),
            implements(),
        ]
        .into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    task.decide(Decision::PlanChange {
        response: PlanChangeResponse::Allow,
        instructions: Some("Touch only session.rs.".to_owned()),
    });
    task.decide(Decision::PlanChange {
        response: PlanChangeResponse::Replan,
        instructions: None,
    });

    let planning = runs(&task, Role::Planner)[1];
    assert!(text_of(&task, planning, Session::Fresh).contains("Touch only session.rs."));
    assert!(
        task.view()
            .quote_sources()
            .contains(&"Touch only session.rs.")
    );
    task.decide(Decision::ApprovePlan { conditions: None });
    let reviewing = runs(&task, Role::Reviewer)[0];
    assert!(text_of(&task, reviewing, Session::Fresh).contains("Touch only session.rs."));
    assert_replayable(&task);
}

#[test]
fn declining_a_plan_change_keeps_the_plan_and_tells_the_implementer_to_keep_to_it() {
    let mut task = planned_task(
        vec![
            implementer_wants_plan_change("The form needs the shared session.", &["src/shared/**"]),
            implements(),
        ],
        vec![approves()],
    );

    task.decide(Decision::PlanChange {
        response: PlanChangeResponse::Decline,
        instructions: None,
    });

    let view = task.view();
    assert!(view.amendments.is_empty());
    assert_eq!(
        building(&task)[1].resolutions,
        [Resolution::PlanChanged {
            reason: "The form needs the shared session.".to_owned(),
            allowed: false,
            instructions: None
        }]
    );
    assert_eq!(view.phase, approved(1, ApprovedBy::Reviewer));
}

#[test]
fn asking_for_a_new_plan_drops_the_step_and_the_author_writes_a_plan_the_user_must_approve_again() {
    let mut task = planned_task(
        vec![
            implementer_wants_plan_change("The whole approach is wrong.", &[]),
            plan_with("src/**"),
            implements(),
        ],
        vec![approves()],
    );

    task.decide(Decision::PlanChange {
        response: PlanChangeResponse::Replan,
        instructions: Some("Start from the session helper.".to_owned()),
    });

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::PlanApproval));
    assert!(
        view.suspended.is_empty(),
        "the dropped step does not come back"
    );
    assert!(matches!(view.plan, PlanState::Drafted { revision: 2, .. }));
    let planning: Vec<_> = runs(&task, Role::Implementer)
        .into_iter()
        .filter(|run| matches!(run.work, Work::Plan { .. }))
        .collect();
    assert_eq!(
        planning[1].work,
        Work::Plan {
            author: Role::Implementer,
            feedback: Some(
                "The whole approach is wrong.\n\nStart from the session helper.".to_owned()
            )
        }
    );
    assert!(planning[1].plan_mode);

    task.decide(Decision::ApprovePlan { conditions: None });

    let asked = building(&task);
    assert_eq!(
        asked[1].work,
        Work::Implement {
            brief: Brief::Planned
        }
    );
    assert_ne!(
        asked[1].step, asked[0].step,
        "the old step was replaced, not resumed"
    );
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_planners_answer_that_would_contradict_the_plan_asks_the_user_to_change_it() {
    let mut task = Task::three().script(Script {
        planner: vec![
            plan_ready(),
            planner_wants_plan_change("The plan forbids the legacy API."),
        ]
        .into(),
        implementer: vec![implementer_asks("Keep the legacy API?")].into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });

    let view = task.view();

    assert_eq!(waiting(&view), Some(&Waiting::PlanChange));
    assert_eq!(
        view.pending_plan_change().map(|r| r.reason.as_str()),
        Some("The plan forbids the legacy API.")
    );
    assert_eq!(
        view.suspended.len(),
        2,
        "the planner's step and the asker's wait together"
    );
}

#[test]
fn with_no_plan_an_allowed_change_widens_the_scope_the_user_gave() {
    let mut task = Task::two().scope(&["src/login/**"]).script(Script {
        implementer: vec![
            implementer_wants_plan_change("Needs src/shared.", &["src/shared/**"]),
            implements(),
        ]
        .into(),
        reviewer: vec![approves()].into(),
        files: vec!["src/shared/session.rs".to_owned()],
        ..Script::default()
    });
    task.run();

    task.decide(Decision::PlanChange {
        response: PlanChangeResponse::Allow,
        instructions: Some("Touch only session.rs.".to_owned()),
    });

    assert_eq!(task.view().outside_scope(), Vec::<String>::new());
    let reviewing = runs(&task, Role::Reviewer)[0];
    assert!(text_of(&task, reviewing, Session::Fresh).contains("Touch only session.rs."));
    assert!(
        task.view()
            .quote_sources()
            .contains(&"Touch only session.rs.")
    );
}
