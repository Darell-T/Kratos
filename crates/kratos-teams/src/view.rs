//! Everything the rules know about one task, derived only from its log and never stored.
//!
//! The view is a state machine. `Phase` says whether Kratos or an agent can take the next step alone
//! (`Active`), the user must decide (`Waiting`), or the task is ending (`Closing`, `Closed`). Only
//! `Waiting` needs the user, and it names exactly what the user is asked.
//!
//! Folding lives in `fold.rs`, `turns.rs`, `rounds.rs`, `user_messages.rs`, and `decisions.rs`. This file is
//! the data they build, plus the questions the other modules ask of it.

use std::collections::{BTreeMap, BTreeSet};

use kratos_protocol::Provider;
use serde::Serialize;

use crate::evidence::{DropReason, EvidenceCheck, Rerun};
use crate::facts::{
    ChangeSize, ContinuationId, Declined, DeliveryId, Depth, FactSeq, MessageId, Outcome,
    ProbePolicy, RulesSnapshot, Sha, StepId, TaskPolicy, UnixMillis,
};
use crate::members::{Bindings, MemberId, Role};
use crate::reports::{
    FindingId, FindingReport, ImplementerDone, Kind, ResponseOutcome, ReviewDone, RulingVerdict,
    Severity, Verdict,
};
use crate::security_triggers::{Trigger, TriggerHit};
use crate::task_plan::{Amendment, PlanChangeRequest, Scope, ScopePattern, ScopeSource, TaskPlan};

/// Fix attempts a blocking finding gets before the task pauses as stuck.
pub const FIX_ATTEMPTS: u32 = 2;
/// Questions one step may ask before the next goes to the user and not to the planner. This keeps a
/// planner and an agent from talking in circles and spending tokens.
pub const MAX_QUESTIONS_PER_STEP: u32 = 3;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TaskView {
    // Frozen when the task started.
    pub request: String,
    pub constraints: Vec<String>,
    pub explicit_scope: Vec<ScopePattern>,
    pub title: String,
    pub branch: String,
    pub base: Sha,
    pub rules: RulesSnapshot,
    pub triggers: Vec<Trigger>,
    pub policy: TaskPolicy,

    /// Who holds which role, and the change that waits for the next new step.
    pub bindings: Bindings,
    pub pending_roles: Option<Bindings>,

    pub plan: PlanState,
    /// Plan changes the user allowed, oldest first.
    pub amendments: Vec<Amendment>,
    /// Requirements accepted through `Direct`, oldest first. They remain in force across steps,
    /// step-back requests and replanning, separately from messages addressed to one member.
    pub requirements: Vec<String>,

    // The record.
    pub rounds: Vec<Round>,
    pub findings: Vec<Finding>,
    pub rulings: Vec<RulingRecord>,
    pub probes: Vec<Probe>,
    pub directions: Vec<Direction>,
    /// Every path any round's commit changed, for the outside-scope list.
    pub changed_files: BTreeSet<String>,

    /// A file changed after the implementer's last command, folded across its turns since the last
    /// commit. A turn that ran no command cannot clear it, and a lost turn counts as dirty.
    pub dirty: bool,
    /// How many rounds existed when the user last asked for more work. The round cap and each open
    /// finding's fix attempts count from there. Only the user's explicit requests move it (D89).
    pub leg_start: usize,
    /// The last commit a standard review judged. The next standard review reads the diff from there.
    pub last_reviewed: Option<Sha>,
    /// The security and regressions passes each run once per task.
    pub passes_done: BTreeSet<PassKind>,

    /// Steps set aside until something is answered or decided, newest last. At most two deep. An
    /// implementer or reviewer asks the planner, and the planner may ask the user. Empty whenever the
    /// user could direct, step back, or accept, and whenever a role change applies.
    pub suspended: Vec<Suspended>,
    pub phase: Phase,
    pub seq: FactSeq,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanState {
    NoPlan,
    /// Written, waiting for the user. `revision` counts drafts.
    Drafted {
        plan: TaskPlan,
        author: Role,
        revision: u32,
    },
    Approved {
        plan: TaskPlan,
        author: Role,
        revision: u32,
        conditions: Option<String>,
    },
}

// ---------------------------------------------------------------------------------------------
// Phases.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    /// Kratos or an agent can take the next step alone. Only a cancel and a role change are
    /// accepted.
    Active {
        activity: Activity,
    },
    /// The user must decide. `decisions::allowed_decisions` says what is accepted.
    Waiting {
        waiting: Waiting,
    },
    /// Cleanup is due.
    Closing {
        outcome: Outcome,
    },
    Closed {
        outcome: Outcome,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Activity {
    Preparing {
        step: StepId,
    },
    Turn {
        turn: Turn,
    },
    Committing {
        step: StepId,
        then: AfterCommit,
    },
    /// A reviewer's finished report, waiting for Kratos to check its evidence. The finished `turn` is
    /// kept so that a verdict mismatch can send a correction on the same step.
    Checking {
        step: StepId,
        turn: Turn,
        job: ReviewJob,
        report: ReviewDone,
    },
    /// A role change was pending when a new step came due. The step starts once the engine applies it.
    Rebinding {
        step: StepId,
        to: Bindings,
        then: Work,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AfterCommit {
    /// The implementer handed off. Review the commit.
    Review { report: ImplementerDone },
    /// The user accepted the work as it is.
    Accept,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Waiting {
    /// The drafted plan is in `TaskView::plan`.
    PlanApproval,
    /// A question routed to the user. It is on top of `TaskView::suspended`.
    Answer,
    /// Disputed findings with nobody to rule, so the user rules.
    Ruling {
        disputed: Vec<FindingId>,
        commit: Sha,
    },
    /// A plan-change request. It is on top of `TaskView::suspended`.
    PlanChange,
    Findings {
        why: FindingsPause,
        ids: Vec<FindingId>,
    },
    Trouble {
        why: Trouble,
        role: Role,
        detail: String,
        left: Activity,
    },
    /// A usage limit stopped the turn, and the user decides what happens next.
    UsageLimit {
        turn: Turn,
        ask: UsageLimitAsk,
    },
    Approved {
        commit: Sha,
        by: ApprovedBy,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingsPause {
    /// A blocking finding survived two fix attempts.
    Stuck,
    /// The round cap ended with blocking findings.
    Cap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trouble {
    InvalidReport,
    Lost,
    UncheckedEdits,
    VerdictMismatch,
    Blocked,
    /// A Kratos step failed. A git error is one.
    System,
    /// The engine could not apply a requested role change.
    RoleChange,
}

/// What the user is asked after a provider stopped a turn at a usage limit.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageLimitAsk {
    /// The Team allows moving the role to another account, and each move needs the user's approval (D40).
    Handoff,
    /// The scheduler will not resume by itself (D41). The user keeps waiting or cancels.
    LongWait { why: Declined },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovedBy {
    Reviewer,
    User,
}

// ---------------------------------------------------------------------------------------------
// Turns. Each is one piece of agent work.
// ---------------------------------------------------------------------------------------------

/// What a member is asked to do. Each variant carries only its own inputs.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Work {
    /// Write the plan. `author` is the planner, or the implementer in a two-member Team with planning on.
    Plan {
        author: Role,
        feedback: Option<String>,
    },
    Implement {
        brief: Brief,
    },
    /// The planner answers a question another member asked. That member's step stays suspended.
    Answer {
        question: Question,
    },
    /// The planner rules on every disputed finding.
    Rule {
        disputed: Vec<FindingId>,
        commit: Sha,
    },
    Review {
        job: ReviewJob,
    },
}

impl Work {
    /// What the work is, in words for a refusal.
    pub fn name(&self) -> &'static str {
        match self {
            Work::Plan { .. } => "plan",
            Work::Implement { .. } => "implementer",
            Work::Answer { .. } => "answer",
            Work::Rule { .. } => "ruling",
            Work::Review { .. } => "review",
        }
    }

    /// The role that does this work. The bindings turn it into a member when the step starts.
    pub fn role(&self) -> Role {
        match self {
            Work::Plan { author, .. } => *author,
            Work::Implement { .. } => Role::Implementer,
            Work::Answer { .. } | Work::Rule { .. } => Role::Planner,
            Work::Review { .. } => Role::Reviewer,
        }
    }

    /// Whether the turn runs in the provider's read-only plan mode. Writing a plan and every planner
    /// job do. The answer depends on the work alone, so a restarted task cannot re-enter plan mode
    /// after its plan was approved.
    pub fn plan_mode(&self) -> bool {
        matches!(
            self,
            Work::Plan { .. } | Work::Answer { .. } | Work::Rule { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Brief {
    /// There is no plan. Start from the request and the user's constraints.
    Start,
    /// The user approved the plan.
    Planned,
    /// Confirmed blocking findings.
    Findings,
    Direction {
        text: String,
    },
    StepBack,
    /// The ruling found these disputed findings to be bugs, so the implementer fixes them.
    Rulings {
        bugs: Vec<FindingId>,
    },
    /// The user chose these probes to keep. Add them as permanent tests.
    KeepProbes {
        paths: Vec<String>,
    },
}

/// One review pass. A change gets a standard pass, and then the passes its depth calls for (D94).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pass {
    /// Correctness against the task, the plan, and the rules.
    Standard,
    /// Runs once per task, when a trigger matched or the change is large.
    Security { hits: Vec<TriggerHit> },
    /// Runs once per task on a large change, to find what the change broke elsewhere.
    Regressions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PassKind {
    Standard,
    Security,
    Regressions,
}

impl Pass {
    pub fn kind(&self) -> PassKind {
        match self {
            Pass::Standard => PassKind::Standard,
            Pass::Security { .. } => PassKind::Security,
            Pass::Regressions => PassKind::Regressions,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReviewJob {
    pub pass: Pass,
    pub commit: Sha,
    /// What the reviewer diffs against. A standard pass uses the last reviewed commit, and every other
    /// pass uses the base.
    pub diff_base: Sha,
    pub depth: Depth,
}

/// A cause that can send a message back once per step. A second problem of the same cause pauses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Retry {
    InvalidReport,
    Lost,
    UncheckedEdits,
    VerdictMismatch,
}

impl From<Retry> for Trouble {
    fn from(retry: Retry) -> Trouble {
        match retry {
            Retry::InvalidReport => Trouble::InvalidReport,
            Retry::Lost => Trouble::Lost,
            Retry::UncheckedEdits => Trouble::UncheckedEdits,
            Retry::VerdictMismatch => Trouble::VerdictMismatch,
        }
    }
}

/// Why the message being sent is a correction. It picks the correction text and nothing else.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Note {
    InvalidReport {
        error: String,
    },
    UncheckedEdits,
    Lost {
        reason: String,
    },
    /// `findings` is what Kratos made of every finding the rejected report spoke to.
    VerdictMismatch {
        verdict: Verdict,
        findings: Vec<Finding>,
    },
}

impl Note {
    /// What the pause says when a second problem of the same cause stops the step.
    pub fn detail(&self) -> String {
        match self {
            Note::InvalidReport { error } => error.clone(),
            Note::UncheckedEdits => "A file changed after the last command.".to_owned(),
            Note::Lost { reason } => reason.clone(),
            Note::VerdictMismatch { verdict, findings } => {
                let blocking = findings.iter().filter(|f| f.is_open()).count();
                format!(
                    "The reviewer's verdict was {}, and Kratos confirmed {blocking} blocking findings.",
                    verdict.as_str()
                )
            }
        }
    }
}

/// A turn the provider stopped at a usage limit.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Stop {
    /// The message that was stopped. It is never sent again.
    pub message: MessageId,
    /// Whether the provider had received it. The scheduler decides what that allows.
    pub delivered: bool,
    /// The user chose to wait out a long or unknown wait (D41), so the scheduler should not ask again.
    pub user_accepted_wait: bool,
}

/// Who owns the next move of a step that cannot go on alone. The rules never resend the stopped message
/// and never run a timer. They hand the step to the one scheduler and observe what it decides.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Hold {
    /// The user approved moving the role to another account (D40). The engine tries that first.
    HandOff { stop: Stop },
    /// The scheduler has not been told yet. Until `ContinuationScheduled` or `ContinuationDeclined` is
    /// recorded, `next_step` keeps asking the engine to hand the step over, and the scheduler treats the
    /// same step and stopped message as one registration.
    ToScheduler { stop: Stop },
    /// The scheduler chose a continuation. Its message is the turn's `message`, and its result arrives
    /// as an ordinary `TurnEnded`.
    Scheduled {
        continuation: ContinuationId,
        resumes_at: Option<UnixMillis>,
    },
}

/// What the user or the planner settled for a step that had suspended, in the order it happened. It
/// goes in the step's next message.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    Answered {
        question: String,
        answer: String,
        by: Answerer,
    },
    PlanChanged {
        reason: String,
        allowed: bool,
        instructions: Option<String>,
    },
}

/// One logical step of agent work, stable from the fact that opened it until its result is folded.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Turn {
    pub step: StepId,
    /// Fixed when the step starts. A role change applies only between steps, so a step never changes hands.
    pub member: MemberId,
    pub work: Work,
    /// The message the step waits on. A result is accepted only for this one.
    pub message: MessageId,
    /// The message this one follows in the same session, for a correction or a resumed question.
    pub follows: Option<MessageId>,
    /// The fact that made `message` due. User messages queued before it are sent with it.
    pub due: FactSeq,
    pub retries: Vec<Retry>,
    pub note: Option<Note>,
    pub resolutions: Vec<Resolution>,
    /// Questions this step has asked so far.
    pub questions_asked: u32,
    pub hold: Option<Hold>,
}

impl Turn {
    /// Whether a result for `message` is the one this turn waits for.
    pub fn awaits(&self, message: &MessageId) -> bool {
        self.is_awaiting_result() && self.message == *message
    }

    /// A turn held for the scheduler or for a handoff waits for the engine, not for a result.
    pub fn is_awaiting_result(&self) -> bool {
        matches!(self.hold, None | Some(Hold::Scheduled { .. }))
    }
}

/// Everything the engine needs to deliver one message. It is a projection of a `Turn`, so the interface
/// does not expose retry budgets or holds. `prompts::message` turns it into text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Run {
    pub step: StepId,
    /// The engine derives the provider's durable message id from this and never sends it twice.
    pub message: MessageId,
    /// Set when this message follows another in the same step. A reviewer's correction goes to the
    /// session that wrote the report it corrects, and any other first message opens a new session.
    pub follows: Option<MessageId>,
    pub member: MemberId,
    pub role: Role,
    pub work: Work,
    /// Run the turn in the provider's read-only plan mode.
    pub plan_mode: bool,
    /// Set when this message corrects the previous one in the same step.
    pub note: Option<Note>,
    pub resolutions: Vec<Resolution>,
    /// User messages queued for this member before the message became due. They are sent with it.
    pub carried: Vec<Direction>,
}

/// A question that suspended a step.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Question {
    pub asker: MemberId,
    pub role: Role,
    /// What the asker had done so far, for whoever answers.
    pub summary: String,
    pub text: String,
}

/// Where a question goes. Implementer and reviewer questions go to the planner, or to the user when
/// there is none or the step has used its questions. A planner's own question goes to the user (D31).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Answerer {
    Planner,
    User,
}

/// A step set aside until something is answered or decided.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Suspended {
    pub turn: Turn,
    pub on: Pending,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pending {
    Question { question: Question },
    PlanChange { request: PlanChangeRequest },
}

// ---------------------------------------------------------------------------------------------
// The record.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Round {
    pub n: u32,
    pub commit: Sha,
    /// `None` when the user accepted the work as it was.
    pub report: Option<ImplementerDone>,
    pub files: Vec<String>,
    pub size: ChangeSize,
    pub reviews: Vec<Review>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Review {
    pub pass: PassKind,
    pub report: ReviewDone,
    /// False when the verdict disagreed with what Kratos confirmed. Its findings were not added.
    pub accepted: bool,
    pub checks: BTreeMap<FindingId, EvidenceCheck>,
    pub rechecks: BTreeMap<FindingId, Rerun>,
    pub hits: Option<Vec<TriggerHit>>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    pub id: FindingId,
    pub round: u32,
    pub pass: PassKind,
    pub report: FindingReport,
    pub status: FindingStatus,
}

/// A finding is in exactly one status.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FindingStatus {
    /// Confirmed, inside the changed lines, and blocking.
    Open {
        attempts: u32,
        response: Option<ResponseOutcome>,
        diagnosis: Option<String>,
    },
    Closed {
        how: ClosedHow,
    },
    /// Kratos could not confirm it. It is kept with the reason and never sent.
    Dropped {
        reason: DropReason,
    },
    /// The user reads it, and it never blocks.
    Listed {
        why: ListedWhy,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosedHow {
    Fixed,
    RuledNotBug,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListedWhy {
    Nit,
    PreExisting,
    Design,
    OutsideDiff,
}

/// A ruling on one disputed finding, kept with its reasons.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RulingRecord {
    pub finding: FindingId,
    pub verdict: RulingVerdict,
    pub reason: String,
    pub by: Answerer,
}

/// A reviewer's probe test and what became of it (D78).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Probe {
    pub path: String,
    pub content: Option<String>,
    pub truncated: bool,
    /// The finding whose report named this probe.
    pub finding: Option<FindingId>,
    /// The output that showed the bug.
    pub showed: Option<String>,
    pub status: ProbeStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeStatus {
    /// Waiting for a decision, the user's, or the implementer's when the user delegated it.
    Offered,
    Retained {
        as_path: String,
        evidence: String,
    },
    Rejected {
        reason: ProbeRejection,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeRejection {
    OutsideAllowedPaths,
    OutsideScope,
    /// It passed on the commit before the fix, so it proves nothing.
    PassedBeforeTheFix,
    /// It still fails after the fix.
    FailedAfterTheFix,
    UserDeclined,
    /// The user delegated the choice, and the implementer did not add it.
    NotAdded,
}

/// A message the user sent to one member.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Direction {
    pub delivery: DeliveryId,
    pub member: MemberId,
    /// The role the member held when the user wrote.
    pub role: Role,
    pub text: String,
    pub recorded_at: FactSeq,
    pub status: DirectionStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectionStatus {
    /// Recorded, with no disposition yet. The member's step cannot finish until there is one.
    Received,
    /// It reached the member's active turn.
    Steered,
    /// Kept for the member's next message.
    Queued,
    /// A turn that received it ended with a report.
    Delivered,
    /// The task ended first.
    Dropped,
}

// ---------------------------------------------------------------------------------------------
// Questions the other modules ask of the view.
// ---------------------------------------------------------------------------------------------

impl TaskView {
    /// A Team with a planner always plans first. Otherwise the stored setting decides (D28).
    pub fn plans_first(&self) -> bool {
        self.bindings.has_planner() || self.policy.plan_first
    }

    /// Who writes the plan. The planner does, or the implementer in a two-member Team.
    pub fn plan_author(&self) -> Role {
        if self.bindings.has_planner() {
            Role::Planner
        } else {
            Role::Implementer
        }
    }

    /// The scope in force. It is the approved plan's, or else the one the user gave at the start, with the
    /// user's amendments on either.
    pub fn scope(&self) -> Scope {
        let (source, base) = match &self.plan {
            PlanState::Approved { plan, .. } => (ScopeSource::ApprovedPlan, plan.scope.clone()),
            _ => (ScopeSource::TaskRequest, self.explicit_scope.clone()),
        };
        // An amendment widens a scope that names files. A scope that names none already covers all.
        let widened = if base.is_empty() {
            base
        } else {
            let extra = self
                .amendments
                .iter()
                .flat_map(|a| a.extra_scope.iter().cloned());
            base.into_iter().chain(extra).collect()
        };
        Scope {
            source,
            patterns: widened,
        }
    }

    /// Changed files no scope pattern covers. The review package lists them for the reviewer to judge.
    pub fn outside_scope(&self) -> Vec<String> {
        self.scope()
            .outside(self.changed_files.iter().map(String::as_str))
    }

    pub fn open_findings(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|finding| finding.is_open())
    }

    /// Every text a requirement or constraint quote may come from (D30): the request, the user's
    /// constraints, task requirements and member messages, the approved plan with its conditions and
    /// amendments, and the three rules files.
    pub fn quote_sources(&self) -> Vec<&str> {
        let plan = match &self.plan {
            PlanState::Approved {
                plan, conditions, ..
            } => plan
                .quotable()
                .chain(conditions.as_deref())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let amendments = self
            .amendments
            .iter()
            .flat_map(|a| a.instructions.as_deref());
        let rules = [
            &self.rules.agents_md,
            &self.rules.claude_md,
            &self.rules.review_md,
        ]
        .into_iter()
        .flat_map(|text| text.as_deref());
        std::iter::once(self.request.as_str())
            .chain(self.constraints.iter().map(String::as_str))
            .chain(self.requirements.iter().map(String::as_str))
            .chain(self.directions.iter().map(|d| d.text.as_str()))
            .chain(plan)
            .chain(amendments)
            .chain(rules)
            .collect()
    }

    /// How many more review rounds the task may run before it pauses at the cap. A round counts once
    /// its commit has been reviewed, so a commit that waits on a ruling, or that the user accepted as
    /// it was, uses none.
    pub fn rounds_left(&self) -> usize {
        let cap = usize::try_from(self.policy.round_cap.get()).unwrap_or(usize::MAX);
        let reviewed = self
            .rounds
            .iter()
            .skip(self.leg_start)
            .filter(|round| !round.reviews.is_empty())
            .count();
        cap.saturating_sub(reviewed)
    }

    pub fn pending_question(&self) -> Option<&Question> {
        match self.suspended.last() {
            Some(Suspended {
                on: Pending::Question { question },
                ..
            }) => Some(question),
            _ => None,
        }
    }

    pub fn pending_plan_change(&self) -> Option<&PlanChangeRequest> {
        match self.suspended.last() {
            Some(Suspended {
                on: Pending::PlanChange { request },
                ..
            }) => Some(request),
            _ => None,
        }
    }

    /// The provider of the member who holds `role`, for the branch name and for prompts.
    pub fn provider_of(&self, role: Role) -> Option<Provider> {
        self.bindings.member(role).map(|member| member.provider)
    }

    /// The commit the next round diffs from. It is the last round's, or the base.
    pub fn head(&self) -> &Sha {
        self.rounds.last().map_or(&self.base, |round| &round.commit)
    }

    pub fn is_user_decides_probes(&self) -> bool {
        matches!(self.policy.probes, ProbePolicy::UserDecides)
    }
}

impl Phase {
    /// A short stable name for refusal messages and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Phase::Active { activity } => activity.name(),
            Phase::Waiting { waiting } => waiting.name(),
            Phase::Closing { .. } => "closing",
            Phase::Closed { .. } => "closed",
        }
    }

    /// The message whose result the task waits for, if it waits for one.
    pub fn waiting_message(&self) -> Option<&MessageId> {
        match self {
            Phase::Active {
                activity: Activity::Turn { turn },
            } if turn.is_awaiting_result() => Some(&turn.message),
            _ => None,
        }
    }
}

impl Activity {
    pub fn name(&self) -> &'static str {
        match self {
            Activity::Preparing { .. } => "preparing",
            Activity::Turn { .. } => "working",
            Activity::Committing { .. } => "committing",
            Activity::Checking { .. } => "checking",
            Activity::Rebinding { .. } => "changing roles",
        }
    }

    /// The role whose job this activity is, for a failure to name.
    pub fn role(&self) -> Role {
        match self {
            Activity::Turn { turn } => turn.work.role(),
            Activity::Checking { .. } => Role::Reviewer,
            Activity::Rebinding { then, .. } => then.role(),
            Activity::Preparing { .. } | Activity::Committing { .. } => Role::Implementer,
        }
    }
}

impl Waiting {
    pub fn name(&self) -> &'static str {
        match self {
            Waiting::PlanApproval => "waiting for plan approval",
            Waiting::Answer => "waiting for an answer",
            Waiting::Ruling { .. } => "waiting for a ruling",
            Waiting::PlanChange => "waiting on a plan change",
            Waiting::Findings { .. } => "paused on findings",
            Waiting::Trouble { .. } => "paused in trouble",
            Waiting::UsageLimit { .. } => "paused at a usage limit",
            Waiting::Approved { .. } => "approved",
        }
    }
}

impl Finding {
    pub fn is_open(&self) -> bool {
        matches!(self.status, FindingStatus::Open { .. })
    }

    /// The order in which the user reads findings. Severity comes first, then the strength of the
    /// evidence, then the order they were found (D94).
    pub fn rank(&self) -> (Severity, Kind, FindingId) {
        (self.report.severity, self.report.kind, self.id)
    }
}
