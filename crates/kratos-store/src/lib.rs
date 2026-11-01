//! The engine's SQLite records.
//!
//! [`Store::open`] locks the data directory, backs up a database that has an older schema, and runs
//! the migrations. The schema and SQL are the TypeScript engine's, so each engine opens what the
//! other wrote. The store is synchronous and owns one connection.

mod backups;
mod data_directory;
mod events;
mod migrations;
mod threads;
mod workspaces;

use std::path::{Path, PathBuf};

use kratos_protocol::{Seq, ThreadId, WorkspaceId};
use rusqlite::Connection;

pub use backups::{
    Backup, BackupError, BackupId, InvalidBackupId, RestoreError, Restored, restore,
};
pub use data_directory::DirectoryError;
pub use migrations::SCHEMA_VERSION;

use backups::back_up;
use data_directory::DataDirectory;

/// The engine's records in one data directory, held exclusively while this value lives.
pub struct Store {
    // Fields drop in declaration order: the connection closes, and checkpoints its log, before the
    // directory is released.
    connection: Connection,
    backup_taken: Option<Backup>,
    _directory: DataDirectory,
}

impl Store {
    /// Opens the records in `home`, creating the directory and `kratos.db` when they are missing.
    ///
    /// Refused while another `Store` (in any process) holds `home`, and for a database whose schema
    /// is newer than [`SCHEMA_VERSION`], which is left untouched. A database at versions 1 to
    /// `SCHEMA_VERSION - 1` is backed up before the first migration runs; a new database (version
    /// 0) and a current one are not.
    pub fn open(home: &Path) -> Result<Store, OpenError> {
        let directory = DataDirectory::lock(home)?;
        let database = directory.database_path();
        let at_database = |source| OpenError::Database {
            path: database.clone(),
            source,
        };
        let mut connection = Connection::open(&database).map_err(at_database)?;
        let found = migrations::schema_version(&connection).map_err(at_database)?;
        if found > SCHEMA_VERSION {
            return Err(OpenError::NewerSchema {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        configure(&connection).map_err(at_database)?;
        let backup_taken = if (1..SCHEMA_VERSION).contains(&found) {
            Some(back_up(&connection, &directory.backups_path(), found)?)
        } else {
            None
        };
        migrations::migrate(&mut connection, found)?;
        Ok(Store {
            connection,
            backup_taken,
            _directory: directory,
        })
    }

    pub fn backup_taken(&self) -> Option<&Backup> {
        self.backup_taken.as_ref()
    }
}

/// The TypeScript store's settings, plus `synchronous = FULL`. FULL is SQLite's default, written
/// down because a commit lost to a power cut would hand its seq to the next event, and a client
/// that already saw that seq would drop the new event as a duplicate.
fn configure(connection: &Connection) -> rusqlite::Result<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "FULL")
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Directory(#[from] DirectoryError),
    #[error("Database schema version {found} is newer than this build supports ({supported})")]
    NewerSchema { found: u32, supported: u32 },
    #[error("could not back up the database before migrating it, so it was not migrated: {0}")]
    Backup(#[from] BackupError),
    /// Each migration commits alone, so the database stays at the version before `version`.
    #[error("migration {version} failed and was rolled back: {source}")]
    Migration {
        version: u32,
        #[source]
        source: rusqlite::Error,
    },
    #[error("cannot open the database {}: {source}", path.display())]
    Database {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
}

/// The two `Unknown…` cases are expected absences that the API answers with 404; the rest are
/// failures.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("Unknown workspace {0}")]
    UnknownWorkspace(WorkspaceId),
    #[error("Unknown thread {0}")]
    UnknownThread(ThreadId),
    /// A stored event this build cannot read, such as a kind it does not know. Reading stops there
    /// rather than skipping the event, because a follower must never miss one.
    #[error("event {} of thread {thread_id} is unreadable: {source}", .seq.get())]
    UnreadableEvent {
        thread_id: ThreadId,
        seq: Seq,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}
