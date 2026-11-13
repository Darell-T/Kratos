//! A thread's settings: replacing them, and which ones Claude can honor (D76, D87).

use kratos_protocol::{ThreadId, ThreadSettings};

use super::ProviderError;
use super::actor::Actor;
use super::threads::SessionConfig;

impl Actor {
    /// Replaces the thread's settings. They apply from the next turn (D87): a turn that runs keeps
    /// its process, and the next message starts one that fits the new settings.
    pub(super) async fn set_settings(
        &mut self,
        thread_id: &ThreadId,
        settings: ThreadSettings,
    ) -> Result<(), ProviderError> {
        self.running()?;
        self.load(thread_id).await?;
        check_settings(&settings)?;
        let stored = settings.clone();
        self.write(thread_id, move |write| write.set_settings(stored))
            .await?;
        let thread = self.threads.get_mut(thread_id).expect("loaded above");
        thread.settings = settings;
        let outdated = thread
            .open()
            .is_some_and(|open| open.config != SessionConfig::of(&thread.settings));
        if outdated && !thread.is_busy() {
            self.retire_session(thread_id);
        }
        Ok(())
    }
}

/// Refuses a setting the pinned Claude CLI cannot honor. Claude Code reports no way to change its
/// context window from a client, so a context limit is unsupported rather than ignored (D76).
pub(super) fn check_settings(settings: &ThreadSettings) -> Result<(), ProviderError> {
    if settings.context_limit_tokens.is_some() {
        return Err(ProviderError::Unsupported(
            "contextLimitTokens: Kratos cannot set Claude's context window yet".into(),
        ));
    }
    Ok(())
}
