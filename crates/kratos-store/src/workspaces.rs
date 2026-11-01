use kratos_protocol::{CreateWorkspace, Timestamp, Workspace, WorkspaceId};
use rusqlite::{Row, params};
use uuid::Uuid;

use crate::{Store, StoreError};

impl Store {
    /// Records a workspace with a new UUID and `created_at = now`. The route has already checked
    /// that the path names an existing absolute directory; the store records the trimmed text as
    /// sent.
    pub fn create_workspace(&self, request: &CreateWorkspace) -> Result<Workspace, StoreError> {
        let workspace = Workspace {
            id: WorkspaceId::new(Uuid::new_v4().to_string()),
            name: request.name.as_str().to_owned(),
            path: request.path.as_str().to_owned(),
            created_at: Timestamp::now(),
        };
        self.connection.execute(
            "INSERT INTO workspaces (id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                workspace.id.as_str(),
                workspace.name,
                workspace.path,
                workspace.created_at.as_str()
            ],
        )?;
        Ok(workspace)
    }

    /// Every workspace, oldest first (by `created_at`, then insertion order).
    pub fn list_workspaces(&self) -> Result<Vec<Workspace>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, name, path, created_at FROM workspaces ORDER BY created_at, rowid",
        )?;
        let workspaces = statement.query_map([], workspace_from_row)?;
        Ok(workspaces.collect::<Result<_, _>>()?)
    }
}

/// A row selected as `id, name, path, created_at`.
fn workspace_from_row(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: WorkspaceId::new(row.get(0)?),
        name: row.get(1)?,
        path: row.get(2)?,
        created_at: Timestamp::from_stored(row.get(3)?),
    })
}
