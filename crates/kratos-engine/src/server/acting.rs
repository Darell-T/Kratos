//! Only Full access permits thread tools to act across workspaces. All threads may read across workspaces.

use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use kratos_protocol::{
    ACTING_THREAD_HEADER, AccessLimit, MessageOrigin, ThreadId, ThreadSettings, WorkspaceId,
};
use kratos_store::StoreError;

use super::{ApiError, ServerState};

/// Answers 403 to a request that names a thread, whatever the header holds, before the route runs.
pub(super) async fn refuse_agents(request: Request, next: Next) -> Response {
    if request.headers().contains_key(ACTING_THREAD_HEADER) {
        return forbidden("Kratos's tools cannot call this route. Only the user can.")
            .into_response();
    }
    next.run(request).await
}

pub(super) struct Acting {
    thread_id: ThreadId,
    workspace_id: WorkspaceId,
    pub(super) access: AccessLimit,
}

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
        let access = state
            .agent_of(thread.thread.provider)?
            .access(thread.thread.id.clone())
            .await?;
        Ok(Some(Acting {
            thread_id: thread.thread.id,
            workspace_id: thread.thread.workspace_id,
            access,
        }))
    }

    pub(super) fn not_on_itself(&self, thread_id: &ThreadId) -> Result<(), ApiError> {
        if self.thread_id == *thread_id {
            return Err(forbidden(
                "That is your own thread. Kratos's tools act on other threads.",
            ));
        }
        Ok(())
    }

    pub(super) fn in_workspace(&self, workspace_id: &WorkspaceId) -> Result<(), ApiError> {
        if self.access != AccessLimit::FULL && self.workspace_id != *workspace_id {
            return Err(forbidden(
                "Below Full access, Kratos's tools act only on threads in your own workspace. Ask the user to act in another one.",
            ));
        }
        Ok(())
    }

    pub(super) async fn reaches(
        &self,
        state: &ServerState,
        thread_id: &ThreadId,
    ) -> Result<(), ApiError> {
        let target = thread_id.clone();
        let thread = state
            .store
            .call(move |store| store.provider_thread(&target))
            .await?;
        self.in_workspace(&thread.thread.workspace_id)
    }

    /// A thread it creates gets no more access than it has.
    pub(super) fn may_create(&self, settings: &ThreadSettings) -> Result<(), ApiError> {
        if !self.access.covers(settings.access()) {
            return Err(forbidden(
                "A thread cannot create a thread with more permission than its own.",
            ));
        }
        Ok(())
    }

    /// Only a Full access thread decides another thread's approval. A lower one would approve what
    /// its own permission has to ask the user about.
    pub(super) fn may_decide(&self) -> Result<(), ApiError> {
        if self.access != AccessLimit::FULL {
            return Err(forbidden(
                "Only a Full access thread may decide another thread's approval. Ask the user instead.",
            ));
        }
        Ok(())
    }

    /// What a message it sends to another thread records as its origin.
    pub(super) fn origin(&self) -> MessageOrigin {
        MessageOrigin::Thread {
            thread_id: self.thread_id.clone(),
            limit: self.access,
        }
    }
}

fn forbidden(message: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, message)
}
