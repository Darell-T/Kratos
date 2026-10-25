//! Approvals and questions: requests from an agent that wait on the user.
//!
//! The first decision on a request is the one that counts (D83). Settling is one conditional
//! update, so two devices deciding at once record one decision, and the other gets that recorded
//! outcome back instead of an error.

use kratos_protocol::{
    Approval, ApprovalDecision, ApprovalId, ApprovalOutcome, Question, QuestionId, QuestionOutcome,
    ThreadEventBody, ThreadId,
};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::json::{required_column, to_column};
use crate::{Store, StoreError, ThreadWrite};

/// How a waiting request is settled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settle<D> {
    Decide(D),
    /// It ended without a decision, for example because its turn ended.
    Close(String),
}

/// An approval with the thread it belongs to and what became of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredApproval {
    pub thread_id: ThreadId,
    pub approval: Approval,
    pub outcome: ApprovalOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredQuestion {
    pub thread_id: ThreadId,
    pub question: Question,
    pub outcome: QuestionOutcome,
}

const DECISIONS: [(ApprovalDecision, &str); 4] = [
    (ApprovalDecision::Accept, "accept"),
    (ApprovalDecision::AcceptForSession, "acceptForSession"),
    (ApprovalDecision::Decline, "decline"),
    (ApprovalDecision::Cancel, "cancel"),
];

impl ThreadWrite<'_> {
    pub fn request_approval(&mut self, approval: Approval) -> Result<(), StoreError> {
        self.transaction.execute(
            "INSERT INTO approvals (id, thread_id, approval, requested_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                approval.id.as_str(),
                self.thread_id().as_str(),
                to_column(&approval),
                self.at().as_str(),
            ],
        )?;
        self.append(ThreadEventBody::ApprovalRequested { approval })?;
        Ok(())
    }

    /// Settles a waiting approval and records how, or, when it was already settled, returns what
    /// was recorded then and writes nothing. `Err(UnknownApproval)` when this thread has no such
    /// approval.
    pub fn settle_approval(
        &mut self,
        id: &ApprovalId,
        settle: Settle<ApprovalDecision>,
    ) -> Result<ApprovalOutcome, StoreError> {
        let (decision, closed_reason) = match &settle {
            Settle::Decide(decision) => (Some(decision_name(*decision)), None),
            Settle::Close(reason) => (None, Some(reason.as_str())),
        };
        let settled = self.transaction.execute(
            "UPDATE approvals SET decision = ?1, closed_reason = ?2, settled_at = ?3
             WHERE id = ?4 AND thread_id = ?5 AND settled_at IS NULL",
            params![
                decision,
                closed_reason,
                self.at().as_str(),
                id.as_str(),
                self.thread_id().as_str(),
            ],
        )?;
        if settled == 0 {
            return match stored_approval(&self.transaction, id)? {
                Some(stored) if &stored.thread_id == self.thread_id() => Ok(stored.outcome),
                _ => Err(StoreError::UnknownApproval(id.clone())),
            };
        }
        let (event, outcome) = match settle {
            Settle::Decide(decision) => (
                ThreadEventBody::ApprovalDecided {
                    approval_id: id.clone(),
                    decision,
                },
                ApprovalOutcome::Decided { decision },
            ),
            Settle::Close(reason) => (
                ThreadEventBody::ApprovalClosed {
                    approval_id: id.clone(),
                    reason: reason.clone(),
                },
                ApprovalOutcome::Closed { reason },
            ),
        };
        self.append(event)?;
        Ok(outcome)
    }

    pub fn ask_question(&mut self, question: Question) -> Result<(), StoreError> {
        self.transaction.execute(
            "INSERT INTO questions (id, thread_id, question, asked_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                question.id.as_str(),
                self.thread_id().as_str(),
                to_column(&question),
                self.at().as_str(),
            ],
        )?;
        self.append(ThreadEventBody::QuestionAsked { question })?;
        Ok(())
    }

    /// As [`ThreadWrite::settle_approval`], for a question. An answer's text is never stored,
    /// because it may be a secret.
    pub fn settle_question(
        &mut self,
        id: &QuestionId,
        settle: Settle<()>,
    ) -> Result<QuestionOutcome, StoreError> {
        let (answered, closed_reason) = match &settle {
            Settle::Decide(()) => (true, None),
            Settle::Close(reason) => (false, Some(reason.as_str())),
        };
        let settled = self.transaction.execute(
            "UPDATE questions SET answered = ?1, closed_reason = ?2, settled_at = ?3
             WHERE id = ?4 AND thread_id = ?5 AND settled_at IS NULL",
            params![
                answered,
                closed_reason,
                self.at().as_str(),
                id.as_str(),
                self.thread_id().as_str(),
            ],
        )?;
        if settled == 0 {
            return match stored_question(&self.transaction, id)? {
                Some(stored) if &stored.thread_id == self.thread_id() => Ok(stored.outcome),
                _ => Err(StoreError::UnknownQuestion(id.clone())),
            };
        }
        let (event, outcome) = match settle {
            Settle::Decide(()) => (
                ThreadEventBody::QuestionAnswered {
                    question_id: id.clone(),
                },
                QuestionOutcome::Answered,
            ),
            Settle::Close(reason) => (
                ThreadEventBody::QuestionClosed {
                    question_id: id.clone(),
                    reason: reason.clone(),
                },
                QuestionOutcome::Closed { reason },
            ),
        };
        self.append(event)?;
        Ok(outcome)
    }
}

impl Store {
    pub fn approval(&self, id: &ApprovalId) -> Result<Option<StoredApproval>, StoreError> {
        stored_approval(&self.connection, id)
    }

    pub fn question(&self, id: &QuestionId) -> Result<Option<StoredQuestion>, StoreError> {
        stored_question(&self.connection, id)
    }

    /// The thread's approvals that wait on the user, oldest first.
    pub fn waiting_approvals(&self, thread_id: &ThreadId) -> Result<Vec<Approval>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT approval FROM approvals WHERE thread_id = ?1 AND settled_at IS NULL
             ORDER BY requested_at, rowid",
        )?;
        let approvals = statement.query_map([thread_id.as_str()], |row| required_column(row, 0))?;
        Ok(approvals.collect::<Result<_, _>>()?)
    }

    pub fn waiting_questions(&self, thread_id: &ThreadId) -> Result<Vec<Question>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT question FROM questions WHERE thread_id = ?1 AND settled_at IS NULL
             ORDER BY asked_at, rowid",
        )?;
        let questions = statement.query_map([thread_id.as_str()], |row| required_column(row, 0))?;
        Ok(questions.collect::<Result<_, _>>()?)
    }

    /// Every thread with an approval or a question still waiting. After a restart nothing can
    /// answer them, because the provider process that asked is gone.
    pub fn threads_with_waiting_requests(&self) -> Result<Vec<ThreadId>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT thread_id FROM approvals WHERE settled_at IS NULL
             UNION SELECT thread_id FROM questions WHERE settled_at IS NULL",
        )?;
        let threads = statement.query_map([], |row| Ok(ThreadId::new(row.get(0)?)))?;
        Ok(threads.collect::<Result<_, _>>()?)
    }
}

fn stored_approval(
    connection: &Connection,
    id: &ApprovalId,
) -> Result<Option<StoredApproval>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT thread_id, approval, decision, closed_reason FROM approvals WHERE id = ?1",
            [id.as_str()],
            approval_from_row,
        )
        .optional()?;
    Ok(stored)
}

fn approval_from_row(row: &Row<'_>) -> rusqlite::Result<StoredApproval> {
    let decision: Option<String> = row.get(2)?;
    let closed_reason: Option<String> = row.get(3)?;
    let outcome = match (decision, closed_reason) {
        (Some(name), _) => {
            let decision = DECISIONS
                .iter()
                .find(|(_, known)| *known == name)
                .map(|(decision, _)| *decision)
                .ok_or_else(|| {
                    let reason = format!("unknown decision {name}");
                    rusqlite::Error::FromSqlConversionFailure(2, Type::Text, reason.into())
                })?;
            ApprovalOutcome::Decided { decision }
        }
        (None, Some(reason)) => ApprovalOutcome::Closed { reason },
        (None, None) => ApprovalOutcome::Waiting,
    };
    Ok(StoredApproval {
        thread_id: ThreadId::new(row.get(0)?),
        approval: required_column(row, 1)?,
        outcome,
    })
}

fn stored_question(
    connection: &Connection,
    id: &QuestionId,
) -> Result<Option<StoredQuestion>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT thread_id, question, answered, closed_reason FROM questions WHERE id = ?1",
            [id.as_str()],
            |row| {
                let answered: bool = row.get(2)?;
                let closed_reason: Option<String> = row.get(3)?;
                let outcome = match (answered, closed_reason) {
                    (true, _) => QuestionOutcome::Answered,
                    (false, Some(reason)) => QuestionOutcome::Closed { reason },
                    (false, None) => QuestionOutcome::Waiting,
                };
                Ok(StoredQuestion {
                    thread_id: ThreadId::new(row.get(0)?),
                    question: required_column(row, 1)?,
                    outcome,
                })
            },
        )
        .optional()?;
    Ok(stored)
}

fn decision_name(decision: ApprovalDecision) -> &'static str {
    DECISIONS
        .iter()
        .find(|(known, _)| *known == decision)
        .map(|(_, name)| *name)
        .expect("every decision has a stored name")
}
