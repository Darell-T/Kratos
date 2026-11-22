//! Git's lock on a worktree's index, which a commit holds as git's own commit does. Git commands
//! that stage files or make a commit take the same lock, so none runs while Kratos commits.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use super::PlaceError;

/// How long Kratos waits for another git command's `index.lock` or a short-lived Windows reader
/// blocking the index rename, such as an editor's `git status`.
const WAIT: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_millis(20);

#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;

pub(super) struct IndexLock {
    index: PathBuf,
    lock: PathBuf,
    is_held: bool,
}

pub(super) struct PreparedReplacement(IndexLock);

impl IndexLock {
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
                    return Err(PlaceError::io(&lock, error));
                }
                Err(_) if Instant::now() >= deadline => {
                    return Err(PlaceError::IndexInUse(index.display().to_string()));
                }
                Err(_) => tokio::time::sleep(RETRY).await,
            }
        }
    }

    /// Copies the locked index to `to` while preserving its modified time so git's racy-index
    /// check still hashes same-second working edits.
    pub(super) async fn copy_index_to(&self, to: &Path) -> Result<(), PlaceError> {
        copy_mtime(&self.index, to, MissingSource::Skip).await
    }

    pub(super) async fn prepare_replacement(
        self,
        updated: &Path,
    ) -> Result<PreparedReplacement, PlaceError> {
        copy_mtime(updated, &self.lock, MissingSource::Error).await?;
        Ok(PreparedReplacement(self))
    }
}

impl PreparedReplacement {
    pub(super) fn lock_path(&self) -> &Path {
        &self.0.lock
    }

    pub(super) async fn commit(self) -> io::Result<()> {
        let mut lock = self.0;
        tokio::task::spawn_blocking(move || commit_rename_sync(&mut lock))
            .await
            .map_err(io::Error::other)?
    }
}

fn commit_rename_sync(lock: &mut IndexLock) -> io::Result<()> {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        match std::fs::rename(&lock.lock, &lock.index) {
            Ok(()) => {
                lock.is_held = false;
                return Ok(());
            }
            Err(error) if is_index_busy(&error) && std::time::Instant::now() < deadline => {
                std::thread::sleep(RETRY);
            }
            Err(error) => return Err(error),
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

#[derive(Clone, Copy)]
enum MissingSource {
    Error,
    Skip,
}

async fn copy_mtime(from: &Path, to: &Path, missing: MissingSource) -> Result<(), PlaceError> {
    let from = from.to_owned();
    let to_owned = to.to_owned();
    tokio::task::spawn_blocking(move || copy_mtime_sync(&from, &to_owned, missing))
        .await
        .map_err(|error| PlaceError::io(to, io::Error::other(error)))?
}

fn copy_mtime_sync(from: &Path, to: &Path, missing: MissingSource) -> Result<(), PlaceError> {
    let from_err = |source| PlaceError::io(from, source);
    let to_err = |source| PlaceError::io(to, source);
    let mut src = match File::open(from) {
        Ok(file) => file,
        Err(error)
            if matches!(missing, MissingSource::Skip)
                && error.kind() == io::ErrorKind::NotFound =>
        {
            return Ok(());
        }
        Err(source) => return Err(from_err(source)),
    };
    let modified = src
        .metadata()
        .and_then(|meta| meta.modified())
        .map_err(from_err)?;
    let mut dst = File::create(to).map_err(to_err)?;
    io::copy(&mut src, &mut dst).map_err(to_err)?;
    dst.set_modified(modified).map_err(to_err)
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
