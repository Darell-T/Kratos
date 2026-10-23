use kratos_protocol::{CreateThread, Provider, Thread, ThreadId, Timestamp, WorkspaceId};
use rusqlite::types::Type;
use rusqlite::{Row, params};
use uuid::Uuid;

use crate::{Store, StoreError};

impl Store {
    /// Records a thread with a new UUID and `created_at = updated_at = now`.
    /// `Err(UnknownWorkspace)` when the workspace does not exist; nothing is written then.
    pub fn create_thread(&self, request: &CreateThread) -> Result<Thread, StoreError> {
        let now = Timestamp::now();
        let thread = Thread {
            id: ThreadId::new(Uuid::new_v4().to_string()),
            workspace_id: request.workspace_id.clone(),
            title: request.title.as_str().to_owned(),
            provider: request.provider,
            created_at: now.clone(),
            updated_at: now,
        };
        // One statement, so the workspace cannot go missing between the check and the insert.
        let inserted = self.connection.execute(
            "INSERT INTO threads (id, workspace_id, title, provider, created_at, updated_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6 WHERE EXISTS (SELECT 1 FROM workspaces WHERE id = ?2)",
            params![
                thread.id.as_str(),
                thread.workspace_id.as_str(),
                thread.title,
                thread.provider.as_str(),
                thread.created_at.as_str(),
                thread.updated_at.as_str(),
            ],
        )?;
        if inserted == 0 {
            return Err(StoreError::UnknownWorkspace(thread.workspace_id));
        }
        Ok(thread)
    }

    /// The workspace's threads, oldest first (by `created_at`, then insertion order).
    /// `Err(UnknownWorkspace)` when the workspace does not exist.
    pub fn list_threads(&self, workspace_id: &WorkspaceId) -> Result<Vec<Thread>, StoreError> {
        let workspace_exists: bool = self.connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM workspaces WHERE id = ?1)",
            [workspace_id.as_str()],
            |row| row.get(0),
        )?;
        if !workspace_exists {
            return Err(StoreError::UnknownWorkspace(workspace_id.clone()));
        }
        let mut statement = self.connection.prepare_cached(
            "SELECT id, workspace_id, title, provider, created_at, updated_at FROM threads
             WHERE workspace_id = ?1 ORDER BY created_at, rowid",
        )?;
        let threads = statement.query_map([workspace_id.as_str()], thread_from_row)?;
        Ok(threads.collect::<Result<_, _>>()?)
    }
}

/// A row selected as `id, workspace_id, title, provider, created_at, updated_at`.
fn thread_from_row(row: &Row<'_>) -> rusqlite::Result<Thread> {
    let provider: String = row.get(3)?;
    let unknown_provider = || {
        let reason = format!("unknown provider {provider}");
        rusqlite::Error::FromSqlConversionFailure(3, Type::Text, reason.into())
    };
    Ok(Thread {
        id: ThreadId::new(row.get(0)?),
        workspace_id: WorkspaceId::new(row.get(1)?),
        title: row.get(2)?,
        provider: Provider::from_name(&provider).ok_or_else(unknown_provider)?,
        created_at: Timestamp::from_stored(row.get(4)?),
        updated_at: Timestamp::from_stored(row.get(5)?),
    })
}
