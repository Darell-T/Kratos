//! Facts in, view out. `fold` replays a task's log into a `TaskView`. `TaskView::apply` adds one fact,
//! and `TaskView::accepts` asks whether a fact would apply without keeping the result.
//!
//! Three ideas hold the folding together.
//!
//! 1. One `Turn` for every piece of agent work. Planning, implementing, answering a question, ruling on
//!    a dispute, and reviewing all send a message, wait for one result, and can end with a finished
//!    report, an invalid one, a question, a blocked stop, a plan-change request, a lost turn, or a
//!    usage-limit stop. `turns.rs` handles those endings once for every role.
//! 2. Only `Waiting` needs the user. Every other phase has a next step that Kratos or an agent can take
//!    alone.
//! 3. A result is accepted only for the message the task is waiting on. That one check makes a
//!    duplicate, a late result after a cancel, and a result from a replaced step harmless.
//!
//! ```text
//! Preparing --Prepared--> Turn(Plan | Implement(Start))
//! Turn(Plan) --plan--> Waiting(PlanApproval) --approve--> Turn(Implement(Planned))
//! Turn(Implement) --done--> Committing --Committed--> Turn(Review) | Turn(Rule) | Waiting(Ruling)
//! Turn(Review) --done--> Checking --EvidenceChecked--> Turn(Implement(Findings)) | Turn(Review(next pass))
//!                                                      | Waiting(Findings) | Waiting(Approved)
//! any Turn --question--> Turn(Answer) on the planner | Waiting(Answer); the answer resumes the same step
//! any Turn --plan change--> Waiting(PlanChange); the decision resumes the same step or replans
//! any Turn --usage limit--> Waiting(UsageLimit) | Turn held for the scheduler; the scheduler owns the next move
//! any new step, with a role change pending --> Rebinding --RolesReassigned--> the step
//! ```

use std::collections::BTreeSet;

use crate::decisions::{self, DecisionKind};
use crate::facts::{
    DeliveryId, Fact, FactSeq, FailedStep, MessageId, Outcome, StepId, StoredFact, TaskStarted,
};
use crate::members::{MemberId, Role};
use crate::reports::ReportKind;
use crate::view::{
    Activity, Brief, DirectionStatus, FindingStatus, Phase, PlanState, TaskView, Trouble, Turn,
    Waiting, Work,
};

/// Why a fact does not apply to the task as it stands. In the engine's short transaction this is how a
/// stale result is recognized and dropped. In a stored log it means the record is damaged.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    #[error("a task's log is empty")]
    EmptyLog,
    #[error("a task's log starts with task_started")]
    FirstFactNotStart,
    #[error("a task's log has one task_started")]
    SecondStart,
    #[error("fact {got:?} does not follow fact {last:?}")]
    OutOfOrder { last: FactSeq, got: FactSeq },
    #[error("the task cannot start: {0}")]
    InvalidStart(&'static str),
    #[error("{fact} does not fit a task that is {phase}")]
    Misfit {
        fact: &'static str,
        phase: &'static str,
    },
    #[error(
        "the result for message {got:?} is not the one the task waits for ({waiting:?}): it is late, duplicated, or from a replaced step"
    )]
    Stale {
        got: MessageId,
        waiting: Option<MessageId>,
    },
    #[error(
        "member {member:?} has user messages with no recorded disposition ({deliveries:?}); record them before completing the step"
    )]
    UndisposedMessages {
        member: MemberId,
        deliveries: Vec<DeliveryId>,
    },
    #[error("delivery {0:?} is already recorded")]
    DuplicateDelivery(DeliveryId),
    #[error("delivery {0:?} was never recorded")]
    UnknownDelivery(DeliveryId),
    #[error("{0:?} is not a member of this Team")]
    UnknownMember(MemberId),
    #[error("this Team has no {0}")]
    NoSuchRole(Role),
    #[error("a {reported:?} report does not answer {work} work")]
    WrongReport {
        work: &'static str,
        reported: ReportKind,
    },
    #[error("{0}")]
    InvalidFact(String),
    #[error("a task that is {phase} does not accept {decision}")]
    NotAllowed {
        decision: DecisionKind,
        phase: &'static str,
    },
    #[error("{decision}: {reason}")]
    InvalidDecision {
        decision: DecisionKind,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("fact {seq:?} was refused: {rejection}")]
pub struct FoldError {
    pub seq: FactSeq,
    #[source]
    pub rejection: Rejection,
}

/// Replays a whole log. The first fact must be `task_started`.
pub fn fold(facts: &[StoredFact]) -> Result<TaskView, FoldError> {
    let Some((first, rest)) = facts.split_first() else {
        return Err(FoldError {
            seq: FactSeq(0),
            rejection: Rejection::EmptyLog,
        });
    };
    let Fact::TaskStarted { started } = &first.fact else {
        return Err(first.refused(Rejection::FirstFactNotStart));
    };
    let view = TaskView::start(started, first.seq).map_err(|rejection| first.refused(rejection))?;
    rest.iter()
        .try_fold(view, |view, stored| view.apply_boxed(stored.clone()))
        .map(|view| *view)
}

impl StoredFact {
    fn refused(&self, rejection: Rejection) -> FoldError {
        FoldError {
            seq: self.seq,
            rejection,
        }
    }
}

impl TaskView {
    /// Adds one fact. It takes the view, so a refusal leaves nothing half changed.
    pub fn apply(self, stored: StoredFact) -> Result<TaskView, FoldError> {
        Box::new(self).apply_boxed(stored).map(|view| *view)
    }

    /// `apply` for a view that is already on the heap. The reducers pass the box along, so a view of
    /// well over a kilobyte is not copied at every step of a fold.
    fn apply_boxed(mut self: Box<Self>, stored: StoredFact) -> Result<Box<TaskView>, FoldError> {
        let StoredFact { seq, fact } = stored;
        if seq <= self.seq {
            return Err(FoldError {
                seq,
                rejection: Rejection::OutOfOrder {
                    last: self.seq,
                    got: seq,
                },
            });
        }
        self.seq = seq;
        self.apply_fact(seq, fact)
            .map_err(|rejection| FoldError { seq, rejection })
    }

    /// Whether `fact` would apply right now. This is the engine's short-transaction check. It folds
    /// the latest facts, asks this, and appends the result only on `Ok`. `Rejection::Stale` means the
    /// execution was replaced or cancelled, or the result is a duplicate, and the engine drops it.
    pub fn accepts(&self, fact: &Fact) -> Result<(), Rejection> {
        let mut view = Box::new(self.clone());
        let seq = self.seq.next();
        view.seq = seq;
        view.apply_fact(seq, fact.clone()).map(drop)
    }

    pub(crate) fn start(started: &TaskStarted, seq: FactSeq) -> Result<Box<TaskView>, Rejection> {
        if [&started.request, &started.title, &started.branch]
            .iter()
            .any(|text| text.trim().is_empty())
        {
            return Err(Rejection::InvalidStart(
                "the request, the title, and the branch must not be blank",
            ));
        }
        let activity = Activity::Preparing { step: StepId(seq) };
        Ok(Box::new(TaskView {
            request: started.request.clone(),
            constraints: started.constraints.clone(),
            explicit_scope: started.scope.clone(),
            title: started.title.clone(),
            branch: started.branch.clone(),
            base: started.base.clone(),
            rules: started.rules.clone(),
            triggers: started.triggers.clone(),
            policy: started.policy.clone(),
            bindings: started.bindings.clone(),
            pending_roles: None,
            plan: PlanState::NoPlan,
            amendments: Vec::new(),
            requirements: Vec::new(),
            rounds: Vec::new(),
            findings: Vec::new(),
            rulings: Vec::new(),
            probes: Vec::new(),
            directions: Vec::new(),
            changed_files: BTreeSet::new(),
            dirty: false,
            leg_start: 0,
            last_reviewed: None,
            passes_done: BTreeSet::new(),
            suspended: Vec::new(),
            phase: Phase::Active { activity },
            seq,
        }))
    }

    // #lizard forgives: one arm for each of the 15 facts, each calling the reducer that applies it.
    pub(crate) fn apply_fact(
        self: Box<Self>,
        seq: FactSeq,
        fact: Fact,
    ) -> Result<Box<TaskView>, Rejection> {
        match fact {
            Fact::TaskStarted { .. } => Err(Rejection::SecondStart),
            Fact::Prepared => self.on_prepared(seq),
            Fact::TurnEnded { message, ended } => self.on_turn_ended(seq, message, *ended),
            Fact::Committed {
                commit,
                files,
                size,
                probe_runs,
            } => self.on_committed(seq, commit, files, size, probe_runs),
            Fact::EvidenceChecked { checked } => self.on_evidence_checked(seq, *checked),
            Fact::StepFailed { step, error } => self.on_step_failed(step, error),
            Fact::UserMessage {
                delivery,
                member,
                text,
            } => self.on_user_message(seq, delivery, member, text),
            Fact::MessageDisposed { delivery, how } => self.on_message_disposed(delivery, how),
            Fact::ContinuationScheduled {
                step,
                continuation,
                message,
                resumes_at,
            } => self.on_continuation_scheduled(step, continuation, message, resumes_at),
            Fact::ContinuationDeclined { step, why } => self.on_continuation_declined(step, why),
            Fact::AccountHandoff { step, outcome } => self.on_account_handoff(seq, step, outcome),
            Fact::RolesReassigned => self.on_roles_reassigned(seq),
            Fact::RoleChangeRejected { reason } => self.on_role_change_rejected(reason),
            Fact::UserDecided { decision } => decisions::apply_decision(self, decision, seq),
            Fact::Cleaned { .. } => self.on_cleaned(),
        }
    }

    pub(crate) fn misfit(&self, fact: &'static str) -> Rejection {
        Rejection::Misfit {
            fact,
            phase: self.phase.name(),
        }
    }

    pub(crate) fn active(mut self: Box<Self>, activity: Activity) -> Box<TaskView> {
        self.phase = Phase::Active { activity };
        self
    }

    pub(crate) fn waiting(mut self: Box<Self>, waiting: Waiting) -> Box<TaskView> {
        self.phase = Phase::Waiting { waiting };
        self
    }

    /// Plan first when the Team plans first, and otherwise start work.
    fn on_prepared(self: Box<Self>, seq: FactSeq) -> Result<Box<TaskView>, Rejection> {
        let Phase::Active {
            activity: Activity::Preparing { .. },
        } = &self.phase
        else {
            return Err(self.misfit("prepared"));
        };
        let work = self.first_work();
        self.begin_step(work, seq)
    }

    fn on_step_failed(
        self: Box<Self>,
        step: FailedStep,
        error: String,
    ) -> Result<Box<TaskView>, Rejection> {
        let left = match (&self.phase, step) {
            (
                Phase::Active {
                    activity: activity @ Activity::Preparing { .. },
                },
                FailedStep::Prepare,
            )
            | (
                Phase::Active {
                    activity: activity @ Activity::Committing { .. },
                },
                FailedStep::Commit,
            )
            | (
                Phase::Active {
                    activity: activity @ Activity::Checking { .. },
                },
                FailedStep::Check,
            ) => activity.clone(),
            _ => return Err(self.misfit("step_failed")),
        };
        let detail = format!("{}: {error}", step.as_str());
        Ok(self.pause_in_trouble(Trouble::System, left.role(), detail, left))
    }

    pub(crate) fn pause_in_trouble(
        self: Box<Self>,
        why: Trouble,
        role: Role,
        detail: String,
        left: Activity,
    ) -> Box<TaskView> {
        self.waiting(Waiting::Trouble {
            why,
            role,
            detail,
            left,
        })
    }

    fn on_cleaned(self: Box<Self>) -> Result<Box<TaskView>, Rejection> {
        match self.phase {
            Phase::Closing { outcome } => {
                let mut view = self;
                view.phase = Phase::Closed { outcome };
                Ok(view)
            }
            _ => Err(self.misfit("cleaned")),
        }
    }

    /// A cancel ends the task. Late results become stale because no turn waits, and the pending role
    /// change and the messages nobody received go with it.
    pub(crate) fn close(mut self: Box<Self>, outcome: Outcome) -> Box<TaskView> {
        self.suspended.clear();
        self.pending_roles = None;
        for direction in &mut self.directions {
            if matches!(
                direction.status,
                DirectionStatus::Received | DirectionStatus::Queued | DirectionStatus::Steered
            ) {
                direction.status = DirectionStatus::Dropped;
            }
        }
        self.phase = Phase::Closing { outcome };
        self
    }

    /// The same step again with a fresh retry budget and a new message, used by a resume. The turn
    /// keeps the note that paused it and its resolutions.
    pub(crate) fn retake(self: Box<Self>, activity: Activity, seq: FactSeq) -> Box<TaskView> {
        let activity = match activity {
            Activity::Turn { mut turn } => {
                turn.retries.clear();
                turn.follows = None;
                turn.hold = None;
                turn.message = MessageId::due_at(seq);
                turn.due = seq;
                Activity::Turn { turn }
            }
            other => other,
        };
        self.active(activity)
    }

    /// An explicit request to go on starts a new count of rounds and of each open finding's fix
    /// attempts (D89). Nothing else does.
    pub(crate) fn new_leg(mut self: Box<Self>) -> Box<TaskView> {
        self.leg_start = self.rounds.len();
        self.reset_attempts()
    }

    pub(crate) fn reset_attempts(mut self: Box<Self>) -> Box<TaskView> {
        for finding in &mut self.findings {
            if let FindingStatus::Open { attempts, .. } = &mut finding.status {
                *attempts = 0;
            }
        }
        self
    }

    /// The step a task starts with once it is prepared.
    fn first_work(&self) -> Work {
        if self.plans_first() {
            Work::Plan {
                author: self.plan_author(),
                feedback: None,
            }
        } else {
            Work::Implement {
                brief: Brief::Start,
            }
        }
    }

    /// Starts a new logical step. This is the role-change checkpoint. With a change pending and
    /// nothing suspended, the step waits behind `Activity::Rebinding`. Otherwise it becomes a `Turn`
    /// held by the role's current member.
    pub(crate) fn begin_step(
        self: Box<Self>,
        work: Work,
        seq: FactSeq,
    ) -> Result<Box<TaskView>, Rejection> {
        match self.pending_roles.clone() {
            Some(to) if self.suspended.is_empty() => Ok(self.active(Activity::Rebinding {
                step: StepId(seq),
                to,
                then: work,
            })),
            _ => self.start_turn(work, StepId(seq), seq),
        }
    }

    /// Hands `work` to the member who holds its role. The message is due at `seq`, and `step` names the
    /// logical step, which is older than `seq` when a role change held the step back.
    pub(crate) fn start_turn(
        self: Box<Self>,
        work: Work,
        step: StepId,
        seq: FactSeq,
    ) -> Result<Box<TaskView>, Rejection> {
        let role = work.role();
        let member = self
            .bindings
            .member(role)
            .ok_or(Rejection::NoSuchRole(role))?
            .id
            .clone();
        let turn = Turn {
            step,
            member,
            work,
            message: MessageId::due_at(seq),
            follows: None,
            due: seq,
            retries: Vec::new(),
            note: None,
            resolutions: Vec::new(),
            questions_asked: 0,
            hold: None,
        };
        Ok(self.active(Activity::Turn { turn }))
    }

    /// `bindings = to`, and the deferred step starts under the new roles. The new holder gets fresh fix
    /// attempts. Rounds, findings, probes, messages, and finished evidence all stay.
    fn on_roles_reassigned(mut self: Box<Self>, seq: FactSeq) -> Result<Box<TaskView>, Rejection> {
        let Phase::Active {
            activity: Activity::Rebinding { to, then, step },
        } = self.phase.clone()
        else {
            return Err(self.misfit("roles_reassigned"));
        };
        self.bindings = to;
        self.pending_roles = None;
        self.reset_attempts().start_turn(then, step, seq)
    }

    /// Nothing changed. The deferred step starts under the old roles, and the task pauses as
    /// `Trouble::RoleChange` so that the user sees why. A resume goes on with the old roles.
    fn on_role_change_rejected(
        mut self: Box<Self>,
        reason: String,
    ) -> Result<Box<TaskView>, Rejection> {
        let Phase::Active {
            activity: Activity::Rebinding { then, step, .. },
        } = self.phase.clone()
        else {
            return Err(self.misfit("role_change_rejected"));
        };
        self.pending_roles = None;
        let role = then.role();
        let held = self.start_turn(then, step, step.0)?;
        let Phase::Active { activity: left } = held.phase.clone() else {
            return Err(held.misfit("role_change_rejected"));
        };
        Ok(held.pause_in_trouble(Trouble::RoleChange, role, reason, left))
    }
}
