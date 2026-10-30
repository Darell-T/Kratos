//! Requests a thread's agent makes through Kratos's tools. The `Kratos-Thread` header names that
//! thread, and a request without it comes from the user. Such a request may give no thread more
//! access than the acting thread has, and may not act on the acting thread itself.

use axum::http::{HeaderMap, StatusCode};
use kratos_protocol::{AccessLimit, MessageOrigin, ThreadId, ThreadSettings};
use kratos_store::{ProviderThread, StoreError};

use super::{ApiError, ServerState};

pub const ACTING_THREAD_HEADER: &str = "Kratos-Thread";

pub(super) struct Acting(ProviderThread);

impl Acting {
    /// The thread the request's `Kratos-Thread` header names, or `None` for the user.
    pub(super) async fn of(
        state: &ServerState,
        headers: &HeaderMap,
    ) -> Result<Option<Acting>, ApiError> {
        let Some(value) = headers.get(ACTING_THREAD_HEADER) else {
            return Ok(None);
        };
        let thread_id = value
            .to_str()
            .ok()
            .filter(|id| !id.is_empty())
            .map(|id| ThreadId::new(id.to_owned()))
            .ok_or_else(|| ApiError::bad_request("Kratos-Thread: not a thread id"))?;
        let thread = state
            .store
            .call(move |store| store.provider_thread(&thread_id))
            .await
            .map_err(|error| match error {
                StoreError::UnknownThread(_) => {
                    ApiError::bad_request("Kratos-Thread: no such thread")
                }
                error => error.into(),
            })?;
        Ok(Some(Acting(thread)))
    }

    fn access(&self) -> AccessLimit {
        self.0.settings.access()
    }

    pub(super) fn not_on_itself(&self, thread_id: &ThreadId) -> Result<(), ApiError> {
        if self.0.thread.id == *thread_id {
            return Err(forbidden(
                "That is your own thread. Kratos's tools act on other threads.",
            ));
        }
        Ok(())
    }

    /// A thread it creates gets no more access than it has.
    pub(super) fn may_create(&self, settings: &ThreadSettings) -> Result<(), ApiError> {
        if !self.access().covers(settings.access()) {
            return Err(forbidden(
                "A thread cannot create a thread with more permission than its own.",
            ));
        }
        Ok(())
    }

    /// Only a Full access thread decides another thread's approval. A lower one would approve what
    /// its own permission has to ask the user about.
    pub(super) fn may_decide(&self) -> Result<(), ApiError> {
        if self.access() != AccessLimit::FULL {
            return Err(forbidden(
                "Only a Full access thread may decide another thread's approval. Ask the user instead.",
            ));
        }
        Ok(())
    }

    /// What a message it sends to another thread records as its origin.
    pub(super) fn origin(&self) -> MessageOrigin {
        MessageOrigin::Thread {
            thread_id: self.0.thread.id.clone(),
            limit: self.access(),
        }
    }
}

fn forbidden(message: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, message)
}
