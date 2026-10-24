//! The one thing that must happen next.
//!
//! `next_step` is a pure function of the view. It does not know whether the engine has already started
//! the step it names. It says that this message must have been delivered and its result recorded, and
//! the engine makes that so exactly once by deduplicating on the message id. Asking twice, after a
//! restart or after a user message, gives the same answer until a result is folded. That is why a user
//! message can arrive in the middle of a step without replacing the active execution.

use serde::Serialize;

use crate::evidence::GIT_DIFF_ARGS;
use crate::facts::{Outcome, ProbePolicy, Sha, StepId};
use crate::members::{Bindings, MemberId};
use crate::prompts::commit_subject;
use crate::reports::{AddedProbe, FindingId, FindingReport, Kind, ReviewDone};
use crate::security_triggers::Trigger;
use crate::task_plan::ProbeConstraints;
use crate::view::{
    Activity, AfterCommit, Finding, Hold, PassKind, Phase, ReviewJob, Run, Stop, TaskView, Turn,
};

/// What the engine does next. The engine handles each variant and nothing else.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    /// Create the worktree, the branch, and the sessions. The result is `Prepared`.
    Prepare,
    /// Deliver `run.message` to `run.member` and record how the turn ends. The result is `TurnEnded`.
    Run { run: Box<Run> },
    /// Commit the worktree under the user's identity. The result is `Committed`.
    Commit { commit: CommitStep },
    /// Check a review's findings. The result is `EvidenceChecked`.
    Check { check: Box<CheckStep> },
    /// Validate and apply the pending role change, release the old roles' processes and worktree access,
    /// and record the new bindings together. The result is `RolesReassigned` or `RoleChangeRejected`.
    ApplyRoles { from: Bindings, to: Bindings },
    /// Try to move the stopped role to another account, because the user approved it (D40). The result is
    /// `AccountHandoff`.
    HandOffAccount { step: StepId, member: MemberId },
    /// Hand the stopped step to the usage scheduler, the only owner of automatic continuation. Asking
    /// twice is safe. The scheduler keys the request by the step and the stopped message, so asking again
    /// after a restart returns the same continuation. The result is `ContinuationScheduled` or
    /// `ContinuationDeclined`.
    HandToScheduler { step: StepId, stop: Stop },
    /// Stop everything for the task, remove its worktrees, and keep the branch. The result is `Cleaned`.
    Cleanup { outcome: Outcome },
    /// Nothing to do until a fact arrives.
    Wait { on: Wait },
}

/// What a waiting task is waiting for. The view has the details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Wait {
    /// A decision from the user. `decisions::allowed_decisions` lists them.
    User,
    /// The scheduled continuation's turn, whose result arrives as an ordinary `TurnEnded`.
    Engine { step: StepId },
    /// The task is closed.
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommitStep {
    pub step: StepId,
    /// Counts from 1 over the task's lifetime.
    pub round: u32,
    /// The commit to `git reset --soft` to before committing. That is the previous round's commit, or the base.
    /// It folds any commits the implementer made itself into this one.
    pub since: Sha,
    /// What the standard review of this commit will diff against. Measure the change from there, for
    /// `Committed::size`.
    pub review_base: Sha,
    /// From `prompts::commit_subject`: the task's first line and the round, with no agent text, so that
    /// no trailer can appear.
    pub subject: String,
    /// Probes the implementer says it added. The engine runs each before and after the fix and answers
    /// in `Committed::probe_runs`.
    pub added_probes: Vec<AddedProbe>,
    pub constraints: Option<ProbeConstraints>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Scan {
    pub triggers: Vec<Trigger>,
    /// Scan the whole task diff: `git diff base commit`, made with `GIT_DIFF_ARGS`.
    pub base: Sha,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Candidate {
    pub id: FindingId,
    pub report: FindingReport,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CheckStep {
    pub step: StepId,
    pub commit: Sha,
    /// The commit the lines of a finding must fall inside the diff of. `None` in the first standard
    /// review, where every confirmed finding counts as inside. The other passes diff from the base.
    pub ranges_base: Option<Sha>,
    /// New findings, each to run through `evidence::start_check`. A nit, a pre-existing problem, or a
    /// design opinion comes back from `start_check` as nothing to check.
    pub candidates: Vec<Candidate>,
    /// Open bugs whose commands Kratos reruns on `commit`. The exit code decides if they are fixed.
    pub recheck: Vec<Candidate>,
    /// `Some` only after a standard review, when the Team runs the security pass, the project has
    /// triggers, and the pass has not run.
    pub scan: Option<Scan>,
    /// Texts a requirement quote may come from.
    pub sources: Vec<String>,
    /// The arguments every diff the engine reads for this step is made with.
    pub git_diff_args: Vec<String>,
}

/// The one step the task needs next. Each phase has exactly one answer.
pub fn next_step(view: &TaskView) -> Step {
    match &view.phase {
        Phase::Active { activity } => active_step(view, activity),
        Phase::Waiting { .. } => Step::Wait { on: Wait::User },
        Phase::Closing { outcome } => Step::Cleanup { outcome: *outcome },
        Phase::Closed { .. } => Step::Wait { on: Wait::Done },
    }
}

fn active_step(view: &TaskView, activity: &Activity) -> Step {
    match activity {
        Activity::Preparing { .. } => Step::Prepare,
        Activity::Turn { turn } => turn_step(view, turn),
        Activity::Committing { step, then } => commit_step(view, *step, then),
        Activity::Checking {
            step, job, report, ..
        } => check_step(view, *step, job, report),
        Activity::Rebinding { to, .. } => Step::ApplyRoles {
            from: view.bindings.clone(),
            to: to.clone(),
        },
    }
}

/// A turn is ready to run unless the engine owns its next move after a usage-limit stop. The rules
/// never send a stopped message again.
fn turn_step(view: &TaskView, turn: &Turn) -> Step {
    match &turn.hold {
        None => Step::Run {
            run: Box::new(view.run_of(turn)),
        },
        Some(Hold::HandOff { .. }) => Step::HandOffAccount {
            step: turn.step,
            member: turn.member.clone(),
        },
        Some(Hold::ToScheduler { stop }) => Step::HandToScheduler {
            step: turn.step,
            stop: stop.clone(),
        },
        Some(Hold::Scheduled { .. }) => Step::Wait {
            on: Wait::Engine { step: turn.step },
        },
    }
}

fn commit_step(view: &TaskView, step: StepId, then: &AfterCommit) -> Step {
    let round = u32::try_from(view.rounds.len() + 1).unwrap_or(u32::MAX);
    let added_probes = match then {
        AfterCommit::Review { report } => report.added_probes.clone(),
        AfterCommit::Accept => Vec::new(),
    };
    let constraints = match &view.policy.probes {
        ProbePolicy::Delegated { constraints } => Some(constraints.clone()),
        ProbePolicy::UserDecides => None,
    };
    Step::Commit {
        commit: CommitStep {
            step,
            round,
            since: view.head().clone(),
            review_base: view
                .last_reviewed
                .clone()
                .unwrap_or_else(|| view.base.clone()),
            subject: commit_subject(&view.title, round),
            added_probes,
            constraints,
        },
    }
}

fn check_step(view: &TaskView, step: StepId, job: &ReviewJob, report: &ReviewDone) -> Step {
    let is_standard = job.pass.kind() == PassKind::Standard;
    let candidates = report
        .findings
        .iter()
        .enumerate()
        .filter(|(_, report)| report.can_block())
        .map(|(index, report)| Candidate {
            id: FindingId::nth(view.findings.len() + index),
            report: report.clone(),
        })
        .collect();
    let recheck = view
        .open_findings()
        .filter(|finding| finding.report.kind == Kind::Bug)
        .map(candidate)
        .collect();
    let scan = (is_standard && view.scans_for_security()).then(|| Scan {
        triggers: view.triggers.clone(),
        base: view.base.clone(),
    });
    let ranges_base = if is_standard {
        view.last_reviewed.clone()
    } else {
        Some(view.base.clone())
    };
    Step::Check {
        check: Box::new(CheckStep {
            step,
            commit: job.commit.clone(),
            ranges_base,
            candidates,
            recheck,
            scan,
            sources: view
                .quote_sources()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            git_diff_args: GIT_DIFF_ARGS.iter().map(|arg| (*arg).to_owned()).collect(),
        }),
    }
}

fn candidate(finding: &Finding) -> Candidate {
    Candidate {
        id: finding.id,
        report: finding.report.clone(),
    }
}
