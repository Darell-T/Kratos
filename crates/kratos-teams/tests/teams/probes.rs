//! Probe tests (D78). The reviewer can leave probe tests while it proves a bug. The user decides which
//! become permanent tests, or hands the choice to the implementer inside recorded constraints. Only the
//! implementer adds a probe to the project, and the approval report always lists what was kept and what
//! was not, with the output that showed each bug.

use kratos_teams::decisions::{Decision, DecisionKind, approval_report, check_decision};
use kratos_teams::evidence::ProbeCapture;
use kratos_teams::facts::{ChangeSize, Ended, Fact, ProbePolicy};
use kratos_teams::fold::Rejection;
use kratos_teams::members::Role;
use kratos_teams::next_step::Step;
use kratos_teams::reports::{AddedProbe, ImplementerDone};
use kratos_teams::task_plan::ProbeConstraints;
use kratos_teams::view::{ApprovedBy, Brief, Note, Probe, ProbeRejection, ProbeStatus, Work};

use crate::replay::*;
use crate::support::*;

const PROBE: &str = "probes/null_session.rs";

fn capture(path: &str) -> ProbeCapture {
    ProbeCapture {
        path: path.to_owned(),
        content: Some("#[test] fn null_session() {}".to_owned()),
        truncated: false,
    }
}

fn implements_adding(from: &str, to: &str) -> Ended {
    implements_with(
        ImplementerDone {
            responses: vec![fixed(1)],
            added_probes: vec![AddedProbe {
                from: from.to_owned(),
                to: to.to_owned(),
            }],
            ..done()
        },
        checked(),
    )
}

/// Round 1 finds a bug and leaves a probe. Round 2 fixes it, and `implementer` is what the fix does.
fn script_with_probe(implementer: Ended, reviewer_after: Vec<Ended>) -> Script {
    let mut reviewer = vec![requests_changes(vec![with_probes(
        bug("null-deref", "cmd1"),
        &[PROBE],
    )])];
    reviewer.extend(reviewer_after);
    let mut script = Script {
        implementer: vec![implements(), implementer].into(),
        reviewer: reviewer.into(),
        probes: vec![capture(PROBE)],
        ..Script::default()
    };
    script_commands(&mut script, "cmd1", vec![fails(), passes()]);
    script
}

fn probe(task: &ScriptedTask) -> Probe {
    task.view().probes[0].clone()
}

fn delegated(test_paths: &[&str]) -> ProbePolicy {
    ProbePolicy::Delegated {
        constraints: ProbeConstraints {
            test_paths: test_paths.iter().map(|p| pattern(p)).collect(),
            note: "Keep only fast tests.".to_owned(),
        },
    }
}

#[test]
fn a_probe_is_captured_with_its_finding_and_the_output_that_showed_the_bug() {
    let mut task = Task::two().script(script_with_probe(
        implements_answering(vec![fixed(1)]),
        vec![approves()],
    ));

    task.run();

    let kept = probe(&task);
    assert_eq!(kept.path, PROBE);
    assert_eq!(kept.finding, Some(fid(1)));
    assert_eq!(kept.showed.as_deref(), Some("FAIL"));
    assert_eq!(kept.status, ProbeStatus::Offered);
    assert_eq!(
        kept.content.as_deref(),
        Some("#[test] fn null_session() {}")
    );
}

#[test]
fn the_user_decides_which_probes_stay_and_the_implementer_adds_the_picks_in_one_more_step() {
    let mut script = script_with_probe(
        implements_answering(vec![fixed(1)]),
        vec![approves(), approves()],
    );
    script.implementer.push_back(Ended::Reported {
        reported: kratos_teams::reports::Reported::Implementer {
            report: kratos_teams::reports::Report::Finished {
                done: ImplementerDone {
                    added_probes: vec![AddedProbe {
                        from: PROBE.to_owned(),
                        to: "tests/null_session.rs".to_owned(),
                    }],
                    ..done()
                },
            },
            changes: checked(),
        },
    });
    let mut task = Task::two().script(script);
    task.run();
    assert_eq!(
        decisions(&task.view()),
        [
            DecisionKind::Direct,
            DecisionKind::Finish,
            DecisionKind::KeepProbes,
            DecisionKind::Cancel
        ]
    );

    task.decide(Decision::KeepProbes {
        keep: vec![PROBE.to_owned()],
    });

    let adding = runs(&task, Role::Implementer)
        .pop()
        .expect("the implementer added the pick");
    assert_eq!(
        adding.work,
        Work::Implement {
            brief: Brief::KeepProbes {
                paths: vec![PROBE.to_owned()]
            }
        }
    );
    let view = task.view();
    assert_eq!(
        view.phase,
        approved(3, ApprovedBy::Reviewer),
        "the new commit was reviewed like any other"
    );
    assert_eq!(
        view.probes[0].status,
        ProbeStatus::Retained {
            as_path: "tests/null_session.rs".to_owned(),
            evidence: "assertion failed".to_owned()
        }
    );
    let report = approval_report(&view).expect("approved");
    assert_eq!(report.probes[0].showed.as_deref(), Some("FAIL"));
    assert_replayable(&task);
}

#[test]
fn unpicked_probes_are_rejected_and_picking_none_adds_no_step() {
    let mut task = Task::two().script(script_with_probe(
        implements_answering(vec![fixed(1)]),
        vec![approves()],
    ));
    task.run();
    let steps_before = task.steps.len();

    let ran = task.decide(Decision::KeepProbes { keep: vec![] });

    assert_eq!(ran, Vec::<String>::new());
    assert_eq!(task.steps.len(), steps_before);
    assert_eq!(
        probe(&task).status,
        ProbeStatus::Rejected {
            reason: ProbeRejection::UserDeclined
        }
    );
    assert_eq!(
        decisions(&task.view()),
        [
            DecisionKind::Direct,
            DecisionKind::Finish,
            DecisionKind::Cancel
        ],
        "nothing is left to keep"
    );
}

#[test]
fn a_pick_must_be_a_probe_the_reviewer_left() {
    let mut task = Task::two().script(script_with_probe(
        implements_answering(vec![fixed(1)]),
        vec![approves()],
    ));
    task.run();

    let refused = check_decision(
        &task.view(),
        &Decision::KeepProbes {
            keep: vec!["probes/other.rs".to_owned()],
        },
    );

    assert!(matches!(refused, Err(Rejection::InvalidDecision { .. })));
}

#[test]
fn an_implementer_who_forgets_a_pick_is_sent_the_missing_path() {
    let mut script = script_with_probe(
        implements_answering(vec![fixed(1)]),
        vec![approves(), approves()],
    );
    script.implementer.push_back(implements());
    script
        .implementer
        .push_back(implements_adding(PROBE, "tests/null_session.rs"));
    let mut task = Task::two().script(script);
    task.run();

    task.decide(Decision::KeepProbes {
        keep: vec![PROBE.to_owned()],
    });

    let asked = runs(&task, Role::Implementer);
    assert_eq!(
        asked.last().and_then(|run| run.note.clone()),
        Some(Note::InvalidReport {
            error: format!("added_probes: add each probe the user chose. Missing: {PROBE}.")
        })
    );
}

#[test]
fn the_reviewer_cannot_get_a_probe_into_the_project_and_the_implementer_needs_permission_to() {
    // The user did not delegate and did not pick, so adding a probe is not allowed.
    let mut script = script_with_probe(
        implements_adding(PROBE, "tests/null_session.rs"),
        vec![approves()],
    );
    script
        .implementer
        .push_back(implements_answering(vec![fixed(1)]));
    let mut task = Task::two().script(script);

    task.run();

    let asked = runs(&task, Role::Implementer);
    assert_eq!(
        asked[2].note,
        Some(Note::InvalidReport {
            error: "added_probes: you may not add probes now. Leave it empty.".to_owned()
        })
    );
}

#[test]
fn a_probe_the_reviewer_never_left_cannot_be_added() {
    let mut script = script_with_probe(
        implements_adding("probes/invented.rs", "tests/invented.rs"),
        vec![approves()],
    );
    script
        .implementer
        .push_back(implements_answering(vec![fixed(1)]));
    let mut task = Task::two().probes(delegated(&["tests/**"])).script(script);

    task.run();

    let asked = runs(&task, Role::Implementer);
    assert_eq!(
        asked[2].note,
        Some(Note::InvalidReport {
            error: "added_probes: probes/invented.rs is not a probe the reviewer left.".to_owned()
        })
    );
}

fn delegated_outcome(
    scope: &[&str],
    test_paths: &[&str],
    to: &str,
    failed_before_fix: bool,
    passed_after_fix: bool,
) -> ProbeStatus {
    let mut script = script_with_probe(implements_adding(PROBE, to), vec![approves()]);
    script.probe_runs.insert(
        PROBE.to_owned(),
        probe_run(PROBE, failed_before_fix, passed_after_fix),
    );
    let mut task = Task::two()
        .scope(scope)
        .probes(delegated(test_paths))
        .script(script);
    task.run();
    probe(&task).status
}

#[test]
fn a_delegated_probe_inside_the_users_folders_and_scope_that_fails_before_and_passes_after_is_kept()
{
    let status = delegated_outcome(
        &["src/**", "tests/**"],
        &["tests/**"],
        "tests/null_session.rs",
        true,
        true,
    );

    assert_eq!(
        status,
        ProbeStatus::Retained {
            as_path: "tests/null_session.rs".to_owned(),
            evidence: "assertion failed".to_owned()
        }
    );
}

#[test]
fn a_delegated_copy_outside_the_users_folders_is_rejected() {
    let status = delegated_outcome(&["**"], &["tests/**"], "src/null_session.rs", true, true);

    assert_eq!(
        status,
        ProbeStatus::Rejected {
            reason: ProbeRejection::OutsideAllowedPaths
        }
    );
}

#[test]
fn a_delegated_copy_outside_the_scope_is_rejected() {
    let status = delegated_outcome(
        &["src/**"],
        &["tests/**"],
        "tests/null_session.rs",
        true,
        true,
    );

    assert_eq!(
        status,
        ProbeStatus::Rejected {
            reason: ProbeRejection::OutsideScope
        }
    );
}

#[test]
fn a_copy_that_already_passes_before_the_fix_proves_nothing_and_is_rejected() {
    let status = delegated_outcome(&["**"], &["tests/**"], "tests/null_session.rs", false, true);

    assert_eq!(
        status,
        ProbeStatus::Rejected {
            reason: ProbeRejection::PassedBeforeTheFix
        }
    );
}

#[test]
fn a_copy_that_still_fails_after_the_fix_is_rejected() {
    let status = delegated_outcome(&["**"], &["tests/**"], "tests/null_session.rs", true, false);

    assert_eq!(
        status,
        ProbeStatus::Rejected {
            reason: ProbeRejection::FailedAfterTheFix
        }
    );
}

#[test]
fn when_the_choice_is_delegated_probes_nobody_added_are_listed_as_rejected_at_approval() {
    let mut task = Task::two()
        .probes(delegated(&["tests/**"]))
        .script(script_with_probe(
            implements_answering(vec![fixed(1)]),
            vec![approves()],
        ));

    task.run();

    assert_eq!(
        probe(&task).status,
        ProbeStatus::Rejected {
            reason: ProbeRejection::NotAdded
        }
    );
    let report = approval_report(&task.view()).expect("approved");
    assert_eq!(report.probes.len(), 1);
    assert_eq!(
        decisions(&task.view()),
        [
            DecisionKind::Direct,
            DecisionKind::Finish,
            DecisionKind::Cancel
        ],
        "the user has nothing left to pick"
    );
}

#[test]
fn a_delegated_implementer_is_told_the_constraints_and_gets_the_probe_for_its_finding() {
    let mut task = Task::two()
        .probes(delegated(&["tests/**"]))
        .script(script_with_probe(
            implements_answering(vec![fixed(1)]),
            vec![approves()],
        ));
    task.run();
    let fixing = runs(&task, Role::Implementer)[1].clone();
    let text = kratos_teams::prompts::message(
        &view_before(&task, fixing.message.as_str()),
        &fixing,
        &kratos_teams::prompts::PromptContext {
            session: kratos_teams::prompts::Session::Continuing,
            diff_stat: "",
            platform: kratos_teams::prompts::Platform::Unix,
        },
    );

    assert!(text.contains("tests/**"));
    assert!(text.contains("Keep only fast tests."));
    assert!(text.contains(PROBE));
    assert!(text.contains("#[test] fn null_session() {}"));
}

/// The view as it was when the engine delivered `message`.
fn view_before(task: &ScriptedTask, message: &str) -> kratos_teams::view::TaskView {
    for n in 1..=task.log().len() {
        let view = kratos_teams::fold::fold(&task.log()[..n]).expect("folds");
        if view.phase.waiting_message().map(|m| m.as_str()) == Some(message) {
            return view;
        }
    }
    panic!("no prefix waits for {message}");
}

#[test]
fn a_commit_without_a_run_for_an_added_probe_is_a_damaged_record() {
    let mut script = script_with_probe(
        implements_adding(PROBE, "tests/null_session.rs"),
        vec![approves()],
    );
    script.implementer.clear();
    script.implementer.push_back(implements());
    script
        .implementer
        .push_back(implements_adding(PROBE, "tests/null_session.rs"));
    let mut task = Task::two().probes(delegated(&["tests/**"])).script(script);
    while !matches!(task.next(), Step::Commit { commit } if commit.round == 2) {
        task.step();
    }

    let refused = task.view().accepts(&Fact::Committed {
        commit: sha(2),
        files: vec!["src/a.rs".to_owned()],
        size: ChangeSize::default(),
        probe_runs: vec![],
    });

    assert!(matches!(refused, Err(Rejection::InvalidFact(_))));
}

#[test]
fn a_delegated_copy_that_climbs_out_of_the_users_folder_is_rejected_though_a_glob_matches_its_text()
{
    for to in [
        "tests/../../outside.rs",
        "/etc/null_session.rs",
        "C:/work/null_session.rs",
    ] {
        let status = delegated_outcome(&["**"], &["**"], to, true, true);

        assert_eq!(
            status,
            ProbeStatus::Rejected {
                reason: ProbeRejection::OutsideAllowedPaths
            },
            "{to}"
        );
    }
}
