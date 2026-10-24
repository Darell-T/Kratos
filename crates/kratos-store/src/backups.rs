use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use rusqlite::{Connection, OpenFlags};
use uuid::Uuid;
use uuid::fmt::Hyphenated;

use crate::data_directory::{DataDirectory, DirectoryError};
use crate::migrations::schema_version;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackupId(Uuid);

impl BackupId {
    fn new() -> BackupId {
        BackupId(Uuid::now_v7())
    }
}

impl fmt::Display for BackupId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), formatter)
    }
}

impl FromStr for BackupId {
    type Err = InvalidBackupId;

    /// Accepts the hyphenated form [`BackupId`] prints. `Uuid::try_parse` alone also takes forms
    /// that no backup file name uses.
    fn from_str(text: &str) -> Result<BackupId, InvalidBackupId> {
        let invalid = || InvalidBackupId(text.to_owned());
        if text.len() != Hyphenated::LENGTH {
            return Err(invalid());
        }
        Uuid::try_parse(text).map(BackupId).map_err(|_| invalid())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("not a backup id: {0:?}")]
pub struct InvalidBackupId(String);

/// The file is `backups/kratos-v{source_version}-{id}.db`. The copy's own `user_version` is
/// `source_version` (VACUUM keeps it), so the build that wrote that version opens the copy
/// unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backup {
    pub id: BackupId,
    pub source_version: u32,
    pub path: PathBuf,
}

/// `VACUUM INTO` writes `….db.partial`, which becomes `….db` only after SQLite reports success, so
/// a backup exists exactly when its final name does. A failed copy leaves a `.partial` file that
/// nothing reads. Every id is new and `VACUUM INTO` refuses an existing target, so no backup is
/// ever overwritten.
pub(crate) fn back_up(
    connection: &Connection,
    backups: &Path,
    source_version: u32,
) -> Result<Backup, BackupError> {
    fs::create_dir_all(backups).map_err(backup_io_error(backups))?;
    let id = BackupId::new();
    let path = backups.join(format!("kratos-v{source_version}-{id}.db"));
    let partial = path.with_added_extension("partial");
    let copy_error = |source: rusqlite::Error| BackupError::Copy {
        path: partial.clone(),
        source,
    };
    let target = partial
        .to_str()
        .ok_or_else(|| copy_error(rusqlite::Error::InvalidPath(partial.clone())))?;
    connection
        .execute("VACUUM INTO ?1", [target])
        .map_err(copy_error)?;
    fs::rename(&partial, &path).map_err(backup_io_error(&path))?;
    Ok(Backup {
        id,
        source_version,
        path,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    pub restored: Backup,
    /// The live database that the restore replaced, saved as an ordinary backup so `restore` can
    /// bring it back. `None` when `home` had no database.
    pub replaced: Option<Backup>,
}

/// Makes backup `id` the live database of `home`. Refused while an engine holds `home`.
///
/// The restore is for going back to the build that wrote the backup, for example after a migration
/// went wrong. It checks the backup first, then saves the live database, which therefore has to
/// open. Nothing is replaced until the copy of the backup is complete, and running the restore
/// again after a crash finishes the job.
pub fn restore(home: &Path, id: &BackupId) -> Result<Restored, RestoreError> {
    let directory = DataDirectory::lock(home)?;
    let restored = find_backup(&directory.backups_path(), id)?;
    verify(&restored)?;
    let replaced = save_live_database(&directory)?;
    install(&directory, &restored)?;
    Ok(Restored { restored, replaced })
}

fn find_backup(backups: &Path, id: &BackupId) -> Result<Backup, RestoreError> {
    let unknown = || RestoreError::UnknownBackup {
        id: *id,
        folder: backups.to_path_buf(),
    };
    let entries = fs::read_dir(backups).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => unknown(),
        _ => RestoreError::Io {
            path: backups.to_path_buf(),
            source,
        },
    })?;
    for entry in entries {
        let path = entry.map_err(restore_io_error(backups))?.path();
        let name = path.file_name().and_then(|name| name.to_str());
        if let Some(source_version) = name.and_then(|name| source_version_of(name, id)) {
            return Ok(Backup {
                id: *id,
                source_version,
                path,
            });
        }
    }
    Err(unknown())
}

fn source_version_of(file_name: &str, id: &BackupId) -> Option<u32> {
    file_name
        .strip_prefix("kratos-v")?
        .strip_suffix(&format!("-{id}.db"))?
        .parse()
        .ok()
}

fn verify(backup: &Backup) -> Result<(), RestoreError> {
    let damaged = |reason: String| RestoreError::DamagedBackup {
        path: backup.path.clone(),
        reason,
    };
    let from_sqlite = |error: rusqlite::Error| damaged(error.to_string());
    let connection = Connection::open_with_flags(&backup.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(from_sqlite)?;
    let integrity: String = connection
        .pragma_query_value(None, "integrity_check", |row| row.get(0))
        .map_err(from_sqlite)?;
    let version = schema_version(&connection).map_err(from_sqlite)?;
    if integrity != "ok" {
        return Err(damaged(format!(
            "the integrity check reported: {integrity}"
        )));
    }
    if version != backup.source_version {
        return Err(damaged(format!(
            "it holds schema version {version}, but its name says {}",
            backup.source_version
        )));
    }
    Ok(())
}

/// Closing the last connection checkpoints the write-ahead log into the main file and deletes
/// `kratos.db-wal` and `kratos.db-shm`, so nothing of the old database can be replayed into the
/// restored one. A log that survives the close means another program, such as a `sqlite3` shell,
/// still has the database open.
fn save_live_database(directory: &DataDirectory) -> Result<Option<Backup>, RestoreError> {
    let database = directory.database_path();
    if !database.try_exists().map_err(restore_io_error(&database))? {
        return Ok(None);
    }
    let live = Connection::open(&database)?;
    let saved = back_up(&live, &directory.backups_path(), schema_version(&live)?)?;
    live.close().map_err(|(_, source)| source)?;
    if wal_path(&database)
        .try_exists()
        .map_err(restore_io_error(&database))?
    {
        return Err(RestoreError::DatabaseInUse(database));
    }
    Ok(Some(saved))
}

fn install(directory: &DataDirectory, backup: &Backup) -> Result<(), RestoreError> {
    let database = directory.database_path();
    let staged = database.with_added_extension("restoring");
    fs::copy(&backup.path, &staged).map_err(restore_io_error(&staged))?;
    File::options()
        .write(true)
        .open(&staged)
        .and_then(|file| file.sync_all())
        .map_err(restore_io_error(&staged))?;
    fs::rename(&staged, &database).map_err(restore_io_error(&database))
}

fn wal_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

fn backup_io_error(path: &Path) -> impl FnOnce(io::Error) -> BackupError {
    let path = path.to_path_buf();
    move |source| BackupError::Io { path, source }
}

fn restore_io_error(path: &Path) -> impl FnOnce(io::Error) -> RestoreError {
    let path = path.to_path_buf();
    move |source| RestoreError::Io { path, source }
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("cannot write the backup {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("VACUUM INTO {} failed: {source}", path.display())]
    Copy {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error(transparent)]
    Directory(#[from] DirectoryError),
    #[error("there is no backup {id} in {}", folder.display())]
    UnknownBackup { id: BackupId, folder: PathBuf },
    #[error("the backup {} cannot be restored: {reason}", path.display())]
    DamagedBackup { path: PathBuf, reason: String },
    #[error("could not save the database the restore would replace, so nothing was restored: {0}")]
    SaveLive(#[from] BackupError),
    #[error("another program has {} open; close it and restore again", .0.display())]
    DatabaseInUse(PathBuf),
    #[error("restore failed at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot open the database being replaced: {0}")]
    LiveDatabase(#[from] rusqlite::Error),
}
