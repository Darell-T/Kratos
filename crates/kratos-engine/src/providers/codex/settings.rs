//! A thread's settings: replacing them, and which ones Codex can honor, judged by the context
//! windows Codex reports (D76, D87).

use kratos_protocol::{ThreadId, ThreadSettings};

use super::ProviderError;
use super::actor::Actor;

/// The smallest context window a thread may ask for (D76).
const MIN_CONTEXT_TOKENS: u64 = 1_000;

impl Actor {
    /// Replaces the thread's settings. They apply from the next turn (D87): a context limit
    /// applies once Codex loads the thread again, which happens before that turn.
    pub(super) async fn set_settings(
        &mut self,
        thread_id: &ThreadId,
        settings: ThreadSettings,
    ) -> Result<(), ProviderError> {
        self.load(thread_id).await?;
        self.check_settings(&settings, self.threads[thread_id].model())?;
        let stored = settings.clone();
        self.write(thread_id, move |write| write.set_settings(stored))
            .await?;
        self.threads
            .get_mut(thread_id)
            .expect("loaded above")
            .settings = settings;
        Ok(())
    }

    /// Refuses a blank model, and a context window outside the floor and the window the model
    /// reported with no setting of Kratos's in force. Kratos never claims a window larger than the
    /// model's (D76). `loaded_model` is the model Codex runs the thread with when the settings
    /// name none.
    pub(super) fn check_settings(
        &self,
        settings: &ThreadSettings,
        loaded_model: Option<&str>,
    ) -> Result<(), ProviderError> {
        if settings
            .model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(ProviderError::Unsupported(
                "model: must not be blank".into(),
            ));
        }
        let Some(limit) = settings.context_limit_tokens else {
            return Ok(());
        };
        let model = settings.model.as_deref().or(loaded_model);
        let window = model.and_then(|model| self.windows.get(model)).copied();
        match window {
            _ if limit < MIN_CONTEXT_TOKENS => Err(ProviderError::Unsupported(format!(
                "contextLimitTokens: must be at least {MIN_CONTEXT_TOKENS}"
            ))),
            Some(window) if limit > window => Err(ProviderError::Unsupported(format!(
                "contextLimitTokens: the model's context window is {window} tokens"
            ))),
            Some(_) => Ok(()),
            None => Err(ProviderError::Unsupported(
                "contextLimitTokens: Codex has not reported this model's context window yet; run a turn first"
                    .into(),
            )),
        }
    }

    /// Learns a model's own context window from Codex's report on `thread_id` (D76).
    pub(super) fn learn_window(&mut self, thread_id: &ThreadId, reported: Option<i64>) {
        let thread = self
            .threads
            .get_mut(thread_id)
            .expect("the caller found the thread");
        let loaded_without_limit = matches!(
            thread.link,
            super::threads::Link::Attached {
                context_limit: None,
                ..
            }
        );
        if loaded_without_limit
            && let (Some(window), Some(model)) = (reported, thread.model())
            && let Ok(window) = u64::try_from(window)
        {
            // The model's own window, seen with no setting of Kratos's in force (D76).
            self.windows.insert(model.to_owned(), window);
        }
    }
}
