//! Reading a finished task, and the check that a restart at any fact continues the same way.

use kratos_teams::decisions::{
    Decision, DecisionKind, Finish, PlanChangeResponse, allowed_decisions, check_decision,
};
use kratos_teams::facts::{Fact, MessageId, StoredFact};
use kratos_teams::fold::fold;
use kratos_teams::members::{Bindings, Role};
use kratos_teams::next_step::{CheckStep, Step, next_step};
use kratos_teams::prompts::{Platform, PromptContext, Session, message};
use kratos_teams::view::{
    Activity, ApprovedBy, Phase, ProbeStatus, Run, TaskView, Trouble, Turn, Waiting, Work,
};

use crate::support::{ScriptedTask, sha};

/// The phase of an approved task.
pub fn approved(commit: u64, by: ApprovedBy) -> Phase {
    Phase::Waiting {
        waiting: Waiting::Approved {
            commit: sha(commit),
            by,
        },
    }
}

pub fn waiting(view: &TaskView) -> Option<&Waiting> {
    match &view.phase {
        Phase::Waiting { waiting } => Some(waiting),
        _ => None,
    }
}

/// The pause a task is in because of trouble, if any.
pub fn trouble(view: &TaskView) -> Option<(Trouble, Role, &str)> {
    match waiting(view)? {
        Waiting::Trouble {
            why, role, detail, ..
        } => Some((*why, *role, detail)),
        _ => None,
    }
}

/// The decisions a task accepts besides the one that is always available.
pub fn decisions(view: &TaskView) -> Vec<DecisionKind> {
    allowed_decisions(view)
        .into_iter()
        .filter(|kind| *kind != DecisionKind::ReassignRoles)
        .collect()
}

/// The messages the loop delivered to `role`, in order.
pub fn runs(task: &ScriptedTask, role: Role) -> Vec<&Run> {
    task.steps
        .iter()
        .filter_map(|step| match step {
            Step::Run { run } if run.role == role => Some(&**run),
            _ => None,
        })
        .collect()
}

/// The kinds of correction the messages carried, in order, with `None` for a message that was not one.
pub fn notes(runs: &[&Run]) -> Vec<Option<String>> {
    runs.iter()
        .map(|run| {
            run.note.as_ref().map(|note| {
                serde_json::to_value(note).expect("a note serializes")["kind"]
                    .as_str()
                    .expect("tagged")
                    .to_owned()
            })
        })
        .collect()
}

/// The evidence checks the loop ran.
pub fn checks(task: &ScriptedTask) -> Vec<&CheckStep> {
    task.steps
        .iter()
        .filter_map(|step| match step {
            Step::Check { check } => Some(&**check),
            _ => None,
        })
        .collect()
}

/// Each finding's status, with the reason for a listed or dropped one.
pub fn outcomes(view: &TaskView) -> Vec<String> {
    use kratos_teams::view::FindingStatus as Status;
    view.findings
        .iter()
        .map(|finding| match &finding.status {
            Status::Listed { why } => format!("{why:?}"),
            Status::Dropped { reason } => format!("{:?}", reason.why),
            Status::Closed { how } => format!("Closed {how:?}"),
            Status::Open { .. } => "Open".to_owned(),
        })
        .collect()
}

/// A decision of `kind` that is valid wherever the task allows that kind.
pub fn sample_decision(kind: DecisionKind, view: &TaskView) -> Decision {
    samples(view)
        .into_iter()
        .find(|decision| decision.kind() == kind)
        .unwrap_or_else(|| panic!("no sample decision of kind {kind}"))
}

/// One decision of every kind, with content that fits the task as it stands.
fn samples(view: &TaskView) -> Vec<Decision> {
    let disputed = match waiting(view) {
        Some(Waiting::Ruling { disputed, .. }) => disputed.clone(),
        Some(Waiting::Trouble {
            left:
                Activity::Turn {
                    turn:
                        Turn {
                            work: Work::Rule { disputed, .. },
                            ..
                        },
                },
            ..
        }) => disputed.clone(),
        _ => Vec::new(),
    };
    let offered = view
        .probes
        .iter()
        .filter(|probe| probe.status == ProbeStatus::Offered)
        .map(|probe| probe.path.clone())
        .take(1)
        .collect();
    vec![
        Decision::ApprovePlan { conditions: None },
        Decision::RevisePlan {
            feedback: "Narrow the scope.".to_owned(),
        },
        Decision::Answer {
            text: "Use the session helper.".to_owned(),
        },
        Decision::Rule {
            not_bug: disputed.into_iter().take(1).collect(),
        },
        Decision::PlanChange {
            response: PlanChangeResponse::Allow,
            instructions: None,
        },
        Decision::Direct {
            text: "Use the session helper.".to_owned(),
        },
        Decision::StepBack,
        Decision::Resume,
        Decision::Accept,
        Decision::Finish {
            outcome: Finish::Merged,
        },
        Decision::Cancel,
        Decision::KeepProbes { keep: offered },
        Decision::ReassignRoles {
            to: swap_implementer_and_reviewer(&view.bindings),
        },
        Decision::KeepWaiting,
        Decision::ApproveHandoff,
        Decision::DeclineHandoff,
    ]
}

pub fn swap_implementer_and_reviewer(bindings: &Bindings) -> Bindings {
    let implementer = bindings.member(Role::Implementer).expect("bound").clone();
    let reviewer = bindings.member(Role::Reviewer).expect("bound").clone();
    match bindings.member(Role::Planner) {
        Some(planner) => Bindings::three(planner.clone(), reviewer, implementer),
        None => Bindings::two(reviewer, implementer),
    }
    .expect("distinct members")
}

/// Every prefix of the record folds the same from a stored copy, plans the same step, accepts each
/// decision it allows, and never waits again for a message whose result it already holds. A restart at
/// any fact therefore continues the same way, and no completed step repeats.
pub fn assert_log_replayable(log: &[StoredFact]) {
    let mut completed: Vec<MessageId> = Vec::new();
    for n in 1..=log.len() {
        let prefix = &log[..n];
        let view = fold(prefix).expect("every prefix folds");
        let stored: Vec<StoredFact> = prefix
            .iter()
            .map(|fact| {
                let json = serde_json::to_string(fact).expect("a fact serializes");
                serde_json::from_str(&json).expect("a fact reads back")
            })
            .collect();
        let restored = fold(&stored).expect("the stored copy folds");
        assert_eq!(
            restored, view,
            "a stored copy of the first {n} facts folds differently"
        );
        assert_eq!(next_step(&restored), next_step(&view));
        for kind in allowed_decisions(&view) {
            let decision = sample_decision(kind, &view);
            check_decision(&view, &decision).unwrap_or_else(|error| {
                panic!("{kind} is allowed after fact {n} but refused: {error}")
            });
        }
        if let Some(message) = view.phase.waiting_message() {
            assert!(
                !completed.contains(message),
                "after fact {n} the task waits again for {message:?}, which already ended"
            );
        }
        if let Fact::TurnEnded { message, .. } = &log[n - 1].fact {
            completed.push(message.clone());
        }
    }
}

/// `assert_log_replayable` for a scripted task, and no message id is sent twice.
pub fn assert_replayable(task: &ScriptedTask) {
    let mut messages: Vec<&MessageId> = task
        .steps
        .iter()
        .filter_map(|step| match step {
            Step::Run { run } => Some(&run.message),
            _ => None,
        })
        .collect();
    let sent = messages.len();
    messages.sort();
    messages.dedup();
    assert_eq!(messages.len(), sent, "a message id was sent twice");
    assert_log_replayable(task.log());
}

/// The view as it was when the engine delivered `run`: the prefix of the log that waits for its message.
pub fn view_when_sent(task: &ScriptedTask, run: &Run) -> TaskView {
    (1..=task.log().len())
        .map(|n| fold(&task.log()[..n]).expect("every prefix folds"))
        .find(|view| view.phase.waiting_message() == Some(&run.message))
        .unwrap_or_else(|| panic!("no prefix of the log waits for {:?}", run.message))
}

/// The text the engine would send for `run`.
pub fn text_of(task: &ScriptedTask, run: &Run, session: Session) -> String {
    let view = view_when_sent(task, run);
    let context = PromptContext {
        session,
        diff_stat: " src/a.rs | 2 +-",
        platform: Platform::Windows,
    };
    message(&view, run, &context)
}
