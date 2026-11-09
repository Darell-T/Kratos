//! The provider that drives a thread, one dispatch over [`Provider`] for everything that reaches
//! an agent: the HTTP routes and the usage scheduler (D39). Code that serves both providers calls
//! these methods instead of matching on the provider itself.

use kratos_protocol::{ContinuationId, ThreadId, ThreadSkills};

use super::claude::ClaudeProvider;
use super::codex::CodexProvider;
use super::{Continued, ProviderError};

const NO_CODEX_SLASH_COMMANDS: &str =
    "Codex's app-server protocol does not list its slash commands, so Kratos cannot offer them";

/// Kratos loads only the user's Claude settings (D21), so what a project defines is not in the
/// CLI's list.
const CLAUDE_COMMANDS_FROM_USER_SETTINGS: &str = "Kratos loads only your Claude settings, so a project's own slash commands and skills are not listed";

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

    /// What the agent CLI offers in the thread's folder (D97). Codex lists skills and no slash
    /// commands. Claude lists its skills among its slash commands.
    pub(crate) async fn skills(&self, thread_id: ThreadId) -> Result<ThreadSkills, ProviderError> {
        Ok(match self {
            Agent::Codex(codex) => ThreadSkills {
                skills: codex.skills(thread_id).await?,
                slash_commands: Vec::new(),
                slash_commands_gap: Some(NO_CODEX_SLASH_COMMANDS.to_owned()),
            },
            Agent::Claude(claude) => ThreadSkills {
                skills: Vec::new(),
                slash_commands: claude.slash_commands(thread_id).await?,
                slash_commands_gap: Some(CLAUDE_COMMANDS_FROM_USER_SETTINGS.to_owned()),
            },
        })
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
