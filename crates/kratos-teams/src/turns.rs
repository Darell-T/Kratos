//! How a turn ends, written once for every role: a lost turn, a usage-limit stop, and a report that is
//! invalid, blocked, a question, a plan-change request, or finished.
//!
//! A turn is one logical step of agent work. Its `StepId` stays the same through a correction, a
//! question and its answer, and a usage-limit stop and its continuation. Only its `MessageId` changes,
//! because each message is sent once.

use kratos_protocol::TurnChanges;

use crate::facts::{
    ContinuationId, Declined, Ended, FactSeq, HandoffOutcome, MessageId, Outcome, StepId,
    UnixMillis,
};
use crate::fold::Rejection;
use crate::members::Role;
use crate::reports::{AnswerDone, ImplementerDone, Report, Reported, ReviewDone};
use crate::task_plan::{PlanChangeRequest, TaskPlan};
use crate::view::{
    Activity, AfterCommit, Answerer, Hold, MAX_QUESTIONS_PER_STEP, Note, Pending, Phase, PlanState,
    Question, Resolution, Retry, Stop, Suspended, TaskView, Trouble, Turn, UsageLimitAsk, Waiting,
    Work,
};

/// What a kind of work does with its own finished report.
type Finish<T> = fn(Box<TaskView>, FactSeq, Turn, T) -> Result<Box<TaskView>, Rejection>;

impl TaskView {
    /// Takes the turn a result is for out of the view, or refuses the result as stale. A turn held for
    /// the scheduler or for a handoff waits for the engine and not for a result, so a late result for
    /// the message that was stopped is stale too.
    pub(crate) fn take_waiting_turn(
        mut self: Box<Self>,
        message: &MessageId,
    ) -> Result<(Box<TaskView>, Turn), Rejection> {
        let phase = std::mem::replace(
            &mut self.phase,
            Phase::Closed {
                outcome: Outcome::Cancelled,
            },
        );
        match phase {
            Phase::Active {
                activity: Activity::Turn { turn },
            } if turn.awaits(message) => Ok((self, turn)),
            other => {
                let waiting = other.waiting_message().cloned();
                self.phase = other;
                Err(Rejection::Stale {
                    got: message.clone(),
                    waiting,
                })
            }
        }
    }

    pub(crate) fn on_turn_ended(
        self: Box<Self>,
        seq: FactSeq,
        message: MessageId,
        ended: Ended,
    ) -> Result<Box<TaskView>, Rejection> {
        let (mut view, turn) = self.take_waiting_turn(&message)?;
        match ended {
            Ended::Lost { reason } => Ok(view.turn_lost(turn, reason, seq)),
            Ended::QuotaLimit { delivered } => Ok(view.hold_turn(turn, delivered)),
            Ended::Reported { reported } => {
                view.refuse_undisposed(&turn.member)?;
                view.deliver_to(&turn);
                view.on_reported(seq, turn, reported)
            }
        }
    }

    /// A report is valid only for the work it answers.
    fn on_reported(
        mut self: Box<Self>,
        seq: FactSeq,
        turn: Turn,
        reported: Reported,
    ) -> Result<Box<TaskView>, Rejection> {
        match (&turn.work, reported) {
            (Work::Plan { .. }, Reported::Plan { report }) => {
                self.settle(seq, turn, report, TaskView::on_plan_done)
            }
            (Work::Implement { .. }, Reported::Implementer { report, changes }) => {
                // Every outcome of an implementer turn changes what Kratos knows about unchecked edits.
                self.dirty = fold_dirty(self.dirty, &changes);
                self.settle(seq, turn, report, TaskView::on_implementer_done)
            }
            (Work::Review { .. }, Reported::Review { report }) => {
                self.settle(seq, turn, report, TaskView::on_review_done)
            }
            (Work::Answer { .. }, Reported::Answer { report }) => {
                self.settle(seq, turn, report, TaskView::on_answer_done)
            }
            (Work::Rule { .. }, Reported::Rulings { report }) => {
                self.settle(seq, turn, report, TaskView::on_rulings_done)
            }
            (work, reported) => Err(Rejection::WrongReport {
                work: work.name(),
                reported: reported.kind(),
            }),
        }
    }

    /// The outcomes every kind of work shares. Only a finished report is the work's own business.
    fn settle<T>(
        self: Box<Self>,
        seq: FactSeq,
        mut turn: Turn,
        report: Report<T>,
        finished: Finish<T>,
    ) -> Result<Box<TaskView>, Rejection> {
        match report {
            Report::Invalid { error, .. } => Ok(self.retry_turn(
                turn,
                Retry::InvalidReport,
                Note::InvalidReport { error },
                seq,
            )),
            Report::Blocked { summary } => {
                turn.note = None;
                Ok(self.pause_turn(Trouble::Blocked, turn, summary))
            }
            Report::Question { summary, question } => self.ask(seq, turn, summary, question),
            Report::PlanChange { request, .. } => Ok(self.request_plan_change(turn, request)),
            Report::Finished { done } => finished(self, seq, turn, done),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Corrections and trouble.
    // -----------------------------------------------------------------------------------------

    /// Sends the step's next message with the same step, a new message id, and a correction note. The
    /// first time a cause occurs it is retried, and the second time the task pauses.
    pub(crate) fn retry_turn(
        self: Box<Self>,
        mut turn: Turn,
        cause: Retry,
        note: Note,
        seq: FactSeq,
    ) -> Box<TaskView> {
        if turn.retries.contains(&cause) {
            let detail = note.detail();
            // A reviewer is asked for a fresh review, so it keeps no correction. Everyone else is
            // asked again with the same note.
            turn.note = (turn.work.role() != Role::Reviewer).then_some(note);
            return self.pause_turn(cause.into(), turn, detail);
        }
        turn.retries.push(cause);
        turn.follows = (cause != Retry::Lost).then(|| turn.message.clone());
        turn.note = Some(note);
        turn.message = MessageId::due_at(seq);
        turn.due = seq;
        self.active(Activity::Turn { turn })
    }

    /// Pauses with the turn to run again in `left`.
    pub(crate) fn pause_turn(
        self: Box<Self>,
        why: Trouble,
        turn: Turn,
        detail: String,
    ) -> Box<TaskView> {
        let role = turn.work.role();
        self.pause_in_trouble(why, role, detail, Activity::Turn { turn })
    }

    fn turn_lost(mut self: Box<Self>, turn: Turn, reason: String, seq: FactSeq) -> Box<TaskView> {
        if matches!(turn.work, Work::Implement { .. }) {
            // A lost turn's change record is gone, so assume an unchecked edit.
            self.dirty = true;
        }
        self.requeue_steered(&turn.member);
        self.retry_turn(turn, Retry::Lost, Note::Lost { reason }, seq)
    }

    // -----------------------------------------------------------------------------------------
    // Usage limits (D39 to D41). The rules never resend a message and never run a timer.
    // -----------------------------------------------------------------------------------------

    /// The provider stopped the turn at a usage limit. The step keeps its identity and goes to the one
    /// scheduler. With account handoff on, the user is asked first, every time (D40).
    fn hold_turn(self: Box<Self>, mut turn: Turn, delivered: bool) -> Box<TaskView> {
        turn.hold = Some(Hold::ToScheduler {
            stop: Stop {
                message: turn.message.clone(),
                delivered,
                user_accepted_wait: false,
            },
        });
        if self.policy.account_handoff {
            let ask = UsageLimitAsk::Handoff;
            return self.waiting(Waiting::UsageLimit { turn, ask });
        }
        self.active(Activity::Turn { turn })
    }

    /// The active turn with this step id. The caller checks that the turn has the hold the fact needs.
    fn held_turn(&self, step: StepId, fact: &'static str) -> Result<Turn, Rejection> {
        match &self.phase {
            Phase::Active {
                activity: Activity::Turn { turn },
            } if turn.step == step => Ok(turn.clone()),
            _ => Err(self.misfit(fact)),
        }
    }

    /// The scheduler took the stopped step. The turn's message becomes the continuation's. The old
    /// message is never sent again.
    pub(crate) fn on_continuation_scheduled(
        self: Box<Self>,
        step: StepId,
        continuation: ContinuationId,
        message: MessageId,
        resumes_at: Option<UnixMillis>,
    ) -> Result<Box<TaskView>, Rejection> {
        let mut turn = self.held_turn(step, "continuation_scheduled")?;
        let Some(Hold::ToScheduler { stop }) = &turn.hold else {
            return Err(self.misfit("continuation_scheduled"));
        };
        if stop.message == message {
            return Err(Rejection::InvalidFact(format!(
                "the continuation must have a message id of its own, and not {message:?}, the one that was stopped"
            )));
        }
        turn.message = message;
        turn.hold = Some(Hold::Scheduled {
            continuation,
            resumes_at,
        });
        Ok(self.active(Activity::Turn { turn }))
    }

    /// The scheduler will not resume by itself, so the user decides (D41).
    pub(crate) fn on_continuation_declined(
        self: Box<Self>,
        step: StepId,
        why: Declined,
    ) -> Result<Box<TaskView>, Rejection> {
        let turn = self.held_turn(step, "continuation_declined")?;
        if !matches!(turn.hold, Some(Hold::ToScheduler { .. })) {
            return Err(self.misfit("continuation_declined"));
        }
        let ask = UsageLimitAsk::LongWait { why };
        Ok(self.waiting(Waiting::UsageLimit { turn, ask }))
    }

    /// `HandedOff` sends the step again as a fresh session. `Unavailable` goes on to the scheduler.
    pub(crate) fn on_account_handoff(
        self: Box<Self>,
        seq: FactSeq,
        step: StepId,
        outcome: HandoffOutcome,
    ) -> Result<Box<TaskView>, Rejection> {
        let mut turn = self.held_turn(step, "account_handoff")?;
        let Some(Hold::HandOff { stop }) = turn.hold.take() else {
            return Err(self.misfit("account_handoff"));
        };
        match outcome {
            HandoffOutcome::HandedOff => {
                turn.message = MessageId::due_at(seq);
                turn.due = seq;
                turn.follows = None;
            }
            HandoffOutcome::Unavailable { .. } => turn.hold = Some(Hold::ToScheduler { stop }),
        }
        Ok(self.active(Activity::Turn { turn }))
    }

    // -----------------------------------------------------------------------------------------
    // Questions (D31, D90, D91) and plan-change requests (D32).
    // -----------------------------------------------------------------------------------------

    /// Suspends `turn` on a question and routes it. An implementer or reviewer question goes to the
    /// planner, or to the user when there is no planner or the step used its questions.
    /// A planner's own question goes to the user, because a planner never guesses (D91).
    fn ask(
        mut self: Box<Self>,
        seq: FactSeq,
        mut turn: Turn,
        summary: String,
        text: String,
    ) -> Result<Box<TaskView>, Rejection> {
        let role = turn.work.role();
        let question = Question {
            asker: turn.member.clone(),
            role,
            summary,
            text,
        };
        let to_planner = self.bindings.has_planner()
            && role != Role::Planner
            && turn.questions_asked < MAX_QUESTIONS_PER_STEP;
        turn.questions_asked += 1;
        self.suspended.push(Suspended {
            turn,
            on: Pending::Question {
                question: question.clone(),
            },
        });
        if to_planner {
            return self.begin_step(Work::Answer { question }, seq);
        }
        Ok(self.waiting(Waiting::Answer))
    }

    /// A plan-change request always pauses for the user, even with a planner.
    fn request_plan_change(
        mut self: Box<Self>,
        turn: Turn,
        request: PlanChangeRequest,
    ) -> Box<TaskView> {
        self.suspended.push(Suspended {
            turn,
            on: Pending::PlanChange { request },
        });
        self.waiting(Waiting::PlanChange)
    }

    /// Pops the top suspended step and sends it its next message with `resolution` attached. The step
    /// keeps its `StepId`, gets a new message id, and carries no note. It does not pass through the role-change checkpoint.
    pub(crate) fn resume_step(
        mut self: Box<Self>,
        resolution: Resolution,
        seq: FactSeq,
    ) -> Result<Box<TaskView>, Rejection> {
        let Some(Suspended { mut turn, .. }) = self.suspended.pop() else {
            return Err(self.misfit("an answer"));
        };
        turn.resolutions.push(resolution);
        turn.follows = Some(turn.message.clone());
        turn.message = MessageId::due_at(seq);
        turn.due = seq;
        turn.note = None;
        turn.hold = None;
        Ok(self.active(Activity::Turn { turn }))
    }

    /// The planner answered. The step that asked resumes with the answer.
    fn on_answer_done(
        self: Box<Self>,
        seq: FactSeq,
        turn: Turn,
        answer: AnswerDone,
    ) -> Result<Box<TaskView>, Rejection> {
        let Work::Answer { question } = turn.work else {
            return Err(self.misfit("an answer"));
        };
        let resolution = Resolution::Answered {
            question: question.text,
            answer: answer.answer,
            by: Answerer::Planner,
        };
        self.resume_step(resolution, seq)
    }

    // -----------------------------------------------------------------------------------------
    // Finished work that is not a review or a commit.
    // -----------------------------------------------------------------------------------------

    /// The plan is written and waits for the user's approval (D37).
    fn on_plan_done(
        mut self: Box<Self>,
        _seq: FactSeq,
        turn: Turn,
        plan: TaskPlan,
    ) -> Result<Box<TaskView>, Rejection> {
        let revision = match &self.plan {
            PlanState::NoPlan => 1,
            PlanState::Drafted { revision, .. } | PlanState::Approved { revision, .. } => {
                revision + 1
            }
        };
        self.plan = PlanState::Drafted {
            plan,
            author: turn.work.role(),
            revision,
        };
        Ok(self.waiting(Waiting::PlanApproval))
    }

    /// The handoff check runs in this order. The responses cover the open findings, the probes are
    /// allowed, and no file changed after the last command. A problem goes back once and then pauses.
    fn on_implementer_done(
        self: Box<Self>,
        seq: FactSeq,
        turn: Turn,
        done: ImplementerDone,
    ) -> Result<Box<TaskView>, Rejection> {
        if let Some(error) = self
            .responses_problem(&done.responses)
            .or_else(|| self.probes_problem(&turn.work, &done))
        {
            return Ok(self.retry_turn(
                turn,
                Retry::InvalidReport,
                Note::InvalidReport { error },
                seq,
            ));
        }
        if self.dirty {
            return Ok(self.retry_turn(turn, Retry::UncheckedEdits, Note::UncheckedEdits, seq));
        }
        Ok(self.active(Activity::Committing {
            step: turn.step,
            then: AfterCommit::Review { report: done },
        }))
    }

    /// The reviewer finished. Evidence is checked before anything is believed.
    fn on_review_done(
        self: Box<Self>,
        _seq: FactSeq,
        turn: Turn,
        report: ReviewDone,
    ) -> Result<Box<TaskView>, Rejection> {
        let Work::Review { job } = turn.work.clone() else {
            return Err(self.misfit("a review"));
        };
        Ok(self.active(Activity::Checking {
            step: turn.step,
            turn,
            job,
            report,
        }))
    }
}

/// A turn that ran a command reports its own state. One that ran none cannot clear an earlier
/// unchecked edit.
fn fold_dirty(dirty: bool, changes: &TurnChanges) -> bool {
    if changes.ran_command {
        changes.edited_since_command
    } else {
        dirty || changes.edited_since_command
    }
}
