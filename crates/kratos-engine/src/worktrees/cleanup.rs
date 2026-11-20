//! Removing a worktree Kratos made, once every thread that worked in it is archived (D16). Its
//! branch stays, and so do files the removal would delete unless the user chose to discard them.

use std::io;
use std::path::Path;

use kratos_protocol::{Cleanup, LeftFiles, Worktree};
use kratos_store::{Removal, RemovalEnd, StoreError};

use super::git::{self, CHECKOUT};
use super::{PlaceError, Worktrees};
use crate::store_thread::StoreThread;

impl Worktrees {
    /// Removes `worktree`, which Kratos made, unless a thread that is not archived still works in
    /// it. A removal git refuses stays marked as begun, so no thread joins the worktree until a
    /// later call removes it.
    pub(crate) async fn clean_up(
        &self,
        store: &StoreThread,
        worktree: &Worktree,
        files: LeftFiles,
    ) -> Result<Cleanup, StoreError> {
        let id = worktree.id.clone();
        let repository = match store.call(move |store| store.begin_removal(&id)).await? {
            Removal::InUse => return Ok(Cleanup::InUse),
            Removal::Done => return Ok(Cleanup::Removed),
            Removal::Started { repository } => repository,
        };
        let cleanup = self
            .remove(Path::new(&repository), &worktree.path, files)
            .await
            .unwrap_or_else(|error| Cleanup::Blocked {
                error: error.to_string(),
            });
        let end = match cleanup {
            Cleanup::Removed => RemovalEnd::Removed,
            Cleanup::Kept { .. } => RemovalEnd::Kept,
            _ => return Ok(cleanup),
        };
        let id = worktree.id.clone();
        store.call(move |store| store.end_removal(&id, end)).await?;
        Ok(cleanup)
    }

    /// Runs git in `repository`, the shared git folder, since the worktree's own folder goes.
    async fn remove(
        &self,
        repository: &Path,
        path: &str,
        files: LeftFiles,
    ) -> Result<Cleanup, PlaceError> {
        let lock = self.lock_of(repository);
        let _one_at_a_time = lock.lock().await;
        let folder = Path::new(path);
        let found = self.locate(folder).await?;
        if found.is_none_or(|worktree| worktree.top != folder) {
            return self.remove_leftover(repository, folder).await;
        }
        let status = [
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=normal",
        ];
        let left = self.git.run(folder, &status, git::QUICK).await?;
        let mut remove = vec!["worktree", "remove"];
        if !left.is_empty() {
            match files {
                LeftFiles::Keep => {
                    let files = left.lines().map(str::to_owned).collect();
                    return Ok(Cleanup::Kept { files });
                }
                LeftFiles::Discard => remove.push("--force"),
            }
        }
        remove.extend(["--", path]);
        self.git.run(repository, &remove, CHECKOUT).await?;
        Ok(Cleanup::Removed)
    }

    /// Removes what is left of a worktree that is no longer one, such as the folder a removal git
    /// could not finish leaves after deleting its `.git` file. Git would read the project's
    /// repository there, so Kratos removes the folder only while it is empty.
    async fn remove_leftover(
        &self,
        repository: &Path,
        folder: &Path,
    ) -> Result<Cleanup, PlaceError> {
        match tokio::fs::remove_dir(folder).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(PlaceError::CannotRemove {
                    path: folder.display().to_string(),
                    source: error,
                });
            }
            _ => {}
        }
        let prune = ["worktree", "prune"];
        self.git.run(repository, &prune, git::QUICK).await?;
        Ok(Cleanup::Removed)
    }
}
