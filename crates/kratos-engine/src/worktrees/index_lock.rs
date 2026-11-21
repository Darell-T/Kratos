//! Git's lock on a worktree's index, which a commit holds as git's own commit does. Git commands
//! that stage files or make a commit take the same lock, so none runs while Kratos commits.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use super::PlaceError;

/// How long Kratos waits for a git command that holds the lock, such as an editor's
/// `git status`, which takes it for a moment to save what it learned.
const WAIT: Duration = Duration::from_secs(2);

const RETRY: Duration = Duration::from_millis(20);

/// The lock git takes on the index at `index`: the file `index.lock`, which exists while one git
/// command changes the index. Dropping it lets go of the lock.
pub(super) struct IndexLock {
    index: PathBuf,
    lock: PathBuf,
    is_held: bool,
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

    /// Makes `updated` the index and lets go of the lock, as git does: the lock file takes its
    /// contents, then the index's place.
    pub(super) async fn replace_with(mut self, updated: &Path) -> io::Result<()> {
        tokio::fs::copy(updated, &self.lock).await?;
        let deadline = Instant::now() + WAIT;
        loop {
            match tokio::fs::rename(&self.lock, &self.index).await {
                // Windows refuses to replace a file another program has open, as a git command
                // reading the index does for a moment. Git for Windows waits the same way.
                Err(error)
                    if error.kind() == io::ErrorKind::PermissionDenied
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(RETRY).await;
                }
                renamed => {
                    self.is_held = renamed.is_err();
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
