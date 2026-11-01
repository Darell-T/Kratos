use rusqlite::{Connection, TransactionBehavior};

use crate::OpenError;

/// The schema steps, oldest first. Step `n` (counting from 1) takes the database from version
/// `n - 1` to `n`. Append only: a shipped step never changes, because databases in the field have
/// already run it.
const STEPS: [&str; 5] = [
    include_str!("../migrations/0001_workspaces_and_threads.sql"),
    include_str!("../migrations/0002_codex_threads.sql"),
    include_str!("../migrations/0003_claude_threads.sql"),
    include_str!("../migrations/0004_plan_mode.sql"),
    include_str!("../migrations/0005_thread_sandbox_and_usage.sql"),
];

/// The schema version this build writes: the number of steps.
pub const SCHEMA_VERSION: u32 = 5;
const _: () = assert!(STEPS.len() == SCHEMA_VERSION as usize);

pub(crate) fn schema_version(connection: &Connection) -> rusqlite::Result<u32> {
    connection.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Runs steps `from + 1 ..= SCHEMA_VERSION`. A failing step rolls back alone and stops the loop, so
/// the database keeps the last version that committed and a later open retries from there.
pub(crate) fn migrate(connection: &mut Connection, from: u32) -> Result<(), OpenError> {
    for (index, step) in STEPS.iter().enumerate().skip(from as usize) {
        let version = index as u32 + 1;
        apply(connection, version, step)
            .map_err(|source| OpenError::Migration { version, source })?;
    }
    Ok(())
}

fn apply(connection: &mut Connection, version: u32, step: &str) -> rusqlite::Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(step)?;
    transaction.pragma_update(None, "user_version", version)?;
    transaction.commit()
}
