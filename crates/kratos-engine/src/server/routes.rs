use std::path::Path as FilePath;

use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use kratos_protocol::{
    CreateThread, CreateWorkspace, Health, ListThreads, PostMessage, ThreadEventBody, ThreadId,
};

use super::json::read_json;
use super::{ApiError, ServerState, json_reply};

pub(super) async fn health() -> Response {
    json_reply(StatusCode::OK, &Health { ok: true })
}

pub(super) async fn not_found() -> ApiError {
    ApiError::not_found()
}

/// `GET /api/workspaces`: 200 with every workspace, oldest first.
pub(super) async fn list_workspaces(
    State(state): State<ServerState>,
) -> Result<Response, ApiError> {
    let workspaces = state.store.call(|store| store.list_workspaces()).await?;
    Ok(json_reply(StatusCode::OK, &workspaces))
}

/// `POST /api/workspaces`: 201 with the workspace. Body (413 or 400), then the path check (400),
/// then the insert.
pub(super) async fn create_workspace(
    State(state): State<ServerState>,
    body: Body,
) -> Result<Response, ApiError> {
    let request: CreateWorkspace = read_json(body).await?;
    require_directory(request.path.as_str()).await?;
    let workspace = state
        .store
        .call(move |store| store.create_workspace(&request))
        .await?;
    Ok(json_reply(StatusCode::CREATED, &workspace))
}

/// `GET /api/threads?workspaceId=…`: 200 with the workspace's threads. A missing or empty
/// `workspaceId` answers 400, an unknown workspace 404.
pub(super) async fn list_threads(
    State(state): State<ServerState>,
    query: Result<Query<ListThreads>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(ListThreads { workspace_id }) =
        query.map_err(|rejection| ApiError::bad_request(rejection.body_text()))?;
    let threads = state
        .store
        .call(move |store| store.list_threads(&workspace_id))
        .await?;
    Ok(json_reply(StatusCode::OK, &threads))
}

/// `POST /api/threads`: 201 with the thread. Body (413 or 400), then the workspace (404), then the
/// insert.
pub(super) async fn create_thread(
    State(state): State<ServerState>,
    body: Body,
) -> Result<Response, ApiError> {
    let request: CreateThread = read_json(body).await?;
    let thread = state
        .store
        .call(move |store| store.create_thread(&request))
        .await?;
    Ok(json_reply(StatusCode::CREATED, &thread))
}

/// `POST /api/threads/{id}/messages`: 201 with the appended event.
///
/// The thread is checked before the body is read: a post to an unknown thread answers 404 without
/// reading or size-checking its body.
pub(super) async fn post_message(
    State(state): State<ServerState>,
    id: Result<Path<ThreadId>, PathRejection>,
    body: Body,
) -> Result<Response, ApiError> {
    let Path(thread_id) = id.map_err(|_| ApiError::bad_request("Malformed URL path"))?;
    let lookup = thread_id.clone();
    state
        .store
        .call(move |store| store.last_seq(&lookup))
        .await?;
    let PostMessage { text } = read_json(body).await?;
    let body = ThreadEventBody::UserMessage {
        text: text.into_string(),
    };
    let event = state.hub.append(thread_id, body).await?;
    Ok(json_reply(StatusCode::CREATED, &event))
}

/// On Windows `Path::is_absolute` refuses `\folder` and `/folder`, which Node accepts as absolute
/// but which depend on the engine's current drive.
async fn require_directory(path: &str) -> Result<(), ApiError> {
    let is_directory = FilePath::new(path).is_absolute()
        && tokio::fs::metadata(path)
            .await
            .is_ok_and(|metadata| metadata.is_dir());
    if is_directory {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "Path is not an existing absolute directory: {path}"
        )))
    }
}
