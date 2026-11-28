use std::path::Path as FilePath;

use axum::body::Body;
use axum::extract::Path;
use axum::extract::State;
use axum::extract::rejection::PathRejection;
use axum::http::StatusCode;
use axum::response::Response;
use kratos_protocol::{CreateWorkspace, WorkspaceId};

use super::json::read_json;
use super::{ApiError, ServerState, json_reply, path};

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

/// `GET /api/workspaces/{id}/worktrees`: 200 with the worktrees of the project's repository other
/// than the project folder, for a new thread to work in. 404 for an unknown workspace, and 400 when
/// the project folder is not the top folder of a git repository.
pub(super) async fn list_worktrees(
    State(state): State<ServerState>,
    id: Result<Path<WorkspaceId>, PathRejection>,
) -> Result<Response, ApiError> {
    let workspace_id = path(id)?;
    let workspace = state
        .store
        .call(move |store| store.workspace(&workspace_id))
        .await?;
    let worktrees = state
        .worktrees
        .list(&state.store, FilePath::new(&workspace.path))
        .await?;
    Ok(json_reply(StatusCode::OK, &worktrees))
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
