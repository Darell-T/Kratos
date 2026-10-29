//! Codex requests that wait on the user: approvals and questions, by Kratos's own ids.
//!
//! Codex's request ids last only as long as its process, so they never leave the actor. Dropping an
//! entry drops its `IncomingRequest`, which answers Codex if it still waits.

use std::collections::HashMap;

use kratos_codex_client::{IncomingRequest, RequestId, types};
use kratos_protocol::{ApprovalId, QuestionId, ThreadId, TurnId};

pub(super) struct Waiting {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub codex_id: RequestId,
    pub request: IncomingRequest,
    pub answer: Answer,
}

pub(super) enum Answer {
    /// `{"decision": ...}`, for a command or a patch.
    Decision,
    /// The permissions granted, from what was asked.
    Permissions(types::RequestPermissionProfile),
    Answers,
}

#[derive(Default)]
pub(super) struct Requests {
    pub approvals: HashMap<ApprovalId, Waiting>,
    pub questions: HashMap<QuestionId, Waiting>,
}

/// The requests a closing step removed, to settle in the records.
pub(super) struct Closed {
    pub approvals: Vec<ApprovalId>,
    pub questions: Vec<QuestionId>,
}

impl Requests {
    pub fn close(&mut self, thread_id: &ThreadId, matches: impl Fn(&Waiting) -> bool) -> Closed {
        let selected = |waiting: &Waiting| &waiting.thread_id == thread_id && matches(waiting);
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
            .or_else(|| question.as_ref().and_then(|id| self.questions.get(id)))
            .map(|waiting| waiting.thread_id.clone())?;
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
            .chain(self.questions.values())
            .map(|waiting| waiting.thread_id.clone())
            .collect();
        threads.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        threads.dedup();
        threads
    }
}

impl Waiting {
    fn is_blocking(&self) -> bool {
        match &self.request.request {
            kratos_codex_client::ServerRequest::UserInput(params) => params.is_blocking,
            _ => true,
        }
    }
}

fn take_where<K: Clone + Eq + std::hash::Hash>(
    map: &mut HashMap<K, Waiting>,
    selected: impl Fn(&Waiting) -> bool,
) -> Vec<K> {
    let ids: Vec<K> = map
        .iter()
        .filter(|(_, waiting)| selected(waiting))
        .map(|(id, _)| id.clone())
        .collect();
    for id in &ids {
        map.remove(id);
    }
    ids
}

fn find_id<K: Clone>(map: &HashMap<K, Waiting>, codex_id: &RequestId) -> Option<K> {
    map.iter()
        .find(|(_, waiting)| &waiting.codex_id == codex_id)
        .map(|(id, _)| id.clone())
}
