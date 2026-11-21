//! Removing a worktree Kratos made, once every thread that worked in it is archived (D16). Its
//! branch stays, and so do files the removal would delete unless the user chose to discard them.

use std::io;
use std::path::Path;

use kratos_protocol::{Cleanup, LeftFiles, Ownership, Place, WorktreeId};
use kratos_store::{Removal, RemovalEnd, StoreError, UnfinishedWorktree};

use super::git::{self, CHECKOUT};
use super::{PlaceError, Worktrees};
use crate::store_thread::StoreThread;

impl Worktrees {
    /// Removes the worktree at `place` when Kratos made it and no thread that is not archived
    /// still works in it. The project folder and borrowed worktrees are never removed. A removal
    /// git refuses stays marked as begun, so no thread joins the worktree until a later call
    /// removes it.
    pub(crate) async fn clean_up(
        &self,
        store: &StoreThread,
        place: &Place,
        files: LeftFiles,
    ) -> Result<Cleanup, StoreError> {
        match place {
            Place::Worktree(worktree) if worktree.ownership == Ownership::CreatedByKratos => {
                self.remove_worktree(store, &worktree.id, &worktree.path, files)
                    .await
            }
            Place::Worktree(_) | Place::ProjectFolder => Ok(Cleanup::NotOwned),
        }
    }

    /// Finishes what an earlier engine left, `unfinished`, read before this engine served. Files
    /// left in a worktree keep it, since only the user may discard them.
    pub async fn sweep(&self, store: &StoreThread, unfinished: Vec<UnfinishedWorktree>) {
        for UnfinishedWorktree { id, path } in unfinished {
            match self
                .remove_worktree(store, &id, &path, LeftFiles::Keep)
                .await
            {
                Ok(Cleanup::Removed | Cleanup::InUse | Cleanup::NotOwned) => {}
                Ok(Cleanup::Kept { files }) => {
                    println!(
                        "Kept the worktree {path}, which holds {} files",
                        files.len()
                    );
                }
                Ok(Cleanup::UnsavedCommit { commit }) => {
                    println!("Kept the worktree {path}, whose HEAD {commit} is on no branch");
                }
                Ok(Cleanup::Blocked { error }) => eprintln!("{error}"),
                Err(error) => eprintln!("cannot finish removing the worktree {path}: {error}"),
            }
        }
    }

    async fn remove_worktree(
        &self,
        store: &StoreThread,
        id: &WorktreeId,
        path: &str,
        files: LeftFiles,
    ) -> Result<Cleanup, StoreError> {
        let begin = id.clone();
        let repository = match store.call(move |store| store.begin_removal(&begin)).await? {
            Removal::InUse => return Ok(Cleanup::InUse),
            Removal::Done => return Ok(Cleanup::Removed),
            Removal::Started { repository } => repository,
        };
        let cleanup = self
            .remove(Path::new(&repository), path, files)
            .await
            .unwrap_or_else(|error| Cleanup::Blocked {
                error: error.to_string(),
            });
        let end = match cleanup {
            Cleanup::Removed => RemovalEnd::Removed,
            Cleanup::Kept { .. } | Cleanup::UnsavedCommit { .. } => RemovalEnd::Kept,
            _ => return Ok(cleanup),
        };
        let id = id.clone();
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
        if let Some(commit) = self.unsaved_head(folder).await? {
            return Ok(Cleanup::UnsavedCommit { commit });
        }
        let status = [
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=normal",
        ];
        let status = self.git.run(folder, &status, git::QUICK).await?;
        let mut left: Vec<String> = status.lines().map(str::to_owned).collect();
        left.extend(self.overlooked_edits(folder).await?);
        let mut remove = vec!["worktree", "remove"];
        if !left.is_empty() {
            match files {
                LeftFiles::Keep => return Ok(Cleanup::Kept { files: left }),
                LeftFiles::Discard => remove.push("--force"),
            }
        }
        remove.extend(["--", path]);
        self.git.run(repository, &remove, CHECKOUT).await?;
        Ok(Cleanup::Removed)
    }

    /// The worktree's `HEAD` when it is detached at a commit no ref holds, which only the
    /// worktree keeps reachable.
    async fn unsaved_head(&self, folder: &Path) -> Result<Option<String>, PlaceError> {
        if self.branch_of(folder).await?.is_some() {
            return Ok(None);
        }
        let holders = [
            "for-each-ref",
            "--count=1",
            "--contains=HEAD",
            "--format=%(refname)",
        ];
        if !self.git.run(folder, &holders, git::QUICK).await?.is_empty() {
            return Ok(None);
        }
        let commit = self
            .git
            .run(folder, &["rev-parse", "HEAD"], git::QUICK)
            .await?;
        Ok(Some(commit))
    }

    /// Edits `git status` does not show, because the file is marked assume-unchanged or
    /// skip-worktree, as status lines such as ` M config.txt`. A file is edited when its content
    /// on disk hashes to another object than the index holds.
    async fn overlooked_edits(&self, folder: &Path) -> Result<Vec<String>, PlaceError> {
        let listed = self
            .git
            .run(folder, &["ls-files", "-v", "-s", "-z"], git::QUICK)
            .await?;
        let mut on_disk = Vec::new();
        for (object, path) in overlooked(&listed) {
            // A skip-worktree file a sparse checkout left out has nothing on disk to lose.
            if tokio::fs::symlink_metadata(folder.join(path)).await.is_ok() {
                on_disk.push((object, path));
            }
        }
        if on_disk.is_empty() {
            return Ok(Vec::new());
        }
        let mut hash = vec!["hash-object", "--"];
        hash.extend(on_disk.iter().map(|(_, path)| *path));
        let hashed = self.git.run(folder, &hash, git::QUICK).await?;
        Ok(on_disk
            .iter()
            .zip(hashed.lines())
            .filter(|((object, _), now)| object != now)
            .map(|((_, path), _)| format!(" M {path}"))
            .collect())
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

/// The object and path of each index entry in `git ls-files -v -s -z` output that git is told to
/// overlook: a lowercase tag marks assume-unchanged, and `S` marks skip-worktree.
fn overlooked(listed: &str) -> impl Iterator<Item = (&str, &str)> {
    listed.split('\0').filter_map(|entry| {
        let (fields, path) = entry.split_once('\t')?;
        let mut fields = fields.split(' ');
        let tag = fields.next()?;
        let object = fields.nth(1)?;
        let is_overlooked = tag == "S" || tag.chars().all(|tag| tag.is_ascii_lowercase());
        is_overlooked.then_some((object, path))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_files_git_is_told_to_overlook_are_listed() {
        let listed = "H 100644 1111 0\tplain.txt\0h 100644 2222 0\tassumed.txt\0\
                      S 100644 3333 0\tsparse.txt\0s 100644 4444 0\tboth.txt\0";

        assert_eq!(
            overlooked(listed).collect::<Vec<_>>(),
            [
                ("2222", "assumed.txt"),
                ("3333", "sparse.txt"),
                ("4444", "both.txt")
            ]
        );
    }
}
