//! Git's lock on a worktree's index, which a commit holds as git's own commit does. Git commands
//! that stage files or make a commit take the same lock, so none runs while Kratos commits.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use super::PlaceError;

/// How long Kratos waits for a git command or reader that holds the index, such as an editor's
/// `git status`, which takes it for a moment to save what it learned.
const WAIT: Duration = Duration::from_secs(2);

const RETRY: Duration = Duration::from_millis(20);

/// Win32 `ERROR_SHARING_VIOLATION`, returned when another process holds a file open without
/// `FILE_SHARE_DELETE`.
#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;

/// The lock git takes on the index at `index`: the file `index.lock`, which exists while one git
/// command changes the index. Dropping it lets go of the lock.
pub(super) struct IndexLock {
    index: PathBuf,
    lock: PathBuf,
    is_held: bool,
}

/// An [`IndexLock`] whose lock file already holds the replacement index bytes and (on Windows)
/// holds a `DELETE` share-check handle on the target index so a reader without `FILE_SHARE_DELETE`
/// is caught before `git commit` advances `HEAD`.
pub(super) struct PreparedReplacement {
    // Dropped before `lock` so the target index handle is released before any teardown.
    replace_guard: Option<tokio::fs::File>,
    lock: IndexLock,
}

impl IndexLock {
    /// Takes the lock, waiting a moment for a git command that holds it.
    pub(super) async fn take(index: &Path) -> Result<IndexLock, PlaceError> {
        let mut lock = index.as_os_str().to_owned();
        lock.push(".lock");
        let lock = PathBuf::from(lock);
        let deadline = Instant::now() + WAIT;
        loop {
            let created = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
                .await;
            match created {
                Ok(_) => {
                    return Ok(IndexLock {
                        index: index.to_owned(),
                        lock,
                        is_held: true,
                    });
                }
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                    return Err(PlaceError::Io {
                        path: lock.display().to_string(),
                        source: error,
                    });
                }
                Err(_) if Instant::now() >= deadline => {
                    return Err(PlaceError::IndexInUse(index.display().to_string()));
                }
                Err(_) => tokio::time::sleep(RETRY).await,
            }
        }
    }

    /// Stages `updated` into the lock file and verifies the target index can be replaced before
    /// `git commit` advances `HEAD`.
    pub(super) async fn prepare_replacement(
        self,
        updated: &Path,
    ) -> Result<PreparedReplacement, PlaceError> {
        tokio::fs::copy(updated, &self.lock)
            .await
            .map_err(|source| PlaceError::Io {
                path: self.lock.display().to_string(),
                source,
            })?;
        let replace_guard = reserve_for_replace(&self.index).await?;
        Ok(PreparedReplacement {
            replace_guard,
            lock: self,
        })
    }
}

impl PreparedReplacement {
    /// Atomically renames the prepared lock file into the index's place and releases the lock.
    pub(super) async fn commit(mut self) -> io::Result<()> {
        drop(self.replace_guard.take());
        let deadline = Instant::now() + WAIT;
        loop {
            match tokio::fs::rename(&self.lock.lock, &self.lock.index).await {
                // Windows refuses to replace a file another program has open without
                // `FILE_SHARE_DELETE`. Retry briefly before reporting the failure.
                Err(error) if is_index_busy(&error) && Instant::now() < deadline => {
                    tokio::time::sleep(RETRY).await;
                }
                renamed => {
                    self.lock.is_held = renamed.is_err();
                    return renamed;
                }
            }
        }
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        // Once the lock file became the index, the name may already be another git command's
        // lock.
        if self.is_held {
            let _ = std::fs::remove_file(&self.lock);
        }
    }
}

/// On Windows, opens `index` with `DELETE` access and full sharing so any existing reader without
/// `FILE_SHARE_DELETE` fails this check before `git commit` runs, and new non-delete-sharing
/// readers cannot sneak in while `git commit` runs on the scratch index.
#[cfg(windows)]
async fn reserve_for_replace(index: &Path) -> Result<Option<tokio::fs::File>, PlaceError> {
    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;

    let deadline = Instant::now() + WAIT;
    loop {
        let opened = tokio::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_ALL)
            .open(index)
            .await;
        match opened {
            Ok(file) => return Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if is_index_busy(&error) && Instant::now() < deadline => {
                tokio::time::sleep(RETRY).await;
            }
            Err(error) if is_index_busy(&error) => {
                return Err(PlaceError::IndexInUse(index.display().to_string()));
            }
            Err(source) => {
                return Err(PlaceError::Io {
                    path: index.display().to_string(),
                    source,
                });
            }
        }
    }
}

#[cfg(not(windows))]
async fn reserve_for_replace(_index: &Path) -> Result<Option<tokio::fs::File>, PlaceError> {
    Ok(None)
}

fn is_index_busy(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(windows)]
    if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
        return true;
    }
    false
}
