//! Planning, and the planner. A two-member Team plans only when the user turns it on, and then the
//! implementer writes the plan. A three-member Team always plans first, and the planner writes the plan.
//! The user approves any plan before an implementer starts (D28, D37).

use kratos_teams::decisions::{Decision, DecisionKind};
use kratos_teams::facts::Ended;
use kratos_teams::members::Role;
use kratos_teams::view::{ApprovedBy, Brief, PlanState, Trouble, Waiting, Work};

use crate::replay::*;
use crate::support::*;

fn script(implementer: Vec<Ended>, reviewer: Vec<Ended>) -> Script {
    Script {
        implementer: implementer.into(),
        reviewer: reviewer.into(),
        ..Script::default()
    }
}

#[test]
fn a_two_member_team_with_planning_off_starts_from_the_request_and_waits_for_no_plan() {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));

    assert_eq!(task.run(), ["prepare", "run", "commit", "run", "check"]);

    let view = task.view();
    assert_eq!(view.plan, PlanState::NoPlan);
    assert_eq!(view.phase, approved(1, ApprovedBy::Reviewer));
    let first = &runs(&task, Role::Implementer)[0];
    assert_eq!(
        first.work,
        Work::Implement {
            brief: Brief::Start
        }
    );
    assert!(!first.plan_mode);
    assert_eq!(runs(&task, Role::Planner).len(), 0);
}

#[test]
fn with_planning_on_the_implementer_writes_the_plan_read_only_and_the_user_approves_it_first() {
    let mut task = Task::two().plan_first().script(Script {
        implementer: vec![plan_ready(), implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });

    task.run();

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::PlanApproval));
    let planning = &runs(&task, Role::Implementer)[0];
    assert_eq!(
        planning.work,
        Work::Plan {
            author: Role::Implementer,
            feedback: None
        }
    );
    assert!(
        planning.plan_mode,
        "the plan is written in the provider's read-only mode"
    );
    assert_eq!(
        decisions(&view),
        [
            DecisionKind::ApprovePlan,
            DecisionKind::RevisePlan,
            DecisionKind::Cancel
        ]
    );

    task.decide(Decision::ApprovePlan { conditions: None });

    let asked = runs(&task, Role::Implementer);
    assert_eq!(
        asked[1].work,
        Work::Implement {
            brief: Brief::Planned
        }
    );
    assert!(!asked[1].plan_mode);
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn a_three_member_team_always_plans_first_and_the_planner_writes_the_plan() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        implementer: vec![implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });

    task.run();

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::PlanApproval));
    assert_eq!(
        runs(&task, Role::Implementer).len(),
        0,
        "nothing builds before the user approves"
    );
    let planning = &runs(&task, Role::Planner)[0];
    assert_eq!(planning.member, planner_id());
    assert!(planning.plan_mode);
    assert!(matches!(
        view.plan,
        PlanState::Drafted {
            author: Role::Planner,
            revision: 1,
            ..
        }
    ));
    task.decide(Decision::ApprovePlan { conditions: None });
    assert_eq!(task.view().phase, approved(1, ApprovedBy::Reviewer));
    assert_replayable(&task);
}

#[test]
fn the_plan_first_setting_does_not_apply_to_a_team_with_a_planner() {
    // The setting is false here and the planner still plans, because a planner means planning first.
    let task = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        ..Script::default()
    });
    assert!(!task.view().policy.plan_first);

    let mut task = task;
    task.run();

    assert_eq!(runs(&task, Role::Planner).len(), 1);
}

#[test]
fn the_planner_is_woken_only_to_plan_to_answer_and_to_rule_so_a_clean_task_runs_it_once() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        implementer: vec![implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });

    assert_eq!(runs(&task, Role::Planner).len(), 1);
}

#[test]
fn a_plan_the_user_sends_back_with_notes_is_written_again_by_its_author_and_needs_approval_again() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready(), plan_with("src/**")].into(),
        implementer: vec![implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();

    task.decide(Decision::RevisePlan {
        feedback: "Cover every folder under src.".to_owned(),
    });

    let view = task.view();
    assert_eq!(waiting(&view), Some(&Waiting::PlanApproval));
    assert!(matches!(view.plan, PlanState::Drafted { revision: 2, .. }));
    let revising = &runs(&task, Role::Planner)[1];
    assert_eq!(
        revising.work,
        Work::Plan {
            author: Role::Planner,
            feedback: Some("Cover every folder under src.".to_owned())
        }
    );
    assert!(
        revising.plan_mode,
        "the user explicitly sent the plan back, so planning is on again"
    );
    assert_eq!(runs(&task, Role::Implementer).len(), 0);
    assert_replayable(&task);
}

#[test]
fn the_users_conditions_ride_with_the_approval_and_count_as_requirement_evidence() {
    let mut task = Task::two().plan_first().script(Script {
        implementer: vec![plan_ready(), implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();

    task.decide(Decision::ApprovePlan {
        conditions: Some("Keep the existing route names.".to_owned()),
    });

    let view = task.view();
    assert!(matches!(
        &view.plan,
        PlanState::Approved { conditions: Some(text), .. } if text == "Keep the existing route names."
    ));
    assert!(
        view.quote_sources()
            .contains(&"Keep the existing route names.")
    );
}

#[test]
fn a_plan_missing_a_part_goes_back_once_and_then_pauses_with_only_resume_and_cancel() {
    let mut task = Task::three().script(Script {
        planner: vec![
            plan_invalid("done_means: must hold at least one checkable statement"),
            plan_invalid("done_means: must hold at least one checkable statement"),
        ]
        .into(),
        ..Script::default()
    });

    task.run();

    let view = task.view();
    assert_eq!(
        trouble(&view).map(|(why, role, _)| (why, role)),
        Some((Trouble::InvalidReport, Role::Planner))
    );
    assert_eq!(
        decisions(&view),
        [DecisionKind::Resume, DecisionKind::Cancel]
    );
    assert_eq!(
        notes(&runs(&task, Role::Planner)),
        [None, Some("invalid_report".to_owned())]
    );
}

#[test]
fn a_task_that_planned_never_goes_back_to_planning_after_a_restart_at_any_later_fact() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        implementer: vec![implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    let approved_at = task
        .log()
        .iter()
        .position(|stored| matches!(stored.fact, kratos_teams::facts::Fact::UserDecided { .. }))
        .expect("the user approved");

    for n in approved_at + 1..=task.log().len() {
        let restarted = ScriptedTask::restart_from(&task.log()[..n], Script::default());
        if let kratos_teams::next_step::Step::Run { run } = restarted.next() {
            assert!(!run.plan_mode, "after fact {n} the task would plan again");
            assert_ne!(run.role, Role::Planner);
        }
    }
}

#[test]
fn a_task_with_planning_off_never_enters_planning_on_its_own_even_after_a_question_and_a_pause() {
    let mut task = Task::two().script(Script {
        implementer: vec![implementer_asks("Which database?"), implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::Answer {
        text: "Postgres.".to_owned(),
    });

    assert!(
        runs(&task, Role::Implementer)
            .iter()
            .all(|run| !run.plan_mode)
    );
    assert_eq!(task.view().plan, PlanState::NoPlan);
}

#[test]
fn with_no_plan_the_scope_is_the_one_the_user_gave_and_a_missing_scope_covers_the_worktree() {
    let mut open = Task::two().script(Script::default());
    let mut scoped = Task::two()
        .scope(&["src/login/**"])
        .script(Script::default());
    for task in [&mut open, &mut scoped] {
        task.script.implementer = vec![implements()].into();
        task.script.reviewer = vec![approves()].into();
        task.run();
    }

    assert_eq!(open.view().outside_scope(), Vec::<String>::new());
    assert_eq!(scoped.view().outside_scope(), ["src/a.rs"]);
}

#[test]
fn the_approved_plans_scope_decides_which_changed_files_are_listed_as_outside_it() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_with("src/login/**")].into(),
        implementer: vec![implements()].into(),
        reviewer: vec![approves()].into(),
        files: vec!["src/login/form.rs".to_owned(), "Cargo.lock".to_owned()],
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });

    let view = task.view();
    assert_eq!(view.outside_scope(), ["Cargo.lock"]);
    assert_eq!(
        view.phase,
        approved(1, ApprovedBy::Reviewer),
        "a file outside the scope does not reject the work"
    );
}
