//! The thread records: the list, a new thread, a thread's details, and its event pages. The routes
//! that drive a thread's agent are in `agents`.

use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use kratos_protocol::{
    ACTING_THREAD_HEADER, AccessLimit, CreateThread, ListEvents, ListThreads, Seq, ThreadId,
};

use super::acting::Acting;
use super::agent::Drive;
use super::json::read_json;
use super::{ApiError, ServerState, ThreadPath, json_reply, path};
use crate::hub::PAGE_BYTES;

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

/// `POST /api/threads`: 201 with the thread. Body (413 or 400), then the acting thread's access
/// (400 or 403), then the settings the thread's provider supports (400), then the workspace (404),
/// then the insert.
pub(super) async fn create_thread(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let request: CreateThread = read_json(body).await?;
    if let Some(acting) = Acting::of(&state, &headers).await? {
        acting.may_create(&request.settings_or_default())?;
    }
    if let Some(settings) = &request.settings
        && let Drive::Agent(agent) = state.drive(request.provider)?
    {
        agent.check_settings(settings.clone()).await?;
    }
    let thread = state
        .store
        .call(move |store| store.create_thread(&request))
        .await?;
    Ok(json_reply(StatusCode::CREATED, &thread))
}

/// `GET /api/threads/{id}`: 200 with the thread, its settings, its queue, and what waits on the
/// user.
pub(super) async fn thread_details(
    State(state): State<ServerState>,
    headers: HeaderMap,
    id: ThreadPath,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    let access = if headers
        .get(ACTING_THREAD_HEADER)
        .and_then(|value| value.to_str().ok())
        == Some(thread_id.as_str())
    {
        Acting::of(&state, &headers)
            .await?
            .map(|acting| acting.access)
    } else {
        None
    };
    details(&state, thread_id, access).await
}

pub(super) async fn details(
    state: &ServerState,
    thread_id: ThreadId,
    access: Option<AccessLimit>,
) -> Result<Response, ApiError> {
    let mut details = state
        .store
        .call(move |store| store.thread_details(&thread_id))
        .await?;
    if let Some(access) = access {
        details.settings.permission = access.permission;
        details.settings.plan_mode = access.plan_mode;
    }
    Ok(json_reply(StatusCode::OK, &details))
}

/// `GET /api/threads/{id}/events?after=…`: 200 with up to a page of the thread's events after
/// `after`, oldest first. An empty page means the client has them all. An unknown thread answers
/// 404.
pub(super) async fn thread_events(
    State(state): State<ServerState>,
    id: ThreadPath,
    query: Result<Query<ListEvents>, QueryRejection>,
) -> Result<Response, ApiError> {
    let thread_id = path(id)?;
    let Query(ListEvents { after }) =
        query.map_err(|rejection| ApiError::bad_request(rejection.body_text()))?;
    let events = state
        .store
        .call(move |store| {
            store.provider_thread(&thread_id)?;
            store.events_after(&thread_id, after.unwrap_or(Seq::BEFORE_FIRST), PAGE_BYTES)
        })
        .await?;
    Ok(json_reply(StatusCode::OK, &events))
}
