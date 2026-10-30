use std::path::Path as FilePath;

use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use kratos_protocol::{
    CreateThread, CreateWorkspace, Health, ListEvents, ListThreads, Provider, Seq, ThreadId,
};

use super::acting::Acting;
use super::json::read_json;
use super::{ApiError, ServerState, json_reply};
use crate::hub::PAGE_BYTES;

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

/// `GET /api/threads/{id}/events?after=…`: 200 with up to a page of the thread's events after
/// `after`, oldest first. An empty page means the client has them all. An unknown thread answers
/// 404.
pub(super) async fn thread_events(
    State(state): State<ServerState>,
    Path(thread_id): Path<String>,
    query: Result<Query<ListEvents>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(ListEvents { after }) =
        query.map_err(|rejection| ApiError::bad_request(rejection.body_text()))?;
    let thread_id = ThreadId::new(thread_id);
    let events = state
        .store
        .call(move |store| {
            store.provider_thread(&thread_id)?;
            store.events_after(&thread_id, after.unwrap_or(Seq::BEFORE_FIRST), PAGE_BYTES)
        })
        .await?;
    Ok(json_reply(StatusCode::OK, &events))
}

/// `POST /api/threads`: 201 with the thread. Body (413 or 400), then the acting thread's access
/// (400 or 403), then a Codex thread's settings (400), then the workspace (404), then the insert.
pub(super) async fn create_thread(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let request: CreateThread = read_json(body).await?;
    if let Some(acting) = Acting::of(&state, &headers).await? {
        acting.may_create(&request.settings.clone().unwrap_or_default())?;
    }
    if let (Provider::Codex, Some(settings)) = (request.provider, &request.settings) {
        state.codex()?.check_settings(settings.clone()).await?;
    }
    let thread = state
        .store
        .call(move |store| store.create_thread(&request))
        .await?;
    Ok(json_reply(StatusCode::CREATED, &thread))
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
