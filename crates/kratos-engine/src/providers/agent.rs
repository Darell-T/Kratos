//! The provider that drives a thread, one dispatch over [`Provider`] for everything that reaches
//! an agent: the HTTP routes and the usage scheduler (D39). Code that serves both providers calls
//! these methods instead of matching on the provider itself.

use kratos_protocol::{ContinuationId, ThreadId};

use super::claude::ClaudeProvider;
use super::codex::CodexProvider;
use super::{Continued, ProviderError};

pub(crate) enum Agent<'a> {
    Codex(&'a CodexProvider),
    Claude(&'a ClaudeProvider),
}

impl Agent<'_> {
    /// Reads the account's usage now (D38). Codex reads it from its one server; Claude reads it
    /// through a session of `thread_id`, starting one when none is open.
    pub(crate) async fn refresh_usage(&self, thread_id: &ThreadId) -> Result<(), ProviderError> {
        match self {
            Agent::Codex(codex) => codex.refresh_usage().await,
            Agent::Claude(claude) => claude.refresh_usage(thread_id.clone()).await,
        }
    }

    /// Queues a continuation's message ahead of the thread's waiting messages and lets the queue
    /// run (D39). Called by the usage scheduler only.
    pub(crate) async fn continue_thread(
        &self,
        thread_id: ThreadId,
        continuation_id: ContinuationId,
    ) -> Result<Continued, ProviderError> {
        match self {
            Agent::Codex(codex) => codex.continue_thread(thread_id, continuation_id).await,
            Agent::Claude(claude) => claude.continue_thread(thread_id, continuation_id).await,
        }
    }
}
