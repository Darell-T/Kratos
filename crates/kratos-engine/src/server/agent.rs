//! The provider that drives a thread, chosen by the thread's own provider. A thread's routes
//! reach its agent through [`Agent`], so a route is written once for both.

use kratos_protocol::{
    AccessLimit, Message, MessageId, MessageInput, MessageOrigin, Provider, ThreadId,
    ThreadSettings,
};

use super::{ApiError, ServerState};
use crate::providers::claude::ClaudeProvider;
use crate::providers::codex::CodexProvider;
use crate::providers::{ProviderError, Sent};

pub(super) enum Agent<'a> {
    Codex(&'a CodexProvider),
    Claude(&'a ClaudeProvider),
}

/// What drives a thread: its provider's agent, or nothing, so that a message only joins the log.
pub(super) enum Drive<'a> {
    Agent(Agent<'a>),
    LogOnly,
}

impl ServerState {
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
        match provider {
            Provider::Codex => Ok(Agent::Codex(self.codex()?)),
            Provider::Claude => Ok(Agent::Claude(self.claude()?)),
        }
    }

    /// How `provider`'s threads are driven. A provider that cannot run answers 503 saying why,
    /// except Claude, whose threads keep R2's plain message log until Claude is installed.
    pub(super) fn drive(&self, provider: Provider) -> Result<Drive<'_>, ApiError> {
        match (provider, self.agent_of(provider)) {
            (_, Ok(agent)) => Ok(Drive::Agent(agent)),
            (Provider::Claude, Err(_)) => Ok(Drive::LogOnly),
            (Provider::Codex, Err(unavailable)) => Err(unavailable),
        }
    }

    /// The agent of `thread_id`'s provider. 404 for a thread that does not exist, 503 saying why
    /// when its provider cannot run.
    pub(super) async fn agent(&self, thread_id: &ThreadId) -> Result<Agent<'_>, ApiError> {
        self.agent_of(self.provider_of(thread_id).await?)
    }
}

impl Agent<'_> {
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
