use kratos_protocol::{
    ContextUsage, CreateThread, PauseReason, Permission, Provider, Thread, ThreadDetails,
    ThreadEventBody, ThreadId, ThreadSandbox, ThreadSettings, Timestamp, TurnChanges, TurnId,
    TurnOutcome, WorkspaceId,
};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

use crate::json::{from_column, to_column};
use crate::{Store, StoreError, ThreadWrite};

/// The names the `permission` column stores.
const PERMISSIONS: [(Permission, &str); 3] = [
    (Permission::AskMe, "askMe"),
    (Permission::ApproveForMe, "approveForMe"),
    (Permission::FullAccess, "fullAccess"),
];

/// What a provider needs to run a thread's turns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderThread {
    pub thread: Thread,
    /// The folder the agent works in: the workspace's path until R5 gives threads their own.
    pub folder: String,
    pub settings: ThreadSettings,
    pub sandbox: Option<ThreadSandbox>,
    pub paused: Option<PauseReason>,
    /// The turn the log shows running, which no process may be running any more.
    pub running_turn: Option<TurnId>,
    /// The provider's id for the thread, once a provider session has started one.
    pub native_id: Option<String>,
}

impl Store {
    /// Records a thread with a new UUID, `created_at = updated_at = now`, and the request's
    /// settings or the defaults. `Err(UnknownWorkspace)` when the workspace does not exist; nothing
    /// is written then.
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
        let settings = request.settings.clone().unwrap_or_default();
        // One statement, so the workspace cannot go missing between the check and the insert.
        let inserted = self.connection.execute(
            "INSERT INTO threads (id, workspace_id, title, provider, created_at, updated_at, model,
               permission, plan_mode, context_limit_tokens)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
             WHERE EXISTS (SELECT 1 FROM workspaces WHERE id = ?2)",
            params![
                thread.id.as_str(),
                thread.workspace_id.as_str(),
                thread.title,
                thread.provider.as_str(),
                thread.created_at.as_str(),
                thread.updated_at.as_str(),
                settings.model,
                permission_name(settings.permission),
                settings.plan_mode,
                settings.context_limit_tokens,
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

    /// The thread, its settings, and what its provider needs. `Err(UnknownThread)` when no thread
    /// has this id.
    pub fn provider_thread(&self, thread_id: &ThreadId) -> Result<ProviderThread, StoreError> {
        provider_thread(&self.connection, thread_id)
    }

    /// The thread, its settings, and what its next turn waits on.
    pub fn thread_details(&self, thread_id: &ThreadId) -> Result<ThreadDetails, StoreError> {
        let thread = self.provider_thread(thread_id)?;
        Ok(ThreadDetails {
            thread: thread.thread,
            settings: thread.settings,
            paused: thread.paused,
            running_turn: thread.running_turn,
            queue: self.pending_messages(thread_id)?,
            approvals: self.waiting_approvals(thread_id)?,
            questions: self.waiting_questions(thread_id)?,
        })
    }

    /// Every thread whose log shows a turn running, with that turn. After a restart no process
    /// runs any of them (L1).
    pub fn threads_with_running_turns(&self) -> Result<Vec<(ThreadId, TurnId)>, StoreError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, running_turn FROM threads WHERE running_turn IS NOT NULL ORDER BY rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((ThreadId::new(row.get(0)?), TurnId::new(row.get(1)?)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The thread a provider's own id belongs to.
    pub fn thread_for_native_id(
        &self,
        provider: Provider,
        native_id: &str,
    ) -> Result<Option<ThreadId>, StoreError> {
        let thread_id: Option<String> = self
            .connection
            .query_row(
                "SELECT thread_id FROM provider_sessions WHERE provider = ?1 AND native_id = ?2
                 ORDER BY started_at DESC LIMIT 1",
                params![provider.as_str(), native_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(thread_id.map(ThreadId::new))
    }
}

impl ThreadWrite<'_> {
    /// The thread as this write sees it.
    pub fn provider_thread(&self) -> Result<ProviderThread, StoreError> {
        provider_thread(&self.transaction, self.thread_id())
    }

    /// Replaces the thread's settings, which apply from its next turn (D87).
    pub fn set_settings(&mut self, settings: ThreadSettings) -> Result<(), StoreError> {
        self.transaction.execute(
            "UPDATE threads SET model = ?1, permission = ?2, plan_mode = ?3, context_limit_tokens = ?4
             WHERE id = ?5",
            params![
                settings.model,
                permission_name(settings.permission),
                settings.plan_mode,
                settings.context_limit_tokens,
                self.thread_id().as_str(),
            ],
        )?;
        self.append(ThreadEventBody::SettingsChanged { settings })?;
        Ok(())
    }

    /// Pauses the thread's queue for `reason`. Returns false, and records nothing, when it is
    /// already paused for the same reason.
    pub fn pause(&mut self, reason: PauseReason) -> Result<bool, StoreError> {
        if self.provider_thread()?.paused.as_ref() == Some(&reason) {
            return Ok(false);
        }
        self.transaction.execute(
            "UPDATE threads SET paused = ?1 WHERE id = ?2",
            params![to_column(&reason), self.thread_id().as_str()],
        )?;
        self.append(ThreadEventBody::ThreadPaused { reason })?;
        Ok(true)
    }

    /// Lets the thread's queue run again. Returns false, and records nothing, when it was not
    /// paused.
    pub fn resume(&mut self) -> Result<bool, StoreError> {
        let changed = self.transaction.execute(
            "UPDATE threads SET paused = NULL WHERE id = ?1 AND paused IS NOT NULL",
            [self.thread_id().as_str()],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        self.append(ThreadEventBody::ThreadResumed)?;
        Ok(true)
    }

    /// Records that `turn_id` started. It stays the thread's running turn until
    /// [`ThreadWrite::complete_turn`].
    pub fn start_turn(&mut self, turn_id: TurnId) -> Result<(), StoreError> {
        self.transaction.execute(
            "UPDATE threads SET running_turn = ?1 WHERE id = ?2",
            params![turn_id.as_str(), self.thread_id().as_str()],
        )?;
        self.append(ThreadEventBody::TurnStarted { turn_id })?;
        Ok(())
    }

    /// Records how `turn_id` ended and clears the running turn.
    pub fn complete_turn(
        &mut self,
        turn_id: TurnId,
        outcome: TurnOutcome,
        changes: TurnChanges,
        context: Option<ContextUsage>,
    ) -> Result<(), StoreError> {
        self.transaction.execute(
            "UPDATE threads SET running_turn = NULL WHERE id = ?1 AND running_turn = ?2",
            params![self.thread_id().as_str(), turn_id.as_str()],
        )?;
        self.append(ThreadEventBody::TurnCompleted {
            turn_id,
            outcome,
            changes,
            context,
        })?;
        Ok(())
    }

    /// Records that a provider session now serves the thread under the provider's own id, and the
    /// plan that pays for it (D24).
    pub fn begin_provider_session(
        &mut self,
        provider: Provider,
        native_id: &str,
        plan: Option<String>,
    ) -> Result<(), StoreError> {
        let now = self.at().as_str().to_owned();
        self.transaction.execute(
            "UPDATE provider_sessions SET ended_at = ?1 WHERE thread_id = ?2 AND ended_at IS NULL",
            params![now, self.thread_id().as_str()],
        )?;
        self.transaction.execute(
            "INSERT INTO provider_sessions (id, thread_id, provider, native_id, started_at, plan)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                Uuid::new_v4().to_string(),
                self.thread_id().as_str(),
                provider.as_str(),
                native_id,
                now,
                plan,
            ],
        )?;
        self.append(ThreadEventBody::ProviderSession { provider, plan })?;
        Ok(())
    }
}

fn provider_thread(
    connection: &Connection,
    thread_id: &ThreadId,
) -> Result<ProviderThread, StoreError> {
    let thread = connection
        .query_row(
            "SELECT threads.id, threads.workspace_id, threads.title, threads.provider,
               threads.created_at, threads.updated_at, workspaces.path, threads.model,
               threads.permission, threads.plan_mode, threads.context_limit_tokens, threads.sandbox,
               threads.paused, threads.running_turn,
               (SELECT native_id FROM provider_sessions
                WHERE thread_id = threads.id AND ended_at IS NULL
                ORDER BY started_at DESC LIMIT 1)
             FROM threads JOIN workspaces ON workspaces.id = threads.workspace_id
             WHERE threads.id = ?1",
            [thread_id.as_str()],
            provider_thread_from_row,
        )
        .optional()?;
    thread.ok_or_else(|| StoreError::UnknownThread(thread_id.clone()))
}

fn provider_thread_from_row(row: &Row<'_>) -> rusqlite::Result<ProviderThread> {
    Ok(ProviderThread {
        thread: thread_from_row(row)?,
        folder: row.get(6)?,
        settings: settings_from_row(row)?,
        sandbox: from_column(row, 11)?,
        paused: from_column(row, 12)?,
        running_turn: row.get::<_, Option<String>>(13)?.map(TurnId::new),
        native_id: row.get(14)?,
    })
}

/// Columns 7 to 10: `model, permission, plan_mode, context_limit_tokens`.
fn settings_from_row(row: &Row<'_>) -> rusqlite::Result<ThreadSettings> {
    let permission: String = row.get(8)?;
    let permission = PERMISSIONS
        .iter()
        .find(|(_, name)| *name == permission)
        .map(|(permission, _)| *permission)
        .ok_or_else(|| unknown_value(8, "permission", &permission))?;
    Ok(ThreadSettings {
        model: row.get(7)?,
        permission,
        plan_mode: row.get(9)?,
        context_limit_tokens: row.get(10)?,
    })
}

fn permission_name(permission: Permission) -> &'static str {
    PERMISSIONS
        .iter()
        .find(|(known, _)| *known == permission)
        .map(|(_, name)| *name)
        .expect("every permission has a stored name")
}

/// A row whose first six columns are `id, workspace_id, title, provider, created_at, updated_at`.
fn thread_from_row(row: &Row<'_>) -> rusqlite::Result<Thread> {
    let provider: String = row.get(3)?;
    Ok(Thread {
        id: ThreadId::new(row.get(0)?),
        workspace_id: WorkspaceId::new(row.get(1)?),
        title: row.get(2)?,
        provider: Provider::from_name(&provider)
            .ok_or_else(|| unknown_value(3, "provider", &provider))?,
        created_at: Timestamp::from_stored(row.get(4)?),
        updated_at: Timestamp::from_stored(row.get(5)?),
    })
}

fn unknown_value(column: usize, what: &str, value: &str) -> rusqlite::Error {
    let reason = format!("unknown {what} {value}");
    rusqlite::Error::FromSqlConversionFailure(column, Type::Text, reason.into())
}
