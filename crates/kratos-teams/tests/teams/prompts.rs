//! What each member is told. Kratos writes every message from the record, so Kratos alone decides what
//! a member sees. The reviewer never sees the implementer's conversation, the implementer sees only the
//! findings Kratos confirmed, and a planner never guesses.

use kratos_protocol::Provider;
use kratos_teams::decisions::Decision;
use kratos_teams::facts::{Disposition, Ended, Fact};
use kratos_teams::members::Role;
use kratos_teams::prompts::{STEP_BACK_TEXT, Session, branch_name, commit_subject, task_title};
use kratos_teams::reports::RulingVerdict;
use kratos_teams::security_triggers::TriggerHit;
use kratos_teams::view::{Brief, Work};

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

fn second_round() -> ScriptedTask {
    let mut script = script(
        vec![
            implements(),
            implements_with(
                kratos_teams::reports::ImplementerDone {
                    summary: "Patched it quietly.".to_owned(),
                    responses: vec![fixed(1)],
                    ..done()
                },
                checked(),
            ),
        ],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1"), nit("naming")]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script);
    task.run();
    task
}

#[test]
fn the_reviewer_gets_the_task_the_diff_the_rules_and_the_open_findings_and_never_the_implementers_summary()
 {
    let task = second_round();
    let review = runs(&task, Role::Reviewer)[1].clone();

    let text = text_of(&task, &review, Session::Fresh);

    assert!(text.contains("Use the existing session helper."));
    assert!(text.contains(&format!("git diff {} {}", sha(1), sha(2))));
    assert!(text.contains("src/a.rs | 2 +-"));
    assert!(text.contains("Check the goal first."));
    assert!(text.contains("The implementer's response: fixed."));
    assert!(text.contains("F2 naming at src/a.rs:3: listed for the user as a nit"));
    assert!(text.contains("Do not report these again."));
    assert!(text.contains("one PowerShell command that exits non-zero"));
    assert!(text.contains("a quote of at least 12 characters"));
    assert!(text.contains("never its conversation"));
    assert!(!text.contains("Patched it quietly."));
}

#[test]
fn the_evidence_command_is_written_for_the_shell_of_the_platform() {
    let task = second_round();
    let review = runs(&task, Role::Reviewer)[0].clone();
    let view = view_when_sent(&task, &review);
    let context = |platform| kratos_teams::prompts::PromptContext {
        session: Session::Fresh,
        diff_stat: "",
        platform,
    };

    let unix = kratos_teams::prompts::message(
        &view,
        &review,
        &context(kratos_teams::prompts::Platform::Unix),
    );
    let windows = kratos_teams::prompts::message(
        &view,
        &review,
        &context(kratos_teams::prompts::Platform::Windows),
    );

    assert!(unix.contains("one sh command"));
    assert!(windows.contains("one PowerShell command"));
}

#[test]
fn the_implementer_gets_only_the_confirmed_blocking_findings() {
    let mut script = script(
        vec![implements(), fix_f1()],
        vec![
            requests_changes(vec![
                bug("null-deref", "cmd1"),
                nit("naming"),
                bug("flaky", "cmd2"),
                design("layering"),
            ]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    script_commands(&mut script, "cmd2", vec![passes()]);
    let mut task = Task::two().script(script);
    task.run();
    let fixing = runs(&task, Role::Implementer)[1].clone();

    let text = text_of(&task, &fixing, Session::Continuing);

    assert!(text.contains("## F1: null-deref (bug)"));
    assert!(!text.contains("naming"));
    assert!(!text.contains("flaky"));
    assert!(!text.contains("layering"));
}

#[test]
fn a_reviewer_whose_verdict_disagrees_is_told_which_findings_kratos_confirmed() {
    let mut script = script(
        vec![implements(), fix_f1()],
        vec![
            approves_listing(vec![bug("null-deref", "cmd1")]),
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), fails(), passes()]);
    let mut task = Task::two().script(script);
    task.run();
    let correcting = runs(&task, Role::Reviewer)[1].clone();

    let text = text_of(&task, &correcting, Session::Continuing);

    assert!(text.contains("Your verdict is approve, but Kratos confirmed findings that block"));
    assert!(text.contains("F1 null-deref at src/a.rs:3: confirmed, and it blocks"));
    assert!(
        !text.contains("# Task"),
        "a correction carries only the problem"
    );
}

#[test]
fn a_correction_in_a_continuing_session_carries_only_the_problem_and_a_new_session_gets_everything()
{
    let mut task = Task::two().script(script(vec![lost(), implements()], vec![approves()]));
    task.run();
    let retry = runs(&task, Role::Implementer)[1].clone();

    let continuing = text_of(&task, &retry, Session::Continuing);
    let fresh = text_of(&task, &retry, Session::Fresh);

    assert!(continuing.starts_with("Your previous turn did not finish (process exited)."));
    assert!(!continuing.contains("# Task"));
    assert!(fresh.contains("Your previous turn did not finish"));
    assert!(fresh.contains("Use the existing session helper."));
    assert!(fresh.contains(AGENTS_RULE));
}

#[test]
fn a_replacement_session_is_told_it_is_taking_over_with_the_task_the_rules_and_the_findings() {
    let mut script = script(
        vec![implements(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script);
    task.run();
    let fixing = runs(&task, Role::Implementer)[1].clone();

    let text = text_of(&task, &fixing, Session::Fresh);

    assert!(text.contains("taking over this task"));
    assert!(text.contains("Use the existing session helper."));
    assert!(text.contains(AGENTS_RULE));
    assert!(text.contains("## F1: null-deref (bug)"));
}

#[test]
fn the_reviewer_sees_everything_the_user_said_including_what_was_sent_to_the_implementer() {
    let mut task = Task::two()
        .constraints(&["Keep the existing route names."])
        .script(script(vec![implements()], vec![approves()]));
    task.step_until(|task| matches!(task.next(), kratos_teams::next_step::Step::Run { .. }));
    task.record(Fact::UserMessage {
        delivery: delivery("d1"),
        member: coder(),
        text: "Use tabs.".to_owned(),
    });
    task.record(Fact::MessageDisposed {
        delivery: delivery("d1"),
        how: Disposition::Steered,
    });
    task.run();
    let review = runs(&task, Role::Reviewer)[0].clone();

    let text = text_of(&task, &review, Session::Fresh);

    assert!(text.contains("Keep the existing route names."));
    assert!(text.contains("To the implementer: Use tabs."));
}

#[test]
fn the_reviewer_is_told_the_approved_plan_its_scope_and_the_changed_files_outside_it() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_with("src/login/**")].into(),
        implementer: vec![implements()].into(),
        reviewer: vec![approves()].into(),
        files: vec!["src/login/form.rs".to_owned(), "Cargo.lock".to_owned()],
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan {
        conditions: Some("Keep the route names.".to_owned()),
    });
    let review = runs(&task, Role::Reviewer)[0].clone();

    let text = text_of(&task, &review, Session::Fresh);

    assert!(text.contains("# The approved plan"));
    assert!(text.contains("A login page behind the existing session helper."));
    assert!(text.contains("A signed-in user lands on the dashboard."));
    assert!(text.contains("- src/login/**"));
    assert!(text.contains("Keep the route names."));
    assert!(text.contains("# Changed files outside the agreed scope"));
    assert!(text.contains("- Cargo.lock"));
    assert!(!text.contains("- src/login/form.rs\n"));
}

#[test]
fn the_reviewer_is_told_to_ask_for_the_implementers_reasons_and_not_to_guess_them() {
    let task = second_round();
    let review = runs(&task, Role::Reviewer)[0].clone();

    let text = text_of(&task, &review, Session::Fresh);

    assert!(text.contains("If you need the implementer's reasons, ask a question."));
    assert!(text.contains("status: done when you finished the review"));
}

#[test]
fn the_planner_is_told_to_write_a_plan_in_a_fixed_format_without_changing_files_and_never_to_guess()
{
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        ..Script::default()
    });
    task.run();
    let planning = runs(&task, Role::Planner)[0].clone();

    let text = text_of(&task, &planning, Session::Fresh);

    assert!(text.contains("You are the planner"));
    assert!(text.contains("never write code"));
    assert!(text.contains("build: what to build"));
    assert!(text.contains("done_means"));
    assert!(text.contains("files_in_scope"));
    assert!(text.contains("Never guess."));
    assert!(text.contains("Use the existing session helper."));
}

#[test]
fn a_plan_sent_back_shows_the_author_the_old_plan_and_what_the_user_wants_changed() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready(), plan_with("src/**")].into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::RevisePlan {
        feedback: "Cover every folder under src.".to_owned(),
    });
    let revising = runs(&task, Role::Planner)[1].clone();

    let text = text_of(&task, &revising, Session::Continuing);

    assert!(text.contains("A login page behind the existing session helper."));
    assert!(text.contains("Cover every folder under src."));
}

#[test]
fn the_implementer_who_writes_the_plan_is_told_to_change_no_file_until_the_user_approves() {
    let mut task = Task::two().plan_first().script(Script {
        implementer: vec![plan_ready()].into(),
        ..Script::default()
    });
    task.run();
    let planning = runs(&task, Role::Implementer)[0].clone();

    let text = text_of(&task, &planning, Session::Fresh);

    assert!(text.contains("write the plan. Do not change any file."));
    assert!(text.contains("approves it before any code is written"));
}

#[test]
fn a_planner_asked_by_the_reviewer_sees_what_the_implementer_reported_and_not_its_conversation() {
    let mut task = Task::three().script(Script {
        planner: vec![
            plan_ready(),
            planner_answers("The helper is meant to be reused."),
        ]
        .into(),
        implementer: vec![implements()].into(),
        reviewer: vec![reviewer_asks("Why reuse the session helper?"), approves()].into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    let answering = runs(&task, Role::Planner)[1].clone();

    let text = text_of(&task, &answering, Session::Fresh);

    assert!(text.contains("# What the implementer reported"));
    assert!(text.contains("Round 1: Added the login page."));
    assert!(text.contains("Why reuse the session helper?"));
    assert!(text.contains("The reviewer had done this so far: Read the diff."));
    assert!(text.contains("Never guess."));
}

#[test]
fn a_planner_asked_to_rule_sees_each_disputed_finding_with_the_implementers_reason() {
    let mut script = Script {
        planner: vec![plan_ready(), planner_rules(&[(1, RulingVerdict::NotBug)])].into(),
        implementer: vec![
            implements(),
            implements_answering(vec![disputed(1, "The session is never null.")]),
        ]
        .into(),
        reviewer: vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ]
        .into(),
        ..Script::default()
    };
    script_commands(&mut script, "cmd1", vec![fails()]);
    let mut task = Task::three().script(script);
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    let ruling = runs(&task, Role::Planner)[1].clone();
    assert!(matches!(ruling.work, Work::Rule { .. }));

    let text = text_of(&task, &ruling, Session::Fresh);

    assert!(text.contains("## F1: null-deref (bug)"));
    assert!(text.contains("The implementer disputes it: The session is never null."));
    assert!(text.contains("bug if the finding is real"));
    assert!(text.contains("not_bug"));
}

#[test]
fn an_answer_and_a_plan_decision_ride_in_the_steps_next_message() {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready(), planner_answers("Use Postgres.")].into(),
        implementer: vec![implementer_asks("Which database?"), implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan { conditions: None });
    let resumed = runs(&task, Role::Implementer)[1].clone();

    let text = text_of(&task, &resumed, Session::Continuing);

    assert!(text.contains("You asked: Which database?"));
    assert!(text.contains("Answer from the planner: Use Postgres."));
}

#[test]
fn a_plan_change_decision_and_the_users_instructions_ride_in_the_next_message() {
    let mut task = Task::two().script(Script {
        implementer: vec![
            implementer_wants_plan_change("Needs the shared session.", &["src/shared/**"]),
            implements(),
        ]
        .into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::PlanChange {
        response: kratos_teams::decisions::PlanChangeResponse::Allow,
        instructions: Some("Touch only session.rs.".to_owned()),
    });
    let resumed = runs(&task, Role::Implementer)[1].clone();

    let text = text_of(&task, &resumed, Session::Continuing);

    assert!(
        text.contains(
            "The user allowed the plan change you asked for (Needs the shared session.)."
        )
    );
    assert!(text.contains("The user adds: Touch only session.rs."));
}

#[test]
fn a_message_the_user_queued_rides_in_the_members_next_message() {
    let mut script = script(
        vec![implements(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    let mut task = Task::two().script(script);
    task.step_until(|task| matches!(task.next(), kratos_teams::next_step::Step::Run { .. }));
    task.record(Fact::UserMessage {
        delivery: delivery("d1"),
        member: coder(),
        text: "Use tabs.".to_owned(),
    });
    task.record(Fact::MessageDisposed {
        delivery: delivery("d1"),
        how: Disposition::Queued,
    });
    task.run();
    let fixing = runs(&task, Role::Implementer)[1].clone();

    let text = text_of(&task, &fixing, Session::Continuing);

    assert!(text.contains("# Messages from the user"));
    assert!(text.contains("Use tabs."));
}

#[test]
fn the_first_message_to_a_new_implementer_session_after_a_planners_plan_carries_the_plan_and_no_takeover()
 {
    let mut task = Task::three().script(Script {
        planner: vec![plan_ready()].into(),
        implementer: vec![implements()].into(),
        reviewer: vec![approves()].into(),
        ..Script::default()
    });
    task.run();
    task.decide(Decision::ApprovePlan {
        conditions: Some("Keep the route names.".to_owned()),
    });
    let building = runs(&task, Role::Implementer)[0].clone();
    assert_eq!(
        building.work,
        Work::Implement {
            brief: Brief::Planned
        }
    );

    let text = text_of(&task, &building, Session::Fresh);

    assert!(text.contains("The user approved the plan. Carry it out."));
    assert!(text.contains("A login page behind the existing session helper."));
    assert!(text.contains("Keep the route names."));
    assert!(text.contains("Use the existing session helper."));
    assert!(!text.contains("taking over"));
}

#[test]
fn the_step_back_prompt_is_kratos_own_and_goes_to_the_implementer_unchanged() {
    let mut script = script(
        vec![implements(), fix_f1(), fix_f1(), fix_f1()],
        vec![
            requests_changes(vec![bug("null-deref", "cmd1")]),
            still_open(1, "Still."),
            still_open(1, "Still."),
            approves(),
        ],
    );
    script_commands(
        &mut script,
        "cmd1",
        vec![fails(), fails(), fails(), passes()],
    );
    let mut task = Task::two().script(script);
    task.run();
    task.decide(Decision::StepBack);
    let stepping = runs(&task, Role::Implementer)
        .pop()
        .expect("the implementer stepped back");

    let text = text_of(&task, stepping, Session::Continuing);

    assert!(text.contains(STEP_BACK_TEXT));
}

#[test]
fn the_security_pass_tells_the_reviewer_why_it_runs_and_what_to_look_for() {
    let hit = TriggerHit::Code {
        pattern: "spawn".to_owned(),
        path: "src/run.ts".to_owned(),
        line: "spawn(command);".to_owned(),
    };
    let mut script = script(vec![implements()], vec![approves(), approves()]);
    script.scans = vec![vec![hit]].into();
    let mut task = Task::two().security_pass().script(script);
    task.run();
    let security = runs(&task, Role::Reviewer)[1].clone();

    let text = text_of(&task, &security, Session::Fresh);

    assert!(text.contains("You are the security reviewer"));
    assert!(text.contains("src/run.ts adds a line that matches /spawn/: spawn(command);"));
    assert!(text.contains("Command execution: argument injection"));
    assert!(text.contains("Review the whole change for this task:"));
}

#[test]
fn a_task_title_is_the_first_line_without_headings_and_cut_to_sixty_characters() {
    assert_eq!(
        task_title("# Add a login page\n\nUse the session helper."),
        "Add a login page"
    );
    assert_eq!(task_title("\n  \n"), "Task");
    assert_eq!(
        task_title(&"x".repeat(80)),
        format!("{}...", "x".repeat(57))
    );
    assert_eq!(task_title("  Fix   the\ttabs  "), "Fix the tabs");
    assert_eq!(
        commit_subject(&task_title("Add a login page"), 2),
        "Add a login page (round 2)"
    );
}

#[test]
fn a_branch_starts_with_the_implementers_provider_and_never_clashes_with_an_existing_one() {
    assert_eq!(
        branch_name(Provider::Codex, "Add a login page", &[]),
        "codex/add-a-login-page"
    );
    assert_eq!(
        branch_name(Provider::Claude, "Fix: tabs & spaces!", &[]),
        "claude/fix-tabs-spaces"
    );
    let taken = vec!["codex/add-login".to_owned(), "codex/add-login-2".to_owned()];
    assert_eq!(
        branch_name(Provider::Codex, "Add login", &taken),
        "codex/add-login-3"
    );
    assert_eq!(branch_name(Provider::Codex, "***", &[]), "codex/task");
    let long = branch_name(Provider::Codex, &"word ".repeat(30), &[]);
    assert!(long.len() <= "codex/".len() + 40);
    assert!(!long.ends_with('-'));
}

#[test]
fn a_title_carries_no_control_characters_into_the_commit_subject() {
    assert_eq!(
        task_title("# Fix \u{1b}[31mred\u{7}\u{0} text"),
        "Fix [31mred text"
    );
}

#[test]
fn a_big_probe_file_is_cut_so_it_cannot_fill_the_implementers_context() {
    let big = "x".repeat(10_000);
    let mut script = script(
        vec![implements(), fix_f1()],
        vec![
            requests_changes(vec![with_probes(
                bug("null-deref", "cmd1"),
                &["probes/big.rs"],
            )]),
            approves(),
        ],
    );
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    script.probes = vec![kratos_teams::evidence::ProbeCapture {
        path: "probes/big.rs".to_owned(),
        content: Some(big),
        truncated: false,
    }];
    let policy = kratos_teams::facts::ProbePolicy::Delegated {
        constraints: kratos_teams::task_plan::ProbeConstraints {
            test_paths: vec![pattern("tests/**")],
            note: "Keep fast tests.".to_owned(),
        },
    };
    let mut task = Task::two().probes(policy).script(script);
    task.run();
    let fixing = runs(&task, Role::Implementer)[1].clone();

    let text = text_of(&task, &fixing, Session::Continuing);

    assert!(text.contains(&"x".repeat(4000)));
    assert!(!text.contains(&"x".repeat(4001)));
    assert!(text.contains("(cut short)"));
}
