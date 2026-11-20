//! The routes for a thread's finished work (D27, D55): its diff, a commit, and a pull request.
//! Each works in the thread's folder, archived or not, so the user can still commit what an
//! archive kept.

use std::path::Path as FilePath;

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use kratos_protocol::{CommitThread, Committed, CreatePullRequest, Place, PullRequest};
use kratos_store::ProviderThread;

use super::json::read_json;
use super::{ApiError, ServerState, ThreadPath, json_reply, path};

/// `GET /api/threads/{id}/diff`: 200 with what the thread's folder holds that its base does not.
pub(super) async fn diff(
    State(state): State<ServerState>,
    id: ThreadPath,
) -> Result<Response, ApiError> {
    let thread = thread(&state, id).await?;
    let base = match &thread.place {
        Place::Worktree(worktree) => worktree.base.as_deref(),
        Place::ProjectFolder => None,
    };
    let diff = state
        .worktrees
        .diff(FilePath::new(&thread.folder), base)
        .await?;
    Ok(json_reply(StatusCode::OK, &diff))
}

/// `POST /api/threads/{id}/commit`: 201 with the new commit. 409 while a turn runs, since its
/// agent may be halfway through an edit, and 400 when nothing changed.
pub(super) async fn commit(
    State(state): State<ServerState>,
    id: ThreadPath,
    body: Body,
) -> Result<Response, ApiError> {
    let thread = thread(&state, id).await?;
    let CommitThread { message } = read_json(body).await?;
    if thread.running_turn.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "A turn is running in this thread. Commit once it ends, or stop it first",
        ));
    }
    let commit = state
        .worktrees
        .commit(FilePath::new(&thread.folder), message.as_str())
        .await?;
    Ok(json_reply(StatusCode::CREATED, &Committed { commit }))
}

/// `POST /api/threads/{id}/pull-request`: 201 with the pull request's address, once the thread's
/// branch is pushed to `origin` and `gh` opened it.
pub(super) async fn pull_request(
    State(state): State<ServerState>,
    id: ThreadPath,
    body: Body,
) -> Result<Response, ApiError> {
    let thread = thread(&state, id).await?;
    let CreatePullRequest { title, body } = read_json(body).await?;
    let url = state
        .worktrees
        .pull_request(FilePath::new(&thread.folder), title.as_str(), &body)
        .await?;
    Ok(json_reply(StatusCode::CREATED, &PullRequest { url }))
}

async fn thread(state: &ServerState, id: ThreadPath) -> Result<ProviderThread, ApiError> {
    let thread_id = path(id)?;
    Ok(state
        .store
        .call(move |store| store.provider_thread(&thread_id))
        .await?)
}
