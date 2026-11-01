//! Codex requests that wait on the user, approvals and questions, from their arrival to the answer
//! Codex gets. Kratos keeps them by its own ids.
//!
//! Codex's request ids last only as long as its process, so they never leave the actor. Dropping an
//! entry drops its `IncomingRequest`, which answers Codex if it still waits.

use std::collections::{BTreeMap, HashMap};

use kratos_codex_client::{IncomingRequest, RequestId, ServerRequest, ServerResponse, types};
use kratos_protocol::{
    AccessLimit, Approval, ApprovalDecision, ApprovalId, ApprovalOutcome, ApprovalRequest,
    AskReason, QuestionId, QuestionOutcome, ThreadId, TurnId,
};
use kratos_store::{Settle, StoreError, ThreadWrite};
use uuid::Uuid;

use super::actor::Actor;
use super::policy::{self, CommandRequest, Verdict};
use super::{ProviderError, wire};

/// A request that waits on the user. `A` is what an approval's answer is built from; a question
/// needs nothing.
pub(super) struct Waiting<A = ()> {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub request: IncomingRequest,
    pub answer: A,
}

pub(super) enum ApprovalAnswer {
    Command,
    FileChange,
    /// The permissions granted, from what was asked.
    Permissions(types::RequestPermissionProfile),
}

impl ApprovalAnswer {
    pub fn response(&self, decision: ApprovalDecision) -> ServerResponse {
        match self {
            ApprovalAnswer::Command => {
                ServerResponse::CommandApproval(wire::command_decision(decision))
            }
            ApprovalAnswer::FileChange => {
                ServerResponse::FileChangeApproval(wire::file_change_decision(decision))
            }
            ApprovalAnswer::Permissions(requested) => {
                ServerResponse::Permissions(wire::permissions_decision(decision, requested))
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Requests {
    pub approvals: HashMap<ApprovalId, Waiting<ApprovalAnswer>>,
    pub questions: HashMap<QuestionId, Waiting>,
}

/// The requests a closing step removed, to settle in the records.
#[derive(Default)]
pub(super) struct Closed {
    pub approvals: Vec<ApprovalId>,
    pub questions: Vec<QuestionId>,
}

impl Requests {
    pub fn close(&mut self, thread_id: &ThreadId, in_turn: impl Fn(&TurnId) -> bool) -> Closed {
        let selected = |waiting_thread: &ThreadId, waiting_turn: &TurnId| {
            waiting_thread == thread_id && in_turn(waiting_turn)
        };
        Closed {
            approvals: take_where(&mut self.approvals, selected),
            questions: take_where(&mut self.questions, selected),
        }
    }

    /// The thread whose request Codex withdrew, and that request's ids.
    pub fn withdraw(&mut self, codex_id: &RequestId) -> Option<(ThreadId, Closed)> {
        let approval = find_id(&self.approvals, codex_id);
        let question = find_id(&self.questions, codex_id);
        let thread_id = approval
            .as_ref()
            .and_then(|id| self.approvals.get(id))
            .map(|waiting| waiting.thread_id.clone())
            .or_else(|| {
                question
                    .as_ref()
                    .and_then(|id| self.questions.get(id))
                    .map(|waiting| waiting.thread_id.clone())
            })?;
        approval.as_ref().map(|id| self.approvals.remove(id));
        question.as_ref().map(|id| self.questions.remove(id));
        Some((
            thread_id,
            Closed {
                approvals: approval.into_iter().collect(),
                questions: question.into_iter().collect(),
            },
        ))
    }

    /// Whether the thread's turn waits on the user, so its deadline does not run.
    pub fn blocks(&self, thread_id: &ThreadId) -> bool {
        self.approvals
            .values()
            .any(|waiting| &waiting.thread_id == thread_id)
            || self
                .questions
                .values()
                .any(|waiting| &waiting.thread_id == thread_id && waiting.is_blocking())
    }

    /// Every thread with a request waiting, for closing them all when Codex exits.
    pub fn threads(&self) -> Vec<ThreadId> {
        let mut threads: Vec<ThreadId> = self
            .approvals
            .values()
            .map(|waiting| waiting.thread_id.clone())
            .chain(
                self.questions
                    .values()
                    .map(|waiting| waiting.thread_id.clone()),
            )
            .collect();
        threads.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        threads.dedup();
        threads
    }
}

impl Waiting {
    fn is_blocking(&self) -> bool {
        match &self.request.request {
            ServerRequest::UserInput(params) => params.is_blocking,
            ServerRequest::CommandApproval(_)
            | ServerRequest::FileChangeApproval(_)
            | ServerRequest::Permissions(_)
            | ServerRequest::McpElicitation(_) => true,
        }
    }
}

fn take_where<K: Clone + Eq + std::hash::Hash, A>(
    map: &mut HashMap<K, Waiting<A>>,
    selected: impl Fn(&ThreadId, &TurnId) -> bool,
) -> Vec<K> {
    let ids: Vec<K> = map
        .iter()
        .filter(|(_, waiting)| selected(&waiting.thread_id, &waiting.turn_id))
        .map(|(id, _)| id.clone())
        .collect();
    for id in &ids {
        map.remove(id);
    }
    ids
}

fn find_id<K: Clone, A>(map: &HashMap<K, Waiting<A>>, codex_id: &RequestId) -> Option<K> {
    map.iter()
        .find(|(_, waiting)| &waiting.request.id == codex_id)
        .map(|(id, _)| id.clone())
}

impl Actor {
    /// Codex no longer needs an answer: the request closes, and an answer sent later is dropped.
    pub(super) async fn withdraw(&mut self, codex_id: &RequestId) {
        if let Some((thread_id, closed)) = self.requests.withdraw(codex_id) {
            let _ = self
                .write(&thread_id, move |write| {
                    settle_closed(write, &closed, "Codex no longer needs an answer")
                })
                .await;
        }
    }

    pub(super) async fn on_request(&mut self, request: IncomingRequest) {
        let Some(thread_id) = self.thread_of(request.request.thread_id()) else {
            // A thread Kratos does not run cannot be granted anything.
            refuse(request);
            return;
        };
        match &request.request {
            ServerRequest::CommandApproval(params) => {
                let verdict = {
                    let thread = &self.threads[&thread_id];
                    let ask = CommandRequest {
                        command: params.command.as_deref(),
                        reason: params.reason.as_deref(),
                        wants_more_permissions: params.additional_permissions.is_some(),
                        wants_network: params.network_approval_context.is_some(),
                    };
                    policy::decide_command(thread.turn_settings(), &ask, self.settings.platform)
                };
                let shown = ApprovalRequest::Command {
                    command: params.command.clone().unwrap_or_default(),
                    cwd: params.cwd.as_ref().map(|cwd| cwd.0.clone()),
                    reason: params.reason.clone(),
                    access: wire::command_access(params),
                };
                let turn_id = TurnId::new(params.turn_id.clone());
                self.settle_or_ask(
                    thread_id,
                    turn_id,
                    request,
                    verdict,
                    shown,
                    ApprovalAnswer::Command,
                )
                .await;
            }
            ServerRequest::FileChangeApproval(params) => {
                let thread = &self.threads[&thread_id];
                let changes = thread.file_items.get(&params.item_id).cloned();
                let verdict = policy::decide_patch(
                    thread.turn_settings(),
                    changes.as_deref(),
                    params.grant_root.is_some(),
                    std::path::Path::new(&thread.folder),
                    &self.settings.protected,
                );
                let shown = ApprovalRequest::FileChanges {
                    changes: changes.unwrap_or_default(),
                    reason: params.reason.clone(),
                };
                let turn_id = TurnId::new(params.turn_id.clone());
                self.settle_or_ask(
                    thread_id,
                    turn_id,
                    request,
                    verdict,
                    shown,
                    ApprovalAnswer::FileChange,
                )
                .await;
            }
            ServerRequest::Permissions(params) => {
                let shown = ApprovalRequest::Permissions {
                    reason: params.reason.clone(),
                    access: wire::permissions_access(&params.permissions),
                };
                let (turn_id, requested) = (
                    TurnId::new(params.turn_id.clone()),
                    params.permissions.clone(),
                );
                let verdict = Verdict::Ask(AskReason::Escalation);
                self.settle_or_ask(
                    thread_id,
                    turn_id,
                    request,
                    verdict,
                    shown,
                    ApprovalAnswer::Permissions(requested),
                )
                .await;
            }
            ServerRequest::UserInput(params) => {
                let question = wire::question(QuestionId::new(Uuid::new_v4().to_string()), params);
                self.ask_question(thread_id, request, question).await;
            }
            // Kratos shows no MCP forms yet, so the tool call that asked is declined.
            ServerRequest::McpElicitation(_) => refuse(request),
        }
    }

    async fn settle_or_ask(
        &mut self,
        thread_id: ThreadId,
        turn_id: TurnId,
        request: IncomingRequest,
        verdict: Verdict,
        shown: ApprovalRequest,
        answer: ApprovalAnswer,
    ) {
        let why = match verdict {
            Verdict::Accept => {
                request.answer(answer.response(ApprovalDecision::Accept));
                return;
            }
            Verdict::Ask(why) => why,
        };
        let approval = Approval {
            id: ApprovalId::new(Uuid::new_v4().to_string()),
            turn_id: turn_id.clone(),
            request: shown,
            why,
        };
        let id = approval.id.clone();
        if self
            .write(&thread_id, move |write| write.request_approval(approval))
            .await
            .is_err()
        {
            refuse(request);
            return;
        }
        self.requests.approvals.insert(
            id,
            Waiting {
                thread_id,
                turn_id,
                request,
                answer,
            },
        );
    }

    async fn ask_question(
        &mut self,
        thread_id: ThreadId,
        request: IncomingRequest,
        question: kratos_protocol::Question,
    ) {
        let (id, turn_id) = (question.id.clone(), question.turn_id.clone());
        if self
            .write(&thread_id, move |write| write.ask_question(question))
            .await
            .is_err()
        {
            refuse(request);
            return;
        }
        self.requests.questions.insert(
            id,
            Waiting {
                thread_id,
                turn_id,
                request,
                answer: (),
            },
        );
    }

    pub(super) async fn decide(
        &mut self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<ApprovalOutcome, ProviderError> {
        let Some(waiting) = self.requests.approvals.remove(&approval_id) else {
            return self.recorded_approval(&approval_id).await;
        };
        let outcome = {
            let approval_id = approval_id.clone();
            self.write(&waiting.thread_id, move |write| {
                write.settle_approval(&approval_id, Settle::Decide(decision))
            })
            .await
        };
        match &outcome {
            Ok(ApprovalOutcome::Decided { decision: recorded }) if *recorded == decision => {
                waiting.request.answer(waiting.answer.response(decision));
            }
            // Recorded otherwise, or not recorded: dropping the request answers Codex.
            _ => {}
        }
        outcome
    }

    async fn recorded_approval(
        &self,
        approval_id: &ApprovalId,
    ) -> Result<ApprovalOutcome, ProviderError> {
        let lookup = approval_id.clone();
        self.store
            .call(move |store| store.approval(&lookup))
            .await?
            .map(|stored| stored.outcome)
            .ok_or_else(|| ProviderError::UnknownApproval(approval_id.clone()))
    }

    pub(super) async fn answer(
        &mut self,
        question_id: QuestionId,
        answers: BTreeMap<String, Vec<String>>,
        limit: Option<AccessLimit>,
    ) -> Result<QuestionOutcome, ProviderError> {
        if let (Some(limit), Some(waiting)) = (limit, self.requests.questions.get(&question_id))
            && !limit.covers(self.threads[&waiting.thread_id].turn_settings().access())
        {
            return Err(ProviderError::Forbidden(
                "A thread cannot answer a question running with more permission than its own."
                    .to_owned(),
            ));
        }
        let Some(waiting) = self.requests.questions.remove(&question_id) else {
            let lookup = question_id.clone();
            return self
                .store
                .call(move |store| store.question(&lookup))
                .await?
                .map(|stored| stored.outcome)
                .ok_or(ProviderError::UnknownQuestion(question_id));
        };
        let outcome = self
            .write(&waiting.thread_id, move |write| {
                write.settle_question(&question_id, Settle::Decide(()))
            })
            .await;
        if matches!(outcome, Ok(QuestionOutcome::Answered)) {
            waiting
                .request
                .answer(ServerResponse::UserInput(wire::answers(answers)));
        }
        outcome
    }
}

/// Answers a request Kratos will not put to the user with the refusal of its kind.
fn refuse(request: IncomingRequest) {
    let refusal = wire::refusal(&request.request);
    request.answer(refusal);
}

/// Settles requests a step closed, in the same write as the step.
pub(super) fn settle_closed(
    write: &mut ThreadWrite<'_>,
    closed: &Closed,
    reason: &str,
) -> Result<(), StoreError> {
    for approval in &closed.approvals {
        write.settle_approval(approval, Settle::Close(reason.to_owned()))?;
    }
    for question in &closed.questions {
        write.settle_question(question, Settle::Close(reason.to_owned()))?;
    }
    Ok(())
}
