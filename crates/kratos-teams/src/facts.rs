//! The recorded vocabulary. It holds the ids, settings, and facts the engine appends to a task's log.
//!
//! A fact says what happened. It never says what to do next. `fold` turns facts into a view, and
//! `next_step` turns the view into the one thing that must happen next. Every id, time, and outcome the
//! rules use arrives inside a fact, so replaying a log gives the same answer on any machine on any day.
//! No fact carries a time the rules compare. The one time a fact carries, `resumes_at`, is only shown
//! to the user.
//!
//! Serde conventions for the whole crate. Enums with data are internally tagged with `kind` and use
//! struct variants, so a log line reads as one flat object per level and two tags never collide.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;

use kratos_protocol::Permission;
use serde::{Deserialize, Serialize};

use crate::decisions::Decision;
use crate::evidence::{EvidenceCheck, ProbeCapture, Rerun};
use crate::members::{Bindings, MemberId};
use crate::reports::{FindingId, Reported};
use crate::security_triggers::{Trigger, TriggerHit};
use crate::task_plan::{ProbeConstraints, ScopePattern};

// ---------------------------------------------------------------------------------------------
// Ids. Each is its own type, so a message id cannot go where a step id is wanted.
// ---------------------------------------------------------------------------------------------

/// A fact's position in its task's log. The engine's database assigns it, counting up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FactSeq(pub u64);

impl FactSeq {
    pub fn next(self) -> FactSeq {
        FactSeq(self.0 + 1)
    }
}

/// One logical unit of work: "plan", "implement round 2", "review this commit", "answer this question".
///
/// It is derived and never allocated. It is the position of the fact that opened the step, so the same
/// log always yields the same ids. It survives everything that happens inside the step: a correction, a
/// question and its answer, a usage-limit stop and its continuation. It changes only when the step is
/// replaced on purpose, by a resume, a direction, or a step-back, and that is also what makes a late
/// result from the replaced step recognizable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepId(pub FactSeq);

/// Names one message to one member, and so one expected result. The engine derives the provider's
/// message id from it and never sends the same `MessageId` twice.
///
/// A message the rules make looks like `m12`, the position of the fact that made it due. A continuation
/// after a usage limit carries the id the usage scheduler chose, which arrives in a fact.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(String);

impl MessageId {
    pub fn due_at(seq: FactSeq) -> MessageId {
        MessageId(format!("m{}", seq.0))
    }

    pub fn new(id: impl Into<String>) -> MessageId {
        MessageId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Names one message from the user so that a retried delivery is recognized. The engine chooses it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeliveryId(String);

impl DeliveryId {
    pub fn new(id: impl Into<String>) -> DeliveryId {
        DeliveryId(id.into())
    }
}

/// Names one automatic continuation of a stopped turn. The usage scheduler chooses it, and the rules
/// only record it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContinuationId(String);

impl ContinuationId {
    pub fn new(id: impl Into<String>) -> ContinuationId {
        ContinuationId(id.into())
    }
}

/// A git object id: 40 or 64 lowercase hex digits, checked when parsed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha(String);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a git object id (40 or 64 lowercase hex digits)")]
pub struct ShaError(pub String);

impl Sha {
    pub fn parse(text: &str) -> Result<Sha, ShaError> {
        let is_hex = text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if is_hex && matches!(text.len(), 40 | 64) {
            Ok(Sha(text.to_owned()))
        } else {
            Err(ShaError(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Sha {
    type Error = ShaError;

    fn try_from(text: String) -> Result<Sha, ShaError> {
        Sha::parse(&text)
    }
}

impl From<Sha> for String {
    fn from(sha: Sha) -> String {
        sha.0
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Milliseconds since the Unix epoch, as a time that arrived in a fact. It is shown to the user and
/// never compared by the rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMillis(pub u64);

// ---------------------------------------------------------------------------------------------
// The task's frozen starting point.
// ---------------------------------------------------------------------------------------------

/// `AGENTS.md`, `CLAUDE.md`, and `REVIEW.md` as they were in the Team's folder when the task started.
/// The review package, the quote check, and the security triggers all read this copy, so the
/// implementer cannot weaken the standard it is judged by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesSnapshot {
    pub agents_md: Option<String>,
    pub claude_md: Option<String>,
    pub review_md: Option<String>,
}

/// Who decides which of the reviewer's probe tests become permanent tests (D78).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbePolicy {
    /// The default. The user picks at approval, and the implementer then adds the picks.
    #[default]
    UserDecides,
    /// The user handed the choice to the implementer, inside these recorded constraints.
    Delegated { constraints: ProbeConstraints },
}

/// How many review rounds a task runs before it pauses at the cap. Only the user's explicit request
/// adds rounds (D89).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoundCap(NonZeroU32);

impl RoundCap {
    pub fn new(rounds: u32) -> Option<RoundCap> {
        NonZeroU32::new(rounds).map(RoundCap)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl Default for RoundCap {
    fn default() -> RoundCap {
        const THREE: NonZeroU32 = NonZeroU32::new(3).unwrap();
        RoundCap(THREE)
    }
}

/// What counts as a large change, which gets the deeper review (D94). Either limit reached makes it
/// large. These are proposed numbers that the user can change per Team.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeChange {
    pub files: u32,
    pub lines: u32,
}

impl Default for LargeChange {
    fn default() -> LargeChange {
        LargeChange {
            files: 10,
            lines: 400,
        }
    }
}

/// The size of the change one review reads, as files touched and lines added and removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSize {
    pub files: u32,
    pub lines: u32,
}

/// How deep the reviews of a round go (D94).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Depth {
    /// One pass. It is the standard review, then the security pass when a trigger matched.
    Single,
    /// Passes for correctness, security, and regressions, each from a fresh reviewer session.
    Thorough,
}

impl LargeChange {
    pub fn depth_of(self, size: ChangeSize) -> Depth {
        if size.files >= self.files || size.lines >= self.lines {
            Depth::Thorough
        } else {
            Depth::Single
        }
    }
}

fn is_true() -> bool {
    true
}

/// The Team settings this task runs under, frozen when it started so that a later settings change
/// cannot alter a task in flight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPolicy {
    /// Two-member Teams only, and off by default. A Team with a planner always plans first, whatever
    /// this says. Use `TaskView::plans_first`, never this field alone.
    #[serde(default)]
    pub plan_first: bool,
    #[serde(default = "is_true")]
    pub security_pass: bool,
    #[serde(default)]
    pub probes: ProbePolicy,
    /// D40. Off by default. A usage limit never offers to move a role to another account unless this
    /// is true, and even then each handoff needs the user's approval.
    #[serde(default)]
    pub account_handoff: bool,
    #[serde(default)]
    pub round_cap: RoundCap,
    #[serde(default)]
    pub large_change: LargeChange,
    /// The implementer's permission. The planner and the reviewer have fixed access (D84, D93).
    pub permission: Permission,
}

/// Everything that is true when a task starts. The engine builds it once the folder, identity, sign-ins,
/// and sandbox are checked, and a bad `kratos-security` block has already refused the task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStarted {
    /// The user's task text. The reviewer checks the work against it.
    pub request: String,
    /// Constraints the user gave at the start, in their own words.
    pub constraints: Vec<String>,
    /// The files the user said the task may touch. Empty means the whole worktree.
    pub scope: Vec<ScopePattern>,
    /// The first line of the request, from `prompts::task_title`. It is stored and not derived again, so
    /// a later version of that function cannot rename the commits of a task in flight.
    pub title: String,
    /// From `prompts::branch_name`, which starts it with the implementer's provider (D36).
    pub branch: String,
    pub base: Sha,
    pub rules: RulesSnapshot,
    /// Parsed once from `rules.review_md`, or the defaults, so that a later change to the defaults
    /// cannot change what an old task scans for.
    pub triggers: Vec<Trigger>,
    pub policy: TaskPolicy,
    pub bindings: Bindings,
}

// ---------------------------------------------------------------------------------------------
// Facts.
// ---------------------------------------------------------------------------------------------

/// A fact with the position the engine's database gave it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredFact {
    pub seq: FactSeq,
    pub fact: Fact,
}

/// Everything that can happen to a task. The first fact of a log is always `TaskStarted`.
///
/// A result fact names the message it answers. The rules accept a result only for the message the task
/// is waiting on, so a duplicate, a late result after a cancel, and a result from a replaced step are
/// all recognized and dropped, and no lock is held across the turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact {
    TaskStarted {
        started: Box<TaskStarted>,
    },
    /// The worktree, the branch, and the sessions exist.
    Prepared,
    /// A member's turn ended. `ended` says how.
    TurnEnded {
        message: MessageId,
        ended: Box<Ended>,
    },
    /// Kratos committed the work. `files` are the paths this commit changed since the previous round's
    /// commit, or since the base, from `git diff --name-only`. `size` measures what the commit's
    /// standard review reads, from `CommitStep::review_base`. `probe_runs` has one entry for each probe
    /// the implementer's report added.
    Committed {
        commit: Sha,
        files: Vec<String>,
        size: ChangeSize,
        probe_runs: Vec<ProbeRun>,
    },
    /// Kratos finished checking a review's findings, rerunning open bugs, and scanning for triggers.
    EvidenceChecked {
        checked: Box<EvidenceChecked>,
    },
    /// A Kratos step, not an agent turn, failed. A git error is one.
    StepFailed {
        step: FailedStep,
        error: String,
    },
    /// The user sent a message to one member. The engine records it before it tries to deliver it.
    UserMessage {
        delivery: DeliveryId,
        member: MemberId,
        text: String,
    },
    /// What the engine did with a user message. It either reached the active turn or is kept for the next one.
    MessageDisposed {
        delivery: DeliveryId,
        how: Disposition,
    },
    /// The usage scheduler took over a stopped step and chose the message it will send.
    ContinuationScheduled {
        step: StepId,
        continuation: ContinuationId,
        message: MessageId,
        resumes_at: Option<UnixMillis>,
    },
    /// The scheduler will not resume by itself, and the user must decide (D41).
    ContinuationDeclined {
        step: StepId,
        why: Declined,
    },
    /// The engine tried to hand the stopped role to another account, after the user approved it (D40).
    AccountHandoff {
        step: StepId,
        outcome: HandoffOutcome,
    },
    /// The engine validated and applied the pending role change.
    RolesReassigned,
    /// The engine could not apply the pending role change. Nothing changed.
    RoleChangeRejected {
        reason: String,
    },
    UserDecided {
        decision: Decision,
    },
    /// Cleanup ran. `leftover` names anything a file lock kept.
    Cleaned {
        leftover: Vec<String>,
    },
}

/// How a turn ended. A turn that produced a report, one that produced nothing, and one that hit a usage
/// limit are three kinds of trouble with three remedies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ended {
    /// A final reply, parsed at the boundary into a typed report or an actionable error.
    Reported { reported: Reported },
    /// The turn produced nothing because the process exited, the deadline passed, or a restart cut it off.
    Lost { reason: String },
    /// The provider stopped the turn at a usage limit. `delivered` says whether the message had
    /// already reached the provider, which decides what the scheduler may send again. The rules never
    /// resend a message and never start a timer.
    QuotaLimit { delivered: bool },
}

/// Kratos's checks of one review, recorded as one fact so a restart never repeats half of them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EvidenceChecked {
    /// One entry for each finding that could block. Nits, pre-existing bugs, and design opinions are
    /// listed without a check.
    pub checks: BTreeMap<FindingId, EvidenceCheck>,
    /// Kratos's rerun of each open bug's command on the new commit.
    pub rechecks: BTreeMap<FindingId, Rerun>,
    /// The reviewer's untracked probe files, copied before anything can delete them.
    pub probes: Vec<ProbeCapture>,
    /// `None` when no scan ran. `Some` with an empty list means the scan matched nothing.
    pub security_hits: Option<Vec<TriggerHit>>,
}

/// What git showed about one probe the implementer added (D78). The rules decide from it whether the
/// probe stays, so the engine cannot keep one the user's constraints refuse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRun {
    /// The reviewer's file, as `AddedProbe::from` names it.
    pub from: String,
    /// The probe failed on the commit before the fix.
    pub failed_before_fix: bool,
    /// The probe passes on the new commit.
    pub passed_after_fix: bool,
    /// The output that showed the bug before the fix.
    pub evidence: String,
}

/// The Kratos steps that can fail without being an agent's fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedStep {
    Prepare,
    Commit,
    Check,
}

impl FailedStep {
    pub fn as_str(self) -> &'static str {
        match self {
            FailedStep::Prepare => "prepare",
            FailedStep::Commit => "commit",
            FailedStep::Check => "check",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// It reached the member's active turn.
    Steered,
    /// There was no active turn to reach. The member's next message carries it.
    Queued,
}

/// Why the scheduler handed the decision to the user. R3 owns the cutoff and the evidence, and the
/// rules do not know the number of hours (D41).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Declined {
    TooLong,
    NoResetTime,
    NoRecoveryEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandoffOutcome {
    /// The role now runs on another account. The step is sent again, as a fresh session.
    HandedOff,
    /// No other account could take it. The step goes on to the scheduler and waits for the reset.
    Unavailable { reason: String },
}

/// How a task ended. `Cancelled` is reached only through the user's cancel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Merged,
    Rejected,
    Cancelled,
}
