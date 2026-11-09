//! The provider that drives a thread, one dispatch over [`Provider`] for everything that reaches
//! an agent: the HTTP routes and the usage scheduler (D39). Code that serves both providers calls
//! these methods instead of matching on the provider itself.

use kratos_protocol::{ContinuationId, Provider, ThreadId, ThreadSkills};

use super::claude::ClaudeProvider;
use super::codex::CodexProvider;
use super::{Continued, ProviderError};

const NO_CODEX_SLASH_COMMANDS: &str =
    "Codex's app-server protocol does not list its slash commands, so Kratos cannot offer them";

/// Kratos loads only the user's Claude settings (D21), so what a project defines is not in the
/// CLI's list.
const CLAUDE_COMMANDS_FROM_USER_SETTINGS: &str = "Kratos loads only your Claude settings, so a project's own slash commands and skills are not listed";

/// The providers the engine drives, each running or with the reason it cannot, such as no CLI
/// installed. The one place that maps a [`Provider`] to its [`Agent`], for the routes and the
/// scheduler alike.
#[derive(Clone)]
pub struct Providers {
    pub codex: Result<CodexProvider, String>,
    pub claude: Result<ClaudeProvider, String>,
}

impl Providers {
    /// The agent of `provider`, or why it cannot run.
    pub(crate) fn agent(&self, provider: Provider) -> Result<Agent<'_>, String> {
        match provider {
            Provider::Codex => self.codex.as_ref().map(Agent::Codex),
            Provider::Claude => self.claude.as_ref().map(Agent::Claude),
        }
        .map_err(Clone::clone)
    }

    /// Whether neither provider runs.
    pub fn none_running(&self) -> bool {
        self.codex.is_err() && self.claude.is_err()
    }
}

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
