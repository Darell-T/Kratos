//! The routes' side of [`Agent`], the provider that drives a thread, chosen by the thread's own
//! provider. A thread's routes reach its agent through it, so a route is written once for both.

use kratos_protocol::{
    AccessLimit, ApprovalDecision, ApprovalId, ApprovalOutcome, Message, MessageId, MessageInput,
    MessageOrigin, Provider, QuestionId, QuestionOutcome, ThreadId, ThreadSettings,
};

use super::{ApiError, ServerState};
use crate::providers::{Agent, ProviderError, Sent};

impl ServerState {
    pub(super) async fn approval_agent(&self, id: &ApprovalId) -> Result<Agent<'_>, ApiError> {
        let lookup = id.clone();
        match self
            .store
            .call(move |store| store.approval(&lookup))
            .await?
        {
            Some(stored) => self.agent(&stored.thread_id).await,
            None => Ok(Agent::Codex(self.codex()?)),
        }
    }

    pub(super) async fn question_agent(&self, id: &QuestionId) -> Result<Agent<'_>, ApiError> {
        let lookup = id.clone();
        match self
            .store
            .call(move |store| store.question(&lookup))
            .await?
        {
            Some(stored) => self.agent(&stored.thread_id).await,
            None => Ok(Agent::Codex(self.codex()?)),
        }
    }

    /// The provider of `thread_id`. 404 for a thread that does not exist.
    pub(super) async fn provider_of(&self, thread_id: &ThreadId) -> Result<Provider, ApiError> {
        let lookup = thread_id.clone();
        Ok(self
            .store
            .call(move |store| store.provider_thread(&lookup))
            .await?
            .thread
            .provider)
    }

    /// The agent of `provider`. 503 saying why when it cannot run.
    pub(super) fn agent_of(&self, provider: Provider) -> Result<Agent<'_>, ApiError> {
        self.providers
            .agent(provider)
            .map_err(ApiError::unavailable)
    }

    /// The agent of `thread_id`'s provider. 404 for a thread that does not exist, 503 saying why
    /// when its provider cannot run.
    pub(super) async fn agent(&self, thread_id: &ThreadId) -> Result<Agent<'_>, ApiError> {
        self.agent_of(self.provider_of(thread_id).await?)
    }
}

impl Agent<'_> {
    pub(super) async fn decide(
        &self,
        id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<ApprovalOutcome, ProviderError> {
        match self {
            Agent::Codex(provider) => provider.decide(id, decision).await,
            Agent::Claude(provider) => provider.decide(id, decision).await,
        }
    }

    pub(super) async fn answer(
        &self,
        id: QuestionId,
        answers: std::collections::BTreeMap<String, Vec<String>>,
        limit: Option<AccessLimit>,
    ) -> Result<QuestionOutcome, ProviderError> {
        match self {
            Agent::Codex(provider) => provider.answer(id, answers, limit).await,
            Agent::Claude(provider) => provider.answer(id, answers, limit).await,
        }
    }
    pub(super) async fn send(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
        input: Vec<MessageInput>,
        origin: MessageOrigin,
    ) -> Result<Sent, ProviderError> {
        match self {
            Agent::Codex(codex) => codex.send(thread_id, message_id, input, origin).await,
            Agent::Claude(claude) => claude.send(thread_id, message_id, input, origin).await,
        }
    }

    pub(super) async fn steer(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        match self {
            Agent::Codex(codex) => codex.steer(thread_id, message_id).await,
            Agent::Claude(claude) => claude.steer(thread_id, message_id).await,
        }
    }

    pub(super) async fn remove(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        match self {
            Agent::Codex(codex) => codex.remove(thread_id, message_id).await,
            Agent::Claude(claude) => claude.remove(thread_id, message_id).await,
        }
    }

    pub(super) async fn retry(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        match self {
            Agent::Codex(codex) => codex.retry(thread_id, message_id).await,
            Agent::Claude(claude) => claude.retry(thread_id, message_id).await,
        }
    }

    pub(super) async fn discard(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        match self {
            Agent::Codex(codex) => codex.discard(thread_id, message_id).await,
            Agent::Claude(claude) => claude.discard(thread_id, message_id).await,
        }
    }

    /// Refuses settings the provider cannot run a thread with.
    pub(super) async fn check_settings(
        &self,
        settings: ThreadSettings,
    ) -> Result<(), ProviderError> {
        match self {
            Agent::Codex(codex) => codex.check_settings(settings).await,
            Agent::Claude(claude) => claude.check_settings(settings).await,
        }
    }

    pub(super) async fn stop(&self, thread_id: ThreadId) -> Result<(), ProviderError> {
        match self {
            Agent::Codex(codex) => codex.stop(thread_id).await,
            Agent::Claude(claude) => claude.stop(thread_id).await,
        }
    }

    pub(super) async fn resume(&self, thread_id: ThreadId) -> Result<(), ProviderError> {
        match self {
            Agent::Codex(codex) => codex.resume(thread_id).await,
            Agent::Claude(claude) => claude.resume(thread_id).await,
        }
    }

    pub(super) async fn set_settings(
        &self,
        thread_id: ThreadId,
        settings: ThreadSettings,
    ) -> Result<(), ProviderError> {
        match self {
            Agent::Codex(codex) => codex.set_settings(thread_id, settings).await,
            Agent::Claude(claude) => claude.set_settings(thread_id, settings).await,
        }
    }

    pub(super) async fn access(&self, thread_id: ThreadId) -> Result<AccessLimit, ProviderError> {
        match self {
            Agent::Codex(codex) => codex.access(thread_id).await,
            Agent::Claude(claude) => claude.access(thread_id).await,
        }
    }
}
