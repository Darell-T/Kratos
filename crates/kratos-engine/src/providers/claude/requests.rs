//! Claude controls waiting on the user, persisted through the same store and Hub flow as Codex.

use std::collections::{BTreeMap, HashMap};

use kratos_claude_client::IncomingControl;
use kratos_protocol::{
    AccessLimit, Approval, ApprovalDecision, ApprovalId, ApprovalOutcome, AskReason, Question,
    QuestionId, QuestionOutcome, ThreadId, TurnId,
};
use kratos_store::Settle;
use serde_json::{Value, json};
use uuid::Uuid;

use super::ProviderError;
use super::actor::Actor;
use super::tools::{self, CommandArgs};

pub(super) enum Answer {
    Tool(Value),
    Command { args: CommandArgs, id: Value },
    Elicitation,
}

pub(super) struct Pending {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub control: IncomingControl,
    pub answer: Answer,
}

impl Pending {
    pub(super) fn deny(self) {
        let body = match self.answer {
            Answer::Tool(_) => {
                json!({ "behavior": "deny", "message": "The user declined this action" })
            }
            Answer::Command { id, .. } => {
                tools::error(id, -32000, "The user declined this command")
            }
            Answer::Elicitation => json!({ "action": "cancel" }),
        };
        let _ = self.control.answer(body);
    }
}

#[derive(Default)]
pub(super) struct Requests {
    approvals: HashMap<ApprovalId, Pending>,
    questions: HashMap<QuestionId, Pending>,
}

impl Requests {
    pub(super) fn blocks(&self, thread_id: &ThreadId) -> bool {
        self.approvals
            .values()
            .chain(self.questions.values())
            .any(|waiting| &waiting.thread_id == thread_id)
    }
}

impl Actor {
    pub(super) async fn request_approval(&mut self, pending: Pending, why: AskReason) {
        let request = super::permissions::shown(&pending, &self.threads[&pending.thread_id].folder);
        let approval = Approval {
            id: ApprovalId::new(Uuid::new_v4().to_string()),
            turn_id: pending.turn_id.clone(),
            request,
            why,
        };
        let id = approval.id.clone();
        if self
            .write(&pending.thread_id, move |write| {
                write.request_approval(approval)
            })
            .await
            .is_ok()
        {
            self.requests.approvals.insert(id, pending);
        } else {
            pending.deny();
        }
    }

    pub(super) async fn request_question(&mut self, pending: Pending, question: Question) {
        let id = question.id.clone();
        if self
            .write(&pending.thread_id, move |write| {
                write.ask_question(question)
            })
            .await
            .is_ok()
        {
            self.requests.questions.insert(id, pending);
        } else {
            pending.deny();
        }
    }

    pub(super) fn allow_pending(&mut self, mut pending: Pending) {
        let thread = &self.threads[&pending.thread_id];
        if thread.stopping || thread.run.as_ref() != Some(&pending.turn_id) {
            pending.deny();
            return;
        }
        match std::mem::replace(&mut pending.answer, Answer::Tool(Value::Null)) {
            Answer::Tool(input) => {
                let _ = pending
                    .control
                    .answer(json!({ "behavior": "allow", "updatedInput": input }));
            }
            Answer::Command { args, id } => self.execute_command(pending, args, id),
            Answer::Elicitation => pending.deny(),
        }
    }

    fn remember_command(&mut self, pending: &Pending) {
        let kratos_claude_client::ControlRequest::CanUseTool(tool) = &pending.control.request
        else {
            return;
        };
        if let Some(args) = tools::callback_command(tool) {
            self.threads
                .get_mut(&pending.thread_id)
                .expect("pending thread is loaded")
                .command_grants
                .push(args);
        }
    }

    pub(super) async fn decide(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<ApprovalOutcome, ProviderError> {
        let Some(pending) = self.requests.approvals.remove(&id) else {
            let lookup = id.clone();
            return self
                .store
                .call(move |store| store.approval(&lookup))
                .await?
                .map(|stored| stored.outcome)
                .ok_or(ProviderError::UnknownApproval(id));
        };
        let lookup = id.clone();
        let settlement =
            if pending.control.is_pending() && !self.threads[&pending.thread_id].stopping {
                Settle::Decide(decision)
            } else {
                Settle::Close("Claude no longer needs an answer".into())
            };
        let outcome = self
            .write(&pending.thread_id, move |write| {
                write.settle_approval(&lookup, settlement)
            })
            .await;
        let thread_id = pending.thread_id.clone();
        match &outcome {
            Ok(ApprovalOutcome::Decided { decision: recorded })
                if *recorded == decision && pending.control.is_pending() =>
            {
                match decision {
                    ApprovalDecision::Accept | ApprovalDecision::AcceptForSession => {
                        self.remember_command(&pending);
                        self.allow_pending(pending)
                    }
                    ApprovalDecision::Decline | ApprovalDecision::Cancel => pending.deny(),
                }
                if decision == ApprovalDecision::Cancel {
                    self.interrupt(&thread_id, Vec::new(), false);
                }
            }
            _ => pending.deny(),
        }
        self.arm_deadline(&thread_id);
        outcome
    }

    pub(super) async fn answer(
        &mut self,
        id: QuestionId,
        answers: BTreeMap<String, Vec<String>>,
        limit: Option<AccessLimit>,
    ) -> Result<QuestionOutcome, ProviderError> {
        if let Some(pending) = self.requests.questions.get(&id)
            && limit.is_some_and(|limit| {
                !limit.covers(self.threads[&pending.thread_id].turn_settings().access())
            })
        {
            return Err(ProviderError::Forbidden(
                "A thread cannot answer a question running with more permission than its own."
                    .into(),
            ));
        }
        let Some(pending) = self.requests.questions.remove(&id) else {
            let lookup = id.clone();
            return self
                .store
                .call(move |store| store.question(&lookup))
                .await?
                .map(|stored| stored.outcome)
                .ok_or(ProviderError::UnknownQuestion(id));
        };
        let settlement =
            if pending.control.is_pending() && !self.threads[&pending.thread_id].stopping {
                Settle::Decide(())
            } else {
                Settle::Close("Claude no longer needs an answer".into())
            };
        let outcome = self
            .write(&pending.thread_id, move |write| {
                write.settle_question(&id, settlement)
            })
            .await;
        let thread_id = pending.thread_id.clone();
        if matches!(outcome, Ok(QuestionOutcome::Answered)) {
            let body = question_response(pending.answer, answers);
            let _ = pending.control.answer(body);
        } else {
            pending.deny();
        }
        self.arm_deadline(&thread_id);
        outcome
    }

    pub(super) async fn close_requests(
        &mut self,
        thread_id: &ThreadId,
        control_id: Option<&str>,
        reason: &str,
    ) {
        if let Some(thread) = self.threads.get_mut(thread_id) {
            thread.cancel_commands(control_id);
        }
        let selected = |pending: &Pending| {
            &pending.thread_id == thread_id && control_id.is_none_or(|id| pending.control.id == id)
        };
        let approvals: Vec<_> = self
            .requests
            .approvals
            .iter()
            .filter(|(_, pending)| selected(pending))
            .map(|(id, _)| id.clone())
            .collect();
        let questions: Vec<_> = self
            .requests
            .questions
            .iter()
            .filter(|(_, pending)| selected(pending))
            .map(|(id, _)| id.clone())
            .collect();
        let mut pending = Vec::new();
        for id in &approvals {
            pending.extend(self.requests.approvals.remove(id));
        }
        for id in &questions {
            pending.extend(self.requests.questions.remove(id));
        }
        if pending.is_empty() {
            return;
        }
        let reason = reason.to_owned();
        let _ = self
            .write(thread_id, move |write| {
                for id in approvals {
                    write.settle_approval(&id, Settle::Close(reason.clone()))?;
                }
                for id in questions {
                    write.settle_question(&id, Settle::Close(reason.clone()))?;
                }
                Ok(())
            })
            .await;
        for pending in pending {
            pending.deny();
        }
        self.arm_deadline(thread_id);
    }
}

fn question_response(answer: Answer, answers: BTreeMap<String, Vec<String>>) -> Value {
    match answer {
        Answer::Tool(mut input) => {
            let answers: BTreeMap<_, _> = answers
                .into_iter()
                .map(|(key, values)| (key, values.join(", ")))
                .collect();
            input["answers"] = json!(answers);
            json!({ "behavior": "allow", "updatedInput": input })
        }
        Answer::Elicitation if answers.is_empty() => json!({ "action": "cancel" }),
        Answer::Elicitation => json!({ "action": "accept", "content": answers }),
        Answer::Command { .. } => unreachable!("commands are approvals, never questions"),
    }
}
