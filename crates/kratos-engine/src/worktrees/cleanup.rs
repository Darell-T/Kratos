//! Removing a worktree Kratos made, once every thread that worked in it is archived (D16). Its
//! branch stays, and so do files the removal would delete unless the user chose to discard them.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use kratos_protocol::{Cleanup, LeftFiles, Ownership, Place, WorktreeId};
use kratos_store::{KratosWorktree, Removal, RemovalEnd, StoreError, UnfinishedWorktree};

use super::git::{self, CHECKOUT};
use super::{
    PlaceError, WORKTREE_ID_FILE, Worktrees, is_same_checkout, lock, resolved, unlock_if_ours,
};
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
        // Looked for only once the worktree is marked as being removed, after which no thread can
        // start inside it, so none is missed.
        if holds_a_thread(store, Path::new(path)).await? {
            let id = id.clone();
            store
                .call(move |store| store.end_removal(&id, RemovalEnd::Kept))
                .await?;
            return Ok(Cleanup::InUse);
        }
        let recorded = KratosWorktree {
            id: id.clone(),
            path: path.to_owned(),
        };
        let cleanup = self
            .remove(Path::new(&repository), &recorded, files)
            .await
            .unwrap_or_else(|error| Cleanup::Blocked {
                error: error.to_string(),
            });
        let end = match cleanup {
            // Another checkout took the worktree's place, so Kratos's worktree is gone from it.
            Cleanup::Removed | Cleanup::NotOwned => RemovalEnd::Removed,
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
        recorded: &KratosWorktree,
        files: LeftFiles,
    ) -> Result<Cleanup, PlaceError> {
        let lock = self.lock_of(repository);
        let _one_at_a_time = lock.lock().await;
        let path = recorded.path.as_str();
        let folder = Path::new(path);
        let found = self.locate(folder).await?;
        if found.is_none_or(|worktree| worktree.top != folder) {
            return remove_leftover(repository, recorded).await;
        }
        if !is_same_checkout(recorded).await {
            return Ok(Cleanup::NotOwned);
        }
        if let Some(commit) = self.unsaved_head(folder).await? {
            return Ok(Cleanup::UnsavedCommit { commit });
        }
        if files == LeftFiles::Keep {
            let left = self.left_files(folder).await?;
            if !left.is_empty() {
                return Ok(Cleanup::Kept { files: left });
            }
        }
        // Checked again just before git deletes, as another checkout may have taken the path
        // while git read it.
        if !is_same_checkout(recorded).await {
            return Ok(Cleanup::NotOwned);
        }
        self.remove_checked(repository, path, files).await?;
        Ok(Cleanup::Removed)
    }

    /// Has git remove the worktree at `path`. Git refuses while it holds changed files or
    /// submodules, unless `files` discards them, and while the user locked it. Git checks the files
    /// as it deletes them, so it also sees one saved after Kratos looked.
    async fn remove_checked(
        &self,
        repository: &Path,
        path: &str,
        files: LeftFiles,
    ) -> Result<(), PlaceError> {
        let unlocked = unlock_if_ours(Path::new(path)).await?;
        let mut remove = vec!["worktree", "remove"];
        if files == LeftFiles::Discard {
            remove.push("--force");
        }
        remove.extend(["--", path]);
        let removed = self.git.run(repository, &remove, CHECKOUT).await;
        if removed.is_err()
            && let Some(git_dir) = unlocked
        {
            // Git may have deleted the worktree's git folder before it failed, and then there is
            // no worktree left to lock.
            let _ = lock(&git_dir).await;
        }
        removed?;
        Ok(())
    }

    /// The files removing the worktree at `folder` would delete, as status lines.
    async fn left_files(&self, folder: &Path) -> Result<Vec<String>, PlaceError> {
        let status = [
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ];
        let status = self.git.run(folder, &status, git::QUICK).await?;
        let listed = self
            .git
            .run_bytes(folder, &["ls-files", "-v", "-s", "-z"], git::QUICK)
            .await?;
        let mut left: Vec<String> = status.lines().map(str::to_owned).collect();
        left.extend(self.overlooked_edits(folder, &listed).await?);
        Ok(left)
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
    /// skip-worktree, as status lines such as ` M config.txt`. Git compares each such file on disk
    /// in a scratch copy of the index that no longer marks it, so a new executable bit, or a file
    /// replaced by a link, shows as git shows it for any other file.
    /// `listed` is the worktree's `git ls-files -v -s -z`.
    async fn overlooked_edits(
        &self,
        folder: &Path,
        listed: &[u8],
    ) -> Result<Vec<String>, PlaceError> {
        let mut edited = Vec::new();
        let mut on_disk = Vec::new();
        for path in overlooked(listed) {
            let Some(file) = file_in(folder, path) else {
                // Kratos cannot read the file, so it cannot tell that the file is unchanged.
                edited.push(status_line(b"M", path));
                continue;
            };
            // A skip-worktree file a sparse checkout left out has nothing on disk to lose.
            if tokio::fs::symlink_metadata(file).await.is_ok() {
                on_disk.push(path);
            }
        }
        if on_disk.is_empty() {
            return Ok(edited);
        }
        let index = self.scratch_index(folder).await?;
        let changed = self.changed_once_unmarked(folder, &index, &on_disk).await;
        let _ = tokio::fs::remove_file(&index).await;
        edited.extend(changed?);
        Ok(edited)
    }

    /// The status line of each of `paths` that differs from `index` once `index` no longer marks
    /// it to be overlooked.
    async fn changed_once_unmarked(
        &self,
        folder: &Path,
        index: &Path,
        paths: &[&[u8]],
    ) -> Result<Vec<String>, PlaceError> {
        let listed: Vec<u8> = paths
            .iter()
            .flat_map(|path| path.iter().copied().chain([0]))
            .collect();
        // Git applies one of these marks per run.
        for unmark in ["--no-assume-unchanged", "--no-skip-worktree"] {
            let args = ["update-index", unmark, "-z", "--stdin"];
            self.git
                .run_in_index_with_input(folder, &args, listed.clone(), git::QUICK, index)
                .await?;
        }
        let args = ["diff", "--name-status", "--no-renames", "-z"];
        let changed = self
            .git
            .run_bytes_in_index(folder, &args, git::QUICK, index)
            .await?;
        let paths: HashSet<&[u8]> = paths.iter().copied().collect();
        Ok(changes(&changed)
            .filter(|(_, path)| paths.contains(path))
            .map(|(status, path)| status_line(status, path))
            .collect())
    }
}

/// Removes what is left of a worktree that is no longer one, such as the folder a removal git could
/// not finish leaves after deleting its `.git` file. Git would read the project's repository there,
/// so Kratos removes the folder only while it is empty. `repository` is the shared git folder.
async fn remove_leftover(
    repository: &Path,
    recorded: &KratosWorktree,
) -> Result<Cleanup, PlaceError> {
    let folder = Path::new(&recorded.path);
    match tokio::fs::remove_dir(folder).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(PlaceError::CannotRemove {
                path: folder.display().to_string(),
                source: error,
            });
        }
        _ => {}
    }
    forget(repository, recorded).await?;
    Ok(Cleanup::Removed)
}

/// Deletes git's record of `recorded`, whose folder is gone, as `git worktree prune` would for it
/// alone. Prune also drops the record of every other worktree out of reach for now, such as one
/// on a drive that is not plugged in. A record someone else locked stays, as prune leaves it, but
/// Kratos's own lock does not keep it.
async fn forget(repository: &Path, recorded: &KratosWorktree) -> Result<(), PlaceError> {
    let io_error = |path: &Path| {
        let path = path.display().to_string();
        move |source| PlaceError::Io { path, source }
    };
    let records = repository.join("worktrees");
    let mut entries = match tokio::fs::read_dir(&records).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        entries => entries.map_err(io_error(&records))?,
    };
    let checkout = Path::new(&recorded.path).join(".git");
    while let Some(entry) = entries.next_entry().await.map_err(io_error(&records))? {
        let record = entry.path();
        let Ok(gitdir) = tokio::fs::read_to_string(record.join("gitdir")).await else {
            continue;
        };
        let is_locked = tokio::fs::symlink_metadata(record.join("locked"))
            .await
            .is_ok();
        let is_ours = tokio::fs::read_to_string(record.join(WORKTREE_ID_FILE))
            .await
            .is_ok_and(|id| id == recorded.id.as_str());
        if record.join(gitdir.trim()) == checkout && (is_ours || !is_locked) {
            tokio::fs::remove_dir_all(&record)
                .await
                .map_err(io_error(&record))?;
        }
    }
    Ok(())
}

/// Whether a thread that is not archived works at `worktree` or in a folder inside it, as when the
/// user opens a worktree Kratos made as a project of its own, then makes a worktree from it.
/// Project folders are stored as the user gave them, so each is resolved before the comparison.
async fn holds_a_thread(store: &StoreThread, worktree: &Path) -> Result<bool, StoreError> {
    let folders = store.call(|store| store.folders_in_use()).await?;
    for folder in folders {
        if resolved(Path::new(&folder))
            .await
            .is_ok_and(|folder| folder.starts_with(worktree))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The path of each index entry in `git ls-files -v -s -z` output that git is told to overlook: a
/// lowercase tag marks assume-unchanged, and `S` marks skip-worktree. Paths stay as git wrote
/// them, since a file name need not be UTF-8.
fn overlooked(listed: &[u8]) -> impl Iterator<Item = &[u8]> {
    listed.split(|&byte| byte == 0).filter_map(|entry| {
        let tab = entry.iter().position(|&byte| byte == b'\t')?;
        let tag = entry[..tab].split(|&byte| byte == b' ').next()?;
        let is_overlooked = tag == b"S" || tag.iter().all(u8::is_ascii_lowercase);
        is_overlooked.then_some(&entry[tab + 1..])
    })
}

/// The status and path of each file in `git diff --name-status --no-renames -z` output.
fn changes(listed: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut fields = listed.split(|&byte| byte == 0);
    std::iter::from_fn(move || {
        let status = fields.next().filter(|status| !status.is_empty())?;
        Some((status, fields.next()?))
    })
}

/// The file git's `path` names in `folder`.
#[cfg(unix)]
fn file_in(folder: &Path, path: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Some(folder.join(std::ffi::OsStr::from_bytes(path)))
}

/// The file git's `path` names in `folder`, or `None` when `path` is not UTF-8, which no file name
/// here can be.
#[cfg(not(unix))]
fn file_in(folder: &Path, path: &[u8]) -> Option<PathBuf> {
    std::str::from_utf8(path).ok().map(|path| folder.join(path))
}

/// `path` as a status line for a file changed on disk, such as ` M config.txt`.
fn status_line(status: &[u8], path: &[u8]) -> String {
    format!(
        " {} {}",
        String::from_utf8_lossy(status),
        String::from_utf8_lossy(path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_files_git_is_told_to_overlook_are_listed() {
        let listed = b"H 100644 1111 0\tplain.txt\0h 100644 2222 0\tassumed.txt\0\
                      S 100644 3333 0\tsparse.txt\0s 100644 4444 0\tboth.txt\0";

        assert_eq!(
            overlooked(listed).collect::<Vec<_>>(),
            [&b"assumed.txt"[..], b"sparse.txt", b"both.txt"]
        );
    }

    #[test]
    fn each_change_pairs_its_status_with_its_path() {
        let listed = b"M\0tool.sh\0T\0link\ttab.txt\0";

        assert_eq!(
            changes(listed).collect::<Vec<_>>(),
            [(&b"M"[..], &b"tool.sh"[..]), (b"T", b"link\ttab.txt")]
        );
    }
}
