//! What the user may do, when, and what each choice does to the task.
//!
//! `allowed_decisions` is the list the engine shows. `check_decision` is the check it runs when a
//! decision arrives, and it is a dry run of the real thing, so what is allowed and what applies cannot
//! drift apart. `apply_decision` is the real thing, which the fold calls for a `UserDecided` fact.
//!
//! A cancel and a role change are available whenever a task is alive. User messages are not
//! decisions. They are facts that never change the phase.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::facts::{FactSeq, Outcome, Sha, StepId};
use crate::fold::Rejection;
use crate::members::Bindings;
use crate::reports::{FindingId, RulingVerdict};
use crate::task_plan::Amendment;
use crate::view::{
    Activity, AfterCommit, Answerer, ApprovedBy, Brief, Finding, FindingStatus, Hold, Phase,
    PlanState, Probe, ProbeRejection, ProbeStatus, Resolution, RulingRecord, TaskView, Turn,
    UsageLimitAsk, Waiting, Work,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    /// Approve the drafted plan. The conditions go into the implementer's first brief.
    ApprovePlan {
        conditions: Option<String>,
    },
    /// Send the plan's author back to write it again.
    RevisePlan {
        feedback: String,
    },
    /// Answer a question that was routed to the user, or one the planner failed to answer.
    Answer {
        text: String,
    },
    /// Rule on disputed findings when there is no planner. The named ones are not bugs, and the rest are.
    Rule {
        not_bug: Vec<FindingId>,
    },
    /// Decide an implementer's or planner's plan-change request.
    PlanChange {
        response: PlanChangeResponse,
        instructions: Option<String>,
    },
    /// Give the implementer direction. This is an explicit request to go on, so it starts a new count
    /// of rounds and of fix attempts.
    Direct {
        text: String,
    },
    /// Send the implementer Kratos's step-back prompt. This starts a new count too.
    StepBack,
    /// Try a troubled step again with a fresh retry budget.
    Resume,
    /// Take the work as it is.
    Accept,
    /// Close an approved task. Kratos never merges, and `Merged` records that the user did.
    Finish {
        outcome: Finish,
    },
    Cancel,
    /// Keep these probe paths as permanent tests. The implementer adds them in a short step.
    KeepProbes {
        keep: Vec<String>,
    },
    /// Move roles between the Team's members. It applies before the next new step. Sending the current
    /// roles while a change is pending withdraws it.
    ReassignRoles {
        to: Bindings,
    },
    /// Wait out a long or unknown usage-limit wait, and let the scheduler carry on (D41).
    KeepWaiting,
    /// Let the engine try the stopped role on another account, this once (D40).
    ApproveHandoff,
    /// Wait for the reset instead (D40).
    DeclineHandoff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanChangeResponse {
    /// Add the requested scope to the plan, and let the step go on.
    Allow,
    /// Refuse. The step goes on under the old plan.
    Decline,
    /// Throw the step away. The plan's author writes a new plan, which needs approval again.
    Replan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Finish {
    Merged,
    Rejected,
}

impl From<Finish> for Outcome {
    fn from(finish: Finish) -> Outcome {
        match finish {
            Finish::Merged => Outcome::Merged,
            Finish::Rejected => Outcome::Rejected,
        }
    }
}

/// A `Decision` without its data. `allowed_decisions` lists these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    ApprovePlan,
    RevisePlan,
    Answer,
    Rule,
    PlanChange,
    Direct,
    StepBack,
    Resume,
    Accept,
    Finish,
    Cancel,
    KeepProbes,
    ReassignRoles,
    KeepWaiting,
    ApproveHandoff,
    DeclineHandoff,
}

impl fmt::Display for DecisionKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = serde_json::to_value(self)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        formatter.write_str(&name)
    }
}

impl Decision {
    // #lizard forgives: one arm for each of the 16 variants, and the compiler checks that none is missing.
    pub fn kind(&self) -> DecisionKind {
        match self {
            Decision::ApprovePlan { .. } => DecisionKind::ApprovePlan,
            Decision::RevisePlan { .. } => DecisionKind::RevisePlan,
            Decision::Answer { .. } => DecisionKind::Answer,
            Decision::Rule { .. } => DecisionKind::Rule,
            Decision::PlanChange { .. } => DecisionKind::PlanChange,
            Decision::Direct { .. } => DecisionKind::Direct,
            Decision::StepBack => DecisionKind::StepBack,
            Decision::Resume => DecisionKind::Resume,
            Decision::Accept => DecisionKind::Accept,
            Decision::Finish { .. } => DecisionKind::Finish,
            Decision::Cancel => DecisionKind::Cancel,
            Decision::KeepProbes { .. } => DecisionKind::KeepProbes,
            Decision::ReassignRoles { .. } => DecisionKind::ReassignRoles,
            Decision::KeepWaiting => DecisionKind::KeepWaiting,
            Decision::ApproveHandoff => DecisionKind::ApproveHandoff,
            Decision::DeclineHandoff => DecisionKind::DeclineHandoff,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// What is allowed.
// ---------------------------------------------------------------------------------------------

/// The decisions the task accepts now, in a stable order. Empty once the task is closing or closed.
///
/// | The task is                          | It accepts                                      |
/// | ------------------------------------ | ----------------------------------------------- |
/// | working (an agent or Kratos)         | Cancel                                          |
/// | waiting for plan approval            | ApprovePlan, RevisePlan, Cancel                 |
/// | waiting for an answer                | Answer, Cancel                                  |
/// | waiting for a ruling (no planner)    | Rule, Accept, Cancel                            |
/// | waiting on a plan change             | PlanChange, Cancel                              |
/// | stuck or at the round cap            | Direct, StepBack, Accept, Cancel                |
/// | in trouble                           | Resume, then what fits the failed job, Cancel   |
/// | at a usage limit, handoff allowed    | ApproveHandoff, DeclineHandoff, Cancel          |
/// | at a usage limit, long wait          | KeepWaiting, Cancel                             |
/// | approved                             | Direct, Finish, KeepProbes when offered, Cancel |
///
/// In trouble the user is offered the job that failed. When the planner's answering step failed, that is
/// `Answer`, so the user answers the asker directly. When its ruling step failed, it is `Rule`, the same
/// path a Team with no planner uses. When an implementer, review, commit, or evidence step failed,
/// it is `Direct`, `StepBack`, and `Accept`. When preparation or planning failed, only `Resume` and
/// `Cancel` make sense, because no work exists to direct or accept.
///
/// `ReassignRoles` comes last on every row while the task is alive.
pub fn allowed_decisions(view: &TaskView) -> Vec<DecisionKind> {
    let mut allowed = match &view.phase {
        Phase::Active { .. } => vec![DecisionKind::Cancel],
        Phase::Waiting { waiting } => waiting_decisions(view, waiting),
        Phase::Closing { .. } | Phase::Closed { .. } => return Vec::new(),
    };
    allowed.push(DecisionKind::ReassignRoles);
    allowed
}

fn waiting_decisions(view: &TaskView, waiting: &Waiting) -> Vec<DecisionKind> {
    use DecisionKind::*;
    match waiting {
        Waiting::PlanApproval => vec![ApprovePlan, RevisePlan, Cancel],
        Waiting::Answer => vec![Answer, Cancel],
        Waiting::Ruling { .. } => vec![Rule, Accept, Cancel],
        Waiting::PlanChange => vec![PlanChange, Cancel],
        Waiting::Findings { .. } => vec![Direct, StepBack, Accept, Cancel],
        Waiting::Trouble { left, .. } => trouble_decisions(left),
        Waiting::UsageLimit {
            ask: UsageLimitAsk::Handoff,
            ..
        } => vec![ApproveHandoff, DeclineHandoff, Cancel],
        Waiting::UsageLimit {
            ask: UsageLimitAsk::LongWait { .. },
            ..
        } => vec![KeepWaiting, Cancel],
        Waiting::Approved { .. } if probes_offered(view) => {
            vec![Direct, Finish, KeepProbes, Cancel]
        }
        Waiting::Approved { .. } => vec![Direct, Finish, Cancel],
    }
}

fn trouble_decisions(left: &Activity) -> Vec<DecisionKind> {
    use DecisionKind::*;
    let work = match left {
        Activity::Turn { turn } => Some(&turn.work),
        _ => None,
    };
    let fits: &[DecisionKind] = match (left, work) {
        (Activity::Preparing { .. } | Activity::Rebinding { .. }, _) => &[],
        (_, Some(Work::Plan { .. })) => &[],
        (_, Some(Work::Answer { .. })) => &[Answer],
        (_, Some(Work::Rule { .. })) => &[Rule],
        _ => &[Direct, StepBack, Accept],
    };
    [&[Resume], fits, &[Cancel]].concat()
}

/// `KeepProbes` is offered only while the user decides and a probe still waits for a choice.
fn probes_offered(view: &TaskView) -> bool {
    view.is_user_decides_probes()
        && view
            .probes
            .iter()
            .any(|probe| probe.status == ProbeStatus::Offered)
}

/// Whether `decision` would apply right now, which needs the phase to allow it and its content to be
/// valid. The engine calls this when it records a decision.
pub fn check_decision(view: &TaskView, decision: &Decision) -> Result<(), Rejection> {
    apply_decision(Box::new(view.clone()), decision.clone(), view.seq.next()).map(drop)
}

// ---------------------------------------------------------------------------------------------
// What each decision does.
// ---------------------------------------------------------------------------------------------

/// | Decision       | Effect |
/// | -------------- | ------ |
/// | ApprovePlan    | The plan becomes `Approved` with the user's conditions. The implementer starts. |
/// | RevisePlan     | The plan's author writes it again with the feedback. Approval is needed again. |
/// | Answer         | The suspended step resumes with the user's answer, on the same `StepId`. |
/// | Rule           | The named findings close as not bugs, and the rest go to the implementer. |
/// | PlanChange     | Allow adds the scope as an amendment. Decline keeps the plan. Replan writes a new one. |
/// | Direct         | A new implementer step with the user's direction. It starts a new count of rounds. |
/// | StepBack       | A new implementer step with Kratos's step-back prompt. It starts a new count of rounds. |
/// | Resume         | The troubled activity runs again with a fresh retry budget. |
/// | Accept         | Kratos commits the work as it is, and the user approves it. |
/// | Finish         | The task closes as merged or rejected. Kratos never merges. |
/// | Cancel         | The task closes as cancelled. The active step's late result is stale. |
/// | KeepProbes     | The unpicked probes are rejected, and the implementer adds the picked ones. |
/// | ReassignRoles  | The change becomes pending and applies before the next new step. |
/// | KeepWaiting    | The scheduler schedules the step without asking again. |
/// | ApproveHandoff | The engine tries the stopped role on another account, this once. |
/// | DeclineHandoff | The step waits for the reset. |
// #lizard forgives: one arm for each of the 16 decisions, each calling the function that applies it.
pub(crate) fn apply_decision(
    view: Box<TaskView>,
    decision: Decision,
    seq: FactSeq,
) -> Result<Box<TaskView>, Rejection> {
    let kind = decision.kind();
    if !allowed_decisions(&view).contains(&kind) {
        return Err(Rejection::NotAllowed {
            decision: kind,
            phase: view.phase.name(),
        });
    }
    let invalid = |reason: &str| Rejection::InvalidDecision {
        decision: kind,
        reason: reason.to_owned(),
    };
    match decision {
        Decision::ApprovePlan { conditions } => approve_plan(view, conditions, seq, &invalid),
        Decision::RevisePlan { feedback } => revise_plan(view, feedback, seq, &invalid),
        Decision::Answer { text } => answer(view, text, seq, &invalid),
        Decision::Rule { not_bug } => rule(view, &not_bug, seq, &invalid),
        Decision::PlanChange {
            response,
            instructions,
        } => plan_change(view, response, instructions, seq),
        Decision::Direct { text } => direct(view, Brief::Direction { text }, seq, &invalid),
        Decision::StepBack => direct(view, Brief::StepBack, seq, &invalid),
        Decision::Resume => resume(view, seq),
        Decision::Accept => accept(view, seq),
        Decision::Finish { outcome } => Ok(view.close(outcome.into())),
        Decision::Cancel => Ok(view.close(Outcome::Cancelled)),
        Decision::KeepProbes { keep } => keep_probes(view, &keep, seq, &invalid),
        Decision::ReassignRoles { to } => reassign(view, to, &invalid),
        Decision::KeepWaiting => keep_waiting(view),
        Decision::ApproveHandoff => handoff(view, true),
        Decision::DeclineHandoff => handoff(view, false),
    }
}

type Invalid<'a> = &'a dyn Fn(&str) -> Rejection;

fn not_blank(text: &str, what: &str, invalid: Invalid<'_>) -> Result<(), Rejection> {
    if text.trim().is_empty() {
        return Err(invalid(&format!("{what} must not be blank")));
    }
    Ok(())
}

fn approve_plan(
    mut view: Box<TaskView>,
    conditions: Option<String>,
    seq: FactSeq,
    invalid: Invalid<'_>,
) -> Result<Box<TaskView>, Rejection> {
    if let Some(text) = &conditions {
        not_blank(text, "the conditions", invalid)?;
    }
    let PlanState::Drafted {
        plan,
        author,
        revision,
    } = view.plan.clone()
    else {
        return Err(view.misfit("approve_plan"));
    };
    view.plan = PlanState::Approved {
        plan,
        author,
        revision,
        conditions,
    };
    view.begin_step(
        Work::Implement {
            brief: Brief::Planned,
        },
        seq,
    )
}

fn revise_plan(
    view: Box<TaskView>,
    feedback: String,
    seq: FactSeq,
    invalid: Invalid<'_>,
) -> Result<Box<TaskView>, Rejection> {
    not_blank(&feedback, "the feedback", invalid)?;
    let PlanState::Drafted { author, .. } = &view.plan else {
        return Err(view.misfit("revise_plan"));
    };
    let work = Work::Plan {
        author: *author,
        feedback: Some(feedback),
    };
    view.begin_step(work, seq)
}

/// The user answers the question on top of the stack. When the planner failed to answer, the planner's
/// step is dropped, and the user answers the asker in its place.
fn answer(
    view: Box<TaskView>,
    text: String,
    seq: FactSeq,
    invalid: Invalid<'_>,
) -> Result<Box<TaskView>, Rejection> {
    not_blank(&text, "the answer", invalid)?;
    let Some(question) = view.pending_question() else {
        return Err(view.misfit("answer"));
    };
    let resolution = Resolution::Answered {
        question: question.text.clone(),
        answer: text,
        by: Answerer::User,
    };
    view.resume_step(resolution, seq)
}

/// The user rules on the disputed findings. The same path serves a Team with no planner and a planner
/// that failed to rule.
fn rule(
    mut view: Box<TaskView>,
    not_bug: &[FindingId],
    seq: FactSeq,
    invalid: Invalid<'_>,
) -> Result<Box<TaskView>, Rejection> {
    let (disputed, commit) = match &view.phase {
        Phase::Waiting {
            waiting: Waiting::Ruling { disputed, commit },
        } => (disputed.clone(), commit.clone()),
        Phase::Waiting {
            waiting:
                Waiting::Trouble {
                    left:
                        Activity::Turn {
                            turn:
                                Turn {
                                    work: Work::Rule { disputed, commit },
                                    ..
                                },
                        },
                    ..
                },
        } => (disputed.clone(), commit.clone()),
        _ => return Err(view.misfit("rule")),
    };
    if let Some(stranger) = not_bug.iter().find(|id| !disputed.contains(id)) {
        return Err(invalid(&format!("{stranger} is not a disputed finding")));
    }
    for id in &disputed {
        let verdict = if not_bug.contains(id) {
            RulingVerdict::NotBug
        } else {
            RulingVerdict::Bug
        };
        view.rulings.push(RulingRecord {
            finding: *id,
            verdict,
            reason: String::new(),
            by: Answerer::User,
        });
    }
    view.apply_rulings(not_bug, &disputed, commit, seq)
}

fn plan_change(
    mut view: Box<TaskView>,
    response: PlanChangeResponse,
    instructions: Option<String>,
    seq: FactSeq,
) -> Result<Box<TaskView>, Rejection> {
    let Some(request) = view.pending_plan_change().cloned() else {
        return Err(view.misfit("plan_change"));
    };
    if response == PlanChangeResponse::Replan {
        return replan(view, &request.reason, instructions, seq);
    }
    let allowed = response == PlanChangeResponse::Allow;
    if allowed {
        view.amendments.push(Amendment {
            reason: request.reason.clone(),
            extra_scope: request.extra_scope,
            instructions: instructions.clone(),
        });
    }
    let resolution = Resolution::PlanChanged {
        reason: request.reason,
        allowed,
        instructions,
    };
    view.resume_step(resolution, seq)
}

/// The suspended steps are dropped, because they were working to a plan that is going away. The plan's
/// author writes a new plan, and the user approves it again.
fn replan(
    mut view: Box<TaskView>,
    reason: &str,
    instructions: Option<String>,
    seq: FactSeq,
) -> Result<Box<TaskView>, Rejection> {
    view.suspended.clear();
    if let PlanState::Approved {
        plan,
        author,
        revision,
        ..
    } = view.plan.clone()
    {
        view.plan = PlanState::Drafted {
            plan,
            author,
            revision,
        };
    }
    let feedback = match instructions {
        Some(text) => format!("{reason}\n\n{text}"),
        None => reason.to_owned(),
    };
    let author = view.plan_author();
    view.begin_step(
        Work::Plan {
            author,
            feedback: Some(feedback),
        },
        seq,
    )
}

fn direct(
    mut view: Box<TaskView>,
    brief: Brief,
    seq: FactSeq,
    invalid: Invalid<'_>,
) -> Result<Box<TaskView>, Rejection> {
    if let Brief::Direction { text } = &brief {
        not_blank(text, "the direction", invalid)?;
    }
    view.suspended.clear();
    view.new_leg().begin_step(Work::Implement { brief }, seq)
}

fn resume(view: Box<TaskView>, seq: FactSeq) -> Result<Box<TaskView>, Rejection> {
    let Phase::Waiting {
        waiting: Waiting::Trouble { left, .. },
    } = view.phase.clone()
    else {
        return Err(view.misfit("resume"));
    };
    Ok(view.retake(left, seq))
}

/// Kratos commits the work as it is, and the approval covers that commit.
fn accept(mut view: Box<TaskView>, seq: FactSeq) -> Result<Box<TaskView>, Rejection> {
    view.suspended.clear();
    let step = StepId(seq);
    Ok(view.active(Activity::Committing {
        step,
        then: AfterCommit::Accept,
    }))
}

fn keep_probes(
    mut view: Box<TaskView>,
    keep: &[String],
    seq: FactSeq,
    invalid: Invalid<'_>,
) -> Result<Box<TaskView>, Rejection> {
    let is_offered = |path: &String, probes: &[Probe]| {
        probes
            .iter()
            .any(|probe| probe.path == *path && probe.status == ProbeStatus::Offered)
    };
    if let Some(stranger) = keep.iter().find(|path| !is_offered(path, &view.probes)) {
        return Err(invalid(&format!("{stranger} is not an offered probe")));
    }
    for probe in &mut view.probes {
        if probe.status == ProbeStatus::Offered && !keep.contains(&probe.path) {
            probe.status = ProbeStatus::Rejected {
                reason: ProbeRejection::UserDeclined,
            };
        }
    }
    if keep.is_empty() {
        return Ok(view);
    }
    let brief = Brief::KeepProbes {
        paths: keep.to_vec(),
    };
    view.new_leg().begin_step(Work::Implement { brief }, seq)
}

/// Sending the current roles while a change is pending withdraws it. Any other change must
/// move the same members between roles.
fn reassign(
    mut view: Box<TaskView>,
    to: Bindings,
    invalid: Invalid<'_>,
) -> Result<Box<TaskView>, Rejection> {
    if view.pending_roles.is_some() && to == view.bindings {
        view.pending_roles = None;
        return Ok(view);
    }
    if let Some(problem) = view.bindings.reassignment_problem(&to) {
        return Err(invalid(&problem.to_string()));
    }
    view.pending_roles = Some(to);
    Ok(view)
}

fn usage_limit_turn(view: &TaskView) -> Result<Turn, Rejection> {
    match &view.phase {
        Phase::Waiting {
            waiting: Waiting::UsageLimit { turn, .. },
        } => Ok(turn.clone()),
        _ => Err(view.misfit("a usage-limit decision")),
    }
}

fn keep_waiting(view: Box<TaskView>) -> Result<Box<TaskView>, Rejection> {
    let mut turn = usage_limit_turn(&view)?;
    if let Some(Hold::ToScheduler { stop }) = &mut turn.hold {
        stop.user_accepted_wait = true;
    }
    Ok(view.active(Activity::Turn { turn }))
}

fn handoff(view: Box<TaskView>, approved: bool) -> Result<Box<TaskView>, Rejection> {
    let mut turn = usage_limit_turn(&view)?;
    if let (true, Some(Hold::ToScheduler { stop })) = (approved, turn.hold.clone()) {
        turn.hold = Some(Hold::HandOff { stop });
    }
    Ok(view.active(Activity::Turn { turn }))
}

// ---------------------------------------------------------------------------------------------
// The approval report.
// ---------------------------------------------------------------------------------------------

/// What the user reads before the final call. It is built from the view, so it cannot leave anything out.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ApprovalReport {
    pub commit: Sha,
    pub approved_by: ApprovedBy,
    pub rounds: usize,
    /// Findings that never blocked: nits, pre-existing bugs, design opinions, and real problems outside
    /// the changed lines. They are ranked by severity (D94).
    pub listed: Vec<Finding>,
    /// Findings Kratos could not confirm, with the reason.
    pub dropped: Vec<Finding>,
    /// Changed files outside the agreed scope.
    pub outside_scope: Vec<String>,
    /// Every probe, kept or rejected or still offered, with the output that showed the bug (D78).
    pub probes: Vec<Probe>,
}

/// `Some` exactly while the task waits in `Waiting::Approved`.
pub fn approval_report(view: &TaskView) -> Option<ApprovalReport> {
    let Phase::Waiting {
        waiting: Waiting::Approved { commit, by },
    } = &view.phase
    else {
        return None;
    };
    let mut listed: Vec<Finding> = view
        .findings
        .iter()
        .filter(|f| matches!(f.status, FindingStatus::Listed { .. }))
        .cloned()
        .collect();
    listed.sort_by_key(Finding::rank);
    let dropped = view
        .findings
        .iter()
        .filter(|f| matches!(f.status, FindingStatus::Dropped { .. }))
        .cloned()
        .collect();
    Some(ApprovalReport {
        commit: commit.clone(),
        approved_by: *by,
        rounds: view.rounds.len(),
        listed,
        dropped,
        outside_scope: view.outside_scope(),
        probes: view.probes.clone(),
    })
}
