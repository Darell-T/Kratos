//! The recorded fact logs that `cargo run -p kratos-teams --example replay -- <log>` replays. Each one
//! shows one rule at work, and the verification lanes of the migration plan read them. A log is JSON
//! lines, one `StoredFact` per line, in `tests/fixtures/lanes/`.
//!
//! These tests keep the files honest in two ways. A file must equal what its scenario records now, so a
//! change to the rules that changes a log shows up in the diff. And the file itself, read back and
//! folded, must satisfy what its lane asks for. Set `KRATOS_TEAMS_RECORD_LANES=1` to write the files
//! again after a deliberate change.

use std::fs;
use std::path::PathBuf;

use kratos_teams::decisions::{Decision, PlanChangeResponse};
use kratos_teams::evidence::ProbeCapture;
use kratos_teams::facts::{Disposition, Ended, Fact, HandoffOutcome, ProbePolicy, StoredFact};
use kratos_teams::fold::fold;
use kratos_teams::members::Role;
use kratos_teams::next_step::{Step, next_step};
use kratos_teams::prompts::{Platform, PromptContext, Session, message};
use kratos_teams::reports::{AddedProbe, ImplementerDone, Reported};
use kratos_teams::reports::{Report, RulingVerdict};
use kratos_teams::task_plan::ProbeConstraints;
use kratos_teams::view::{
    Activity, ApprovedBy, FindingStatus, FindingsPause, Phase, ProbeStatus, TaskView, Trouble,
    Waiting, Work,
};

use crate::replay::*;
use crate::support::*;

fn folder() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lanes")
}

fn jsonl(log: &[StoredFact]) -> String {
    log.iter()
        .map(|fact| serde_json::to_string(fact).expect("a fact serializes") + "\n")
        .collect()
}

/// The facts of a lane file, read back.
fn read_lane(name: &str) -> Vec<StoredFact> {
    let path = folder().join(format!("{name}.jsonl"));
    let text =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    text.lines()
        .map(|line| serde_json::from_str(line).expect("a lane line is a stored fact"))
        .collect()
}

fn view_of(log: &[StoredFact]) -> TaskView {
    fold(log).expect("a lane log folds")
}

/// Every prefix of the log, folded.
fn prefixes(log: &[StoredFact]) -> Vec<TaskView> {
    (1..=log.len()).map(|n| view_of(&log[..n])).collect()
}

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

fn text_for(view: &TaskView) -> String {
    let Step::Run { run } = next_step(view) else {
        panic!("a run is next");
    };
    let context = PromptContext {
        session: Session::Fresh,
        diff_stat: "",
        platform: Platform::Unix,
    };
    message(view, &run, &context)
}

// ---------------------------------------------------------------------------------------------
// The scenarios.
// ---------------------------------------------------------------------------------------------

fn invalid_report() -> ScriptedTask {
    let mut task = Task::two().script(script(
        vec![
            implementer_invalid("summary: must not be blank"),
            implementer_invalid("Bad JSON."),
        ],
        vec![],
    ));
    task.run();
    task
}

fn stuck() -> ScriptedTask {
    let mut script_ = script(
        vec![implements(), fix_f1(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            still_open(1, "A null session still reaches it."),
            still_open(1, "Still reachable through b()."),
        ],
    );
    script_commands(&mut script_, "cmd1", vec![fails(), fails(), fails()]);
    let mut task = Task::two().script(script_);
    task.run();
    task
}

fn cap() -> ScriptedTask {
    let mut script_ = script(
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
    script_commands(&mut script_, "cmd1", vec![fails(), passes()]);
    script_commands(&mut script_, "cmd2", vec![fails(), passes()]);
    script_commands(&mut script_, "cmd3", vec![fails()]);
    let mut task = Task::two().script(script_);
    task.run();
    task
}

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

fn question() -> ScriptedTask {
    let mut task = approved_team(
        vec![
            planner_answers("Postgres."),
            planner_asks_user("Is the legacy API still supported?"),
            planner_answers("No, drop it."),
            planner_answers("The helper is meant to be reused."),
        ],
        vec![
            implementer_asks("Which database?"),
            implementer_asks("Keep the legacy API?"),
            implements(),
        ],
        vec![reviewer_asks("Why reuse the session helper?"), approves()],
    );
    task.decide(Decision::Answer {
        text: "It is not.".to_owned(),
    });
    task
}

fn question_no_planner() -> ScriptedTask {
    let mut task = Task::two().script(script(
        vec![implementer_asks("Which database?"), implements()],
        vec![reviewer_asks("Why reuse the session helper?"), approves()],
    ));
    task.run();
    task.decide(Decision::Answer {
        text: "Postgres.".to_owned(),
    });
    task.decide(Decision::Answer {
        text: "Because the task says so.".to_owned(),
    });
    task
}

fn dispute_script(planner: Vec<Ended>) -> Script {
    let mut plans = vec![plan_ready()];
    plans.extend(planner);
    let mut script_ = Script {
        planner: plans.into(),
        implementer: vec![
            implements(),
            implements_answering(vec![disputed(1, "The session is never null."), fixed(2)]),
            implements_answering(vec![fixed(1), fixed(2)]),
        ]
        .into(),
        reviewer: vec![
            requests_changes(vec![bug("null-deref", "cmd1"), bug("race", "cmd2")]),
            approves(),
            approves(),
        ]
        .into(),
        ..Script::default()
    };
    script_commands(&mut script_, "cmd1", vec![fails(), passes()]);
    script_commands(&mut script_, "cmd2", vec![fails(), passes()]);
    script_
}

fn dispute() -> ScriptedTask {
    let mut task = Task::three().script(dispute_script(vec![planner_rules(&[(
        1,
        RulingVerdict::Bug,
    )])]));
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    task
}

fn dispute_no_planner() -> ScriptedTask {
    let mut script_ = dispute_script(vec![]);
    script_.planner.clear();
    let mut task = Task::two().script(script_);
    task.run();
    task.decide(Decision::Rule { not_bug: vec![] });
    task
}

fn plan_change() -> ScriptedTask {
    let mut task = Task::two().plan_first().script(script(
        vec![
            plan_ready(),
            implementer_wants_plan_change("The form needs the shared session.", &["src/shared/**"]),
            implements(),
        ],
        vec![approves()],
    ));
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    task.decide(Decision::PlanChange {
        response: PlanChangeResponse::Allow,
        instructions: Some("Touch only session.rs.".to_owned()),
    });
    task
}

fn scope() -> ScriptedTask {
    let mut task = Task::three().script(Script {
        planner: vec![plan_with("src/login/**")].into(),
        implementer: vec![implements()].into(),
        reviewer: vec![approves()].into(),
        files: vec!["src/login/form.rs".to_owned(), "Cargo.lock".to_owned()],
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    task
}

fn limit() -> ScriptedTask {
    let mut script_ = script(vec![hits_usage_limit(), implements()], vec![approves()]);
    script_.handoffs = vec![HandoffOutcome::Unavailable {
        reason: "no other account has room".to_owned(),
    }]
    .into();
    let mut task = Task::two().account_handoff().script(script_);
    task.run();
    task.decide(Decision::ApproveHandoff);
    task
}

fn planning_off() -> ScriptedTask {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    task.run();
    task
}

fn planning_on() -> ScriptedTask {
    let mut task = Task::two()
        .plan_first()
        .script(script(vec![plan_ready(), implements()], vec![approves()]));
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    task.decide(Decision::Finish {
        outcome: kratos_teams::decisions::Finish::Merged,
    });
    task
}

fn steering() -> ScriptedTask {
    let mut script_ = script(
        vec![implements(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script_, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script_);
    task.step_until(|task| matches!(task.next(), Step::Run { .. }));
    task.record(Fact::UserMessage {
        delivery: delivery("d1"),
        member: coder(),
        text: "Use tabs.".to_owned(),
    });
    task.record(Fact::MessageDisposed {
        delivery: delivery("d1"),
        how: Disposition::Steered,
    });
    task.record(Fact::UserMessage {
        delivery: delivery("d2"),
        member: coder(),
        text: "Also add a logout link.".to_owned(),
    });
    task.record(Fact::MessageDisposed {
        delivery: delivery("d2"),
        how: Disposition::Queued,
    });
    task.run();
    task
}

fn cancel() -> ScriptedTask {
    let mut task = Task::two().script(script(vec![implements()], vec![approves()]));
    task.step_until(|task| matches!(task.next(), Step::Run { run } if run.role == Role::Reviewer));
    task.decide(Decision::Cancel);
    task
}

fn roles() -> ScriptedTask {
    let mut script_ = script(
        vec![implements(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script_, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script_);
    task.step_until(|task| matches!(task.next(), Step::Run { run } if run.role == Role::Reviewer));
    task.decide_while_working(Decision::ReassignRoles {
        to: swapped_members(),
    });
    task.run();
    task
}

fn probes() -> ScriptedTask {
    let probe = "probes/null_session.rs";
    let mut script_ = script(
        vec![
            implements(),
            implements_with(
                ImplementerDone {
                    responses: vec![fixed(1)],
                    added_probes: vec![AddedProbe {
                        from: probe.to_owned(),
                        to: "tests/null_session.rs".to_owned(),
                    }],
                    ..done()
                },
                checked(),
            ),
        ],
        vec![
            requests_changes(vec![with_probes(bug("null-deref", "cmd1"), &[probe])]),
            approves(),
        ],
    );
    script_commands(&mut script_, "cmd1", vec![fails(), passes()]);
    script_.probes = vec![ProbeCapture {
        path: probe.to_owned(),
        content: Some("#[test] fn null_session() {}".to_owned()),
        truncated: false,
    }];
    let policy = ProbePolicy::Delegated {
        constraints: ProbeConstraints {
            test_paths: vec![pattern("tests/**")],
            note: "Keep only fast tests.".to_owned(),
        },
    };
    let mut task = Task::two().probes(policy).script(script_);
    task.run();
    task
}

type Scenario = fn() -> ScriptedTask;

const LANES: [(&str, Scenario); 16] = [
    ("r6-invalid-report", invalid_report),
    ("r6-stuck", stuck),
    ("r6-cap", cap),
    ("r6-question", question),
    ("r6-question-no-planner", question_no_planner),
    ("r6-dispute", dispute),
    ("r6-dispute-no-planner", dispute_no_planner),
    ("r6-plan-change", plan_change),
    ("r6-scope", scope),
    ("r6-limit", limit),
    ("r6-prefixes-planning-off", planning_off),
    ("r6-prefixes-planning-on", planning_on),
    ("r6-prefixes-steering", steering),
    ("r6-prefixes-cancel", cancel),
    ("r6-prefixes-roles", roles),
    ("r6-prefixes-probes", probes),
];

#[test]
fn every_lane_file_equals_what_its_scenario_records_now() {
    let writing = std::env::var_os("KRATOS_TEAMS_RECORD_LANES").is_some();
    if writing {
        fs::create_dir_all(folder()).expect("the lanes folder");
    }
    for (name, scenario) in LANES {
        let recorded = jsonl(scenario().log());
        let path = folder().join(format!("{name}.jsonl"));
        if writing {
            fs::write(&path, &recorded).expect("a lane file");
        }
        let on_file = fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            on_file.replace("\r\n", "\n"),
            recorded,
            "{name}: run `KRATOS_TEAMS_RECORD_LANES=1 cargo test -p kratos-teams lane` to write it again"
        );
    }
}

#[test]
fn every_lane_file_replays_with_each_next_action_valid_and_no_finished_step_repeated() {
    for (name, _) in LANES {
        assert_log_replayable(&read_lane(name));
    }
}

// ---------------------------------------------------------------------------------------------
// What each lane asks for, checked against the file.
// ---------------------------------------------------------------------------------------------

#[test]
fn lane_2_a_report_that_is_invalid_twice_pauses_the_task_for_the_user_after_the_second() {
    let log = read_lane("r6-invalid-report");

    let view = view_of(&log);

    assert_eq!(
        trouble(&view).map(|(why, role, _)| (why, role)),
        Some((Trouble::InvalidReport, Role::Implementer))
    );
    let reports = log
        .iter()
        .filter(|f| matches!(f.fact, Fact::TurnEnded { .. }))
        .count();
    assert_eq!(reports, 2);
    assert_eq!(
        next_step(&view),
        Step::Wait {
            on: kratos_teams::next_step::Wait::User
        }
    );
}

#[test]
fn lane_3_a_finding_open_after_two_fix_attempts_pauses_as_stuck_with_the_reviewers_diagnosis() {
    let view = view_of(&read_lane("r6-stuck"));

    assert!(matches!(
        waiting(&view),
        Some(Waiting::Findings { why: FindingsPause::Stuck, ids }) if *ids == [fid(1)]
    ));
    assert!(matches!(
        &view.findings[0].status,
        FindingStatus::Open { diagnosis: Some(text), .. } if text == "Still reachable through b()."
    ));
}

#[test]
fn lane_4_three_rounds_that_end_with_blocking_findings_pause_at_the_cap() {
    let view = view_of(&read_lane("r6-cap"));

    assert_eq!(view.rounds.len(), 3);
    assert!(matches!(
        waiting(&view),
        Some(Waiting::Findings {
            why: FindingsPause::Cap,
            ..
        })
    ));
}

/// For each question in the log: who it went to, and whether the asking step went on with its own id.
fn question_routes(log: &[StoredFact]) -> Vec<(Role, &'static str, bool)> {
    let mut routes = Vec::new();
    for (index, stored) in log.iter().enumerate() {
        let Fact::TurnEnded { ended, .. } = &stored.fact else {
            continue;
        };
        let Ended::Reported { reported } = &**ended else {
            continue;
        };
        if !is_question(reported) {
            continue;
        }
        let before = view_of(&log[..index]);
        let Phase::Active {
            activity: Activity::Turn { turn },
        } = &before.phase
        else {
            panic!("a question ends a turn");
        };
        let after = view_of(&log[..=index]);
        let to = match &after.phase {
            Phase::Waiting {
                waiting: Waiting::Answer,
            } => "user",
            Phase::Active {
                activity: Activity::Turn { turn },
            } if matches!(turn.work, Work::Answer { .. }) => "planner",
            other => panic!("a question went to {other:?}"),
        };
        let resumed = log[index + 1..].iter().enumerate().any(|(offset, _)| {
            let later = view_of(&log[..index + 2 + offset]);
            matches!(&later.phase, Phase::Active { activity: Activity::Turn { turn: again } }
                if again.step == turn.step && again.message != turn.message)
        });
        routes.push((turn.work.role(), to, resumed));
    }
    routes
}

fn is_question(reported: &Reported) -> bool {
    matches!(
        reported,
        Reported::Implementer {
            report: Report::Question { .. },
            ..
        } | Reported::Review {
            report: Report::Question { .. }
        } | Reported::Answer {
            report: Report::Question { .. }
        }
    )
}

#[test]
fn lane_5_each_question_reaches_the_right_recipient_and_its_answer_resumes_only_the_suspended_step()
{
    let with_planner = question_routes(&read_lane("r6-question"));
    let without = question_routes(&read_lane("r6-question-no-planner"));

    assert_eq!(
        with_planner,
        [
            (Role::Implementer, "planner", true),
            (Role::Implementer, "planner", true),
            (Role::Planner, "user", true),
            (Role::Reviewer, "planner", true),
        ]
    );
    assert_eq!(
        without,
        [
            (Role::Implementer, "user", true),
            (Role::Reviewer, "user", true)
        ]
    );
    assert_eq!(
        view_of(&read_lane("r6-question")).phase,
        approved(1, ApprovedBy::Reviewer)
    );
}

#[test]
fn lane_6_the_planners_ruling_decides_a_dispute_and_with_no_planner_the_users_does() {
    let planner_log = read_lane("r6-dispute");
    let user_log = read_lane("r6-dispute-no-planner");

    assert!(
        prefixes(&planner_log)
            .iter()
            .all(|view| !matches!(waiting(view), Some(Waiting::Ruling { .. }))),
        "the user was never asked to rule"
    );
    assert!(prefixes(&planner_log).iter().any(|view| matches!(
        &view.phase,
        Phase::Active { activity: Activity::Turn { turn } } if matches!(turn.work, Work::Rule { .. })
    )));
    assert!(
        prefixes(&user_log)
            .iter()
            .any(|view| matches!(waiting(view), Some(Waiting::Ruling { .. })))
    );
    let ruled_by_planner = view_of(&planner_log);
    let ruled_by_user = view_of(&user_log);
    assert_eq!(
        ruled_by_planner.rulings[0].by,
        kratos_teams::view::Answerer::Planner
    );
    assert_eq!(
        ruled_by_user.rulings[0].by,
        kratos_teams::view::Answerer::User
    );
}

#[test]
fn lane_7_a_plan_change_pauses_for_the_user_and_the_implementers_next_brief_carries_the_instructions()
 {
    let log = read_lane("r6-plan-change");
    let views = prefixes(&log);

    assert!(
        views
            .iter()
            .any(|view| waiting(view) == Some(&Waiting::PlanChange))
    );
    let decided = log
        .iter()
        .position(|f| {
            matches!(
                f.fact,
                Fact::UserDecided {
                    decision: Decision::PlanChange { .. }
                }
            )
        })
        .expect("the user decided");
    let text = text_for(&views[decided]);
    assert!(text.contains("The user adds: Touch only session.rs."));
}

#[test]
fn lane_8_the_next_review_package_lists_a_file_outside_the_plans_scope() {
    let log = read_lane("r6-scope");
    let views = prefixes(&log);

    let at_review = views
        .iter()
        .find(|view| matches!(next_step(view), Step::Run { run } if run.role == Role::Reviewer))
        .expect("a review is next");

    let text = text_for(at_review);
    assert!(text.contains("# Changed files outside the agreed scope"));
    assert!(text.contains("- Cargo.lock"));
    assert!(!text.contains("- src/login/form.rs"));
}

#[test]
fn lane_9_a_usage_limit_completes_one_logical_step_once_with_a_distinct_continuation_message() {
    let log = read_lane("r6-limit");
    let views = prefixes(&log);

    let mut sent: Vec<_> = views
        .iter()
        .filter_map(|view| match next_step(view) {
            Step::Run { run } if run.role == Role::Implementer => Some(run.message),
            _ => None,
        })
        .collect();
    sent.dedup();
    assert_eq!(
        sent.len(),
        1,
        "the implementer's message was asked for once"
    );
    let scheduled: Vec<_> = log
        .iter()
        .filter_map(|f| match &f.fact {
            Fact::ContinuationScheduled { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(scheduled.len(), 1);
    assert_ne!(scheduled[0], sent[0]);
    assert!(
        views.iter().all(
            |view| !matches!(next_step(view), Step::Run { run } if run.message == scheduled[0])
        ),
        "the rules never ask for the continuation to be sent"
    );
    let end = view_of(&log);
    assert_eq!(end.rounds.len(), 1);
    assert_eq!(end.phase, approved(1, ApprovedBy::Reviewer));
}

#[test]
fn lane_10_the_prefix_logs_cover_planning_steering_cancellation_roles_and_probes() {
    let off = view_of(&read_lane("r6-prefixes-planning-off"));
    let on = prefixes(&read_lane("r6-prefixes-planning-on"));
    let steering = view_of(&read_lane("r6-prefixes-steering"));
    let cancelled = view_of(&read_lane("r6-prefixes-cancel"));
    let roles = view_of(&read_lane("r6-prefixes-roles"));
    let probes = view_of(&read_lane("r6-prefixes-probes"));

    assert_eq!(off.plan, kratos_teams::view::PlanState::NoPlan);
    assert!(
        on.iter()
            .any(|view| waiting(view) == Some(&Waiting::PlanApproval))
    );
    assert_eq!(
        steering
            .directions
            .iter()
            .map(|d| d.status)
            .collect::<Vec<_>>(),
        [kratos_teams::view::DirectionStatus::Delivered; 2]
    );
    assert_eq!(
        cancelled.phase,
        Phase::Closed {
            outcome: kratos_teams::facts::Outcome::Cancelled
        }
    );
    assert_eq!(roles.bindings, swapped_members());
    assert!(matches!(
        probes.probes[0].status,
        ProbeStatus::Retained { .. }
    ));
}

#[test]
fn steering_keeps_the_active_execution_so_each_implementer_message_is_asked_for_once() {
    let log = read_lane("r6-prefixes-steering");

    let mut sent = Vec::new();
    for view in prefixes(&log) {
        if let Step::Run { run } = next_step(&view)
            && run.role == Role::Implementer
            && !sent.contains(&run.message)
        {
            sent.push(run.message);
        }
    }

    assert_eq!(
        sent.len(),
        2,
        "the first turn, and the fix after the review"
    );
}
