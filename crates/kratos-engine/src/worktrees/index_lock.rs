//! Git's lock on a worktree's index, which a commit holds as git's own commit does. Git commands
//! that stage files or make a commit take the same lock, so none runs while Kratos commits.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use super::PlaceError;

/// How long Kratos waits for a git command or reader that holds the index, such as an editor's
/// `git status`, which takes it for a moment to save what it learned.
const WAIT: Duration = Duration::from_secs(2);

const RETRY: Duration = Duration::from_millis(20);

#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;

/// The lock git takes on the index at `index`: the file `index.lock`, which exists while one git
/// command changes the index. Dropping it lets go of the lock.
pub(super) struct IndexLock {
    index: PathBuf,
    lock: PathBuf,
    is_held: bool,
}

pub(super) struct PreparedReplacement {
    lock_guard: Option<tokio::fs::File>,
    replace_guard: Option<tokio::fs::File>,
    lock: IndexLock,
}

impl Drop for PreparedReplacement {
    fn drop(&mut self) {
        // Release Windows DELETE share-check handles before `IndexLock::drop` removes `index.lock`.
        self.lock_guard = None;
        self.replace_guard = None;
    }
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

    pub(super) async fn prepare_replacement(
        self,
        updated: &Path,
    ) -> Result<PreparedReplacement, PlaceError> {
        copy_preserving_mtime(updated, &self.lock).await?;
        let replace_guard = reserve_for_replace(&self.index).await?;
        let lock_guard = reserve_for_replace(&self.lock).await?;
        Ok(PreparedReplacement {
            lock_guard,
            replace_guard,
            lock: self,
        })
    }
}

impl PreparedReplacement {
    pub(super) fn lock_path(&self) -> &Path {
        &self.lock.lock
    }

    pub(super) async fn commit(mut self) -> io::Result<()> {
        self.lock_guard = None;
        self.replace_guard = None;
        // Take ownership of cleanup before entering the blocking pool so cancellation cannot race
        // `IndexLock::drop` against an in-flight rename.
        self.lock.is_held = false;
        let lock_path = self.lock.lock.clone();
        let index_path = self.lock.index.clone();
        tokio::task::spawn_blocking(move || commit_rename_sync(&lock_path, &index_path))
            .await
            .map_err(io::Error::other)?
    }
}

fn commit_rename_sync(lock_path: &Path, index_path: &Path) -> io::Result<()> {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        match std::fs::rename(lock_path, index_path) {
            Ok(()) => return Ok(()),
            Err(error) if is_index_busy(&error) && std::time::Instant::now() < deadline => {
                std::thread::sleep(RETRY);
            }
            Err(error) => {
                let _ = std::fs::remove_file(lock_path);
                return Err(error);
            }
        }
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        // Once the lock file became the index, the name may already be another git command's lock.
        if self.is_held {
            let _ = std::fs::remove_file(&self.lock);
        }
    }
}

/// Copies `from` to `to` while keeping `from`'s modified time, so git's racy-index check stays
/// active on Linux where plain `fs::copy` writes a fresh timestamp.
pub(super) async fn copy_preserving_mtime(from: &Path, to: &Path) -> Result<(), PlaceError> {
    copy_mtime_inner(from, to, MissingSource::Error).await
}

/// Like [`copy_preserving_mtime`], returning `Ok(())` when `from` does not exist yet.
pub(super) async fn copy_preserving_mtime_if_present(
    from: &Path,
    to: &Path,
) -> Result<(), PlaceError> {
    copy_mtime_inner(from, to, MissingSource::Skip).await
}

#[derive(Clone, Copy)]
enum MissingSource {
    Error,
    Skip,
}

async fn copy_mtime_inner(
    from: &Path,
    to: &Path,
    missing: MissingSource,
) -> Result<(), PlaceError> {
    let from = from.to_owned();
    let to = to.to_owned();
    let dest = to.display().to_string();
    tokio::task::spawn_blocking(move || copy_mtime_sync(&from, &to, missing))
        .await
        .map_err(|error| PlaceError::Io {
            path: dest,
            source: io::Error::other(error),
        })?
}

fn copy_mtime_sync(from: &Path, to: &Path, missing: MissingSource) -> Result<(), PlaceError> {
    let mut src = match File::open(from) {
        Ok(file) => file,
        Err(error)
            if matches!(missing, MissingSource::Skip)
                && error.kind() == io::ErrorKind::NotFound =>
        {
            return Ok(());
        }
        Err(source) => {
            return Err(PlaceError::Io {
                path: from.display().to_string(),
                source,
            });
        }
    };
    let modified = src
        .metadata()
        .and_then(|meta| meta.modified())
        .map_err(|source| PlaceError::Io {
            path: from.display().to_string(),
            source,
        })?;
    let mut dst = File::create(to).map_err(|source| PlaceError::Io {
        path: to.display().to_string(),
        source,
    })?;
    io::copy(&mut src, &mut dst).map_err(|source| PlaceError::Io {
        path: to.display().to_string(),
        source,
    })?;
    dst.set_modified(modified).map_err(|source| PlaceError::Io {
        path: to.display().to_string(),
        source,
    })
}

/// On Windows, opens `path` with `DELETE` access and full sharing so any existing reader without
/// `FILE_SHARE_DELETE` fails this check before `git commit` runs, and new non-delete-sharing
/// readers cannot sneak in while `git commit` runs on the scratch index.
#[cfg(windows)]
async fn reserve_for_replace(path: &Path) -> Result<Option<tokio::fs::File>, PlaceError> {
    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;

    let deadline = Instant::now() + WAIT;
    loop {
        let opened = tokio::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_ALL)
            .open(path)
            .await;
        match opened {
            Ok(file) => return Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if is_index_busy(&error) && Instant::now() < deadline => {
                tokio::time::sleep(RETRY).await;
            }
            Err(error) if is_index_busy(&error) => {
                return Err(PlaceError::IndexInUse(path.display().to_string()));
            }
            Err(source) => {
                return Err(PlaceError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
    }
}

#[cfg(not(windows))]
async fn reserve_for_replace(_path: &Path) -> Result<Option<tokio::fs::File>, PlaceError> {
    Ok(None)
}

#[cfg(windows)]
fn is_index_busy(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        || error.raw_os_error() == Some(ERROR_SHARING_VIOLATION)
}

#[cfg(not(windows))]
fn is_index_busy(_error: &io::Error) -> bool {
    false
}
