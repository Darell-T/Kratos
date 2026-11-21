//! Removing a worktree Kratos made, once every thread that worked in it is archived (D16). Its
//! branch stays, and so do files the removal would delete unless the user chose to discard them.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use kratos_protocol::{Cleanup, LeftFiles, Ownership, Place, WorktreeId};
use kratos_store::{KratosWorktree, Removal, RemovalEnd, StoreError, UnfinishedWorktree};

use super::git::{self, CHECKOUT};
use super::{
    PlaceError, WorktreeLock, Worktrees, ensure_our_lock, git_dir_of, is_same_checkout,
    is_same_checkout_at, is_same_checkout_dir, read_lock, resolved,
};
use crate::store_thread::StoreThread;

/// Outcome of checking whether a worktree may be deleted under [`LeftFiles::Keep`].
enum KeepCheck {
    Clean,
    Dirty(Vec<String>),
    HasSubmodule,
}

/// Decision reached after re-verifying a quarantined worktree.
enum QuarantineVerdict {
    Remove(PathBuf),
    Keep(Cleanup),
}

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
        let quarantine = quarantine_path(folder, &recorded.id)?;
        let found = self.locate(folder).await?;
        if found.is_none_or(|worktree| worktree.top != folder) {
            if is_same_checkout_at(&quarantine, recorded).await {
                return self
                    .finish_quarantined(repository, recorded, &quarantine, files)
                    .await;
            }
            return remove_leftover(repository, recorded).await;
        }
        let Some(git_dir) = git_dir_of(folder).await else {
            return Ok(Cleanup::NotOwned);
        };
        if !is_same_checkout_dir(&git_dir, recorded).await {
            return Ok(Cleanup::NotOwned);
        }
        ensure_our_lock(&git_dir, path).await?;
        if let Some(commit) = self.unsaved_head(folder).await? {
            return Ok(Cleanup::UnsavedCommit { commit });
        }
        if files == LeftFiles::Keep {
            match self.check_clean_for_keep(folder).await? {
                KeepCheck::Clean => {}
                KeepCheck::Dirty(left) => return Ok(Cleanup::Kept { files: left }),
                KeepCheck::HasSubmodule => {
                    return Err(PlaceError::SubmodulesPresent {
                        path: path.to_owned(),
                    });
                }
            }
        }
        if !is_same_checkout(recorded).await {
            return Ok(Cleanup::NotOwned);
        }
        // Move the worktree out of its live path atomically before re-checking and deleting, so
        // late writes (including ignored files) move with it and git never acts on `folder`.
        match tokio::fs::rename(folder, &quarantine).await {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return remove_leftover(repository, recorded).await;
            }
            Err(source) => {
                return Err(PlaceError::CannotRemove {
                    path: path.to_owned(),
                    source,
                });
            }
        }
        self.finish_quarantined(repository, recorded, &quarantine, files)
            .await
    }

    /// Re-verifies `quarantine` and removes it through git, or restores it to `recorded.path` if
    /// any check fails or decides to keep it.
    async fn finish_quarantined(
        &self,
        repository: &Path,
        recorded: &KratosWorktree,
        quarantine: &Path,
        files: LeftFiles,
    ) -> Result<Cleanup, PlaceError> {
        let folder = Path::new(recorded.path.as_str());
        match self.verify_quarantined(recorded, quarantine, files).await {
            Ok(QuarantineVerdict::Remove(git_dir)) => {
                self.remove_quarantined(repository, folder, quarantine, &git_dir)
                    .await?;
                Ok(Cleanup::Removed)
            }
            Ok(QuarantineVerdict::Keep(cleanup)) => {
                restore_quarantine(quarantine, folder).await?;
                Ok(cleanup)
            }
            Err(error) => {
                let _ = restore_quarantine(quarantine, folder).await;
                Err(error)
            }
        }
    }

    async fn verify_quarantined(
        &self,
        recorded: &KratosWorktree,
        quarantine: &Path,
        files: LeftFiles,
    ) -> Result<QuarantineVerdict, PlaceError> {
        let path = recorded.path.as_str();
        let Some(git_dir) = git_dir_of(quarantine).await else {
            return Ok(QuarantineVerdict::Keep(Cleanup::NotOwned));
        };
        if !is_same_checkout_dir(&git_dir, recorded).await {
            return Ok(QuarantineVerdict::Keep(Cleanup::NotOwned));
        }
        ensure_our_lock(&git_dir, path).await?;
        if let Some(commit) = self.unsaved_head(quarantine).await? {
            return Ok(QuarantineVerdict::Keep(Cleanup::UnsavedCommit { commit }));
        }
        if files == LeftFiles::Keep {
            match self.check_clean_for_keep(quarantine).await? {
                KeepCheck::Clean => {}
                KeepCheck::Dirty(left) => {
                    return Ok(QuarantineVerdict::Keep(Cleanup::Kept { files: left }));
                }
                KeepCheck::HasSubmodule => {
                    return Err(PlaceError::SubmodulesPresent {
                        path: path.to_owned(),
                    });
                }
            }
        }
        Ok(QuarantineVerdict::Remove(git_dir))
    }

    /// Points `git_dir/gitdir` at `quarantine` and removes `quarantine` through git while Kratos's
    /// own lock stays intact, so `folder` is never named to `git worktree remove`.
    async fn remove_quarantined(
        &self,
        repository: &Path,
        folder: &Path,
        quarantine: &Path,
        git_dir: &Path,
    ) -> Result<(), PlaceError> {
        let gitdir_file = git_dir.join("gitdir");
        let previous_gitdir = tokio::fs::read(&gitdir_file).await.ok();
        let quarantine_text = super::text(quarantine)?;
        let quarantine_gitdir = gitdir_entry(&quarantine.join(".git"));
        if let Err(source) = tokio::fs::write(&gitdir_file, quarantine_gitdir).await {
            let _ = restore_quarantine(quarantine, folder).await;
            return Err(PlaceError::Io {
                path: gitdir_file.display().to_string(),
                source,
            });
        }
        let remove = [
            "worktree",
            "remove",
            "--force",
            "--force",
            "--",
            &quarantine_text,
        ];
        let removed = self.git.run(repository, &remove, CHECKOUT).await;
        if let Err(error) = removed {
            if tokio::fs::symlink_metadata(quarantine).await.is_ok() {
                if let Some(previous) = previous_gitdir {
                    let _ = tokio::fs::write(&gitdir_file, previous).await;
                }
                let _ = restore_quarantine(quarantine, folder).await;
            }
            return Err(error.into());
        }
        Ok(())
    }

    /// Checks whether `folder` has any file or initialized submodule that [`LeftFiles::Keep`] must
    /// preserve.
    async fn check_clean_for_keep(&self, folder: &Path) -> Result<KeepCheck, PlaceError> {
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
        if !left.is_empty() {
            return Ok(KeepCheck::Dirty(left));
        }
        if has_initialized_submodule(folder, &listed).await {
            return Ok(KeepCheck::HasSubmodule);
        }
        Ok(KeepCheck::Clean)
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
/// so Kratos removes the folder only while it is empty. A record the user locked stays untouched,
/// even when its folder is offline and the record carries Kratos's ownership id.
async fn remove_leftover(
    repository: &Path,
    recorded: &KratosWorktree,
) -> Result<Cleanup, PlaceError> {
    let records = matching_records(repository, recorded).await?;
    if let Some(reason) = records.iter().find_map(|(_, lock)| match lock {
        WorktreeLock::Other(reason) => Some(reason.clone()),
        WorktreeLock::Unlocked | WorktreeLock::Ours => None,
    }) {
        return Err(PlaceError::WorktreeLocked {
            path: recorded.path.clone(),
            reason,
        });
    }
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
    for (record, _) in records {
        if is_same_checkout_dir(&record, recorded).await {
            tokio::fs::remove_dir_all(&record)
                .await
                .map_err(|source| PlaceError::Io {
                    path: record.display().to_string(),
                    source,
                })?;
        }
    }
    Ok(Cleanup::Removed)
}

/// Every git worktree admin record under `repository/worktrees` whose `gitdir` points at
/// `recorded.path/.git`, paired with its lock state.
async fn matching_records(
    repository: &Path,
    recorded: &KratosWorktree,
) -> Result<Vec<(PathBuf, WorktreeLock)>, PlaceError> {
    let io_error = |path: &Path| {
        let path = path.display().to_string();
        move |source| PlaceError::Io { path, source }
    };
    let records = repository.join("worktrees");
    let mut entries = match tokio::fs::read_dir(&records).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries.map_err(io_error(&records))?,
    };
    let checkout = Path::new(&recorded.path).join(".git");
    let mut matched = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(io_error(&records))? {
        let record = entry.path();
        let Ok(gitdir) = tokio::fs::read_to_string(record.join("gitdir")).await else {
            continue;
        };
        if record.join(gitdir.trim()) == checkout {
            let lock = read_lock(&record).await?;
            matched.push((record, lock));
        }
    }
    Ok(matched)
}

/// A sibling path in the same parent folder where `folder` is moved before final verification and
/// deletion, keeping the rename atomic on the same filesystem.
fn quarantine_path(folder: &Path, id: &WorktreeId) -> Result<PathBuf, PlaceError> {
    let parent = folder.parent().ok_or_else(|| PlaceError::CannotRemove {
        path: folder.display().to_string(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "worktree has no parent folder"),
    })?;
    Ok(parent.join(format!(".kratos-removing-{}", id.as_str())))
}

/// Moves a quarantined worktree back to its original path when post-rename verification keeps it.
async fn restore_quarantine(quarantine: &Path, folder: &Path) -> Result<(), PlaceError> {
    tokio::fs::rename(quarantine, folder)
        .await
        .map_err(|source| PlaceError::CannotRemove {
            path: quarantine.display().to_string(),
            source,
        })
}

/// Formats `git_file` (`<quarantine>/.git`) as git stores `gitdir` in a linked worktree's admin
/// folder.
#[cfg(windows)]
fn gitdir_entry(git_file: &Path) -> Vec<u8> {
    format!("{}\n", git_file.to_string_lossy().replace('\\', "/")).into_bytes()
}

#[cfg(not(windows))]
fn gitdir_entry(git_file: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = git_file.as_os_str().as_bytes().to_vec();
    bytes.push(b'\n');
    bytes
}

/// Whether `listed` (`git ls-files -v -s -z`) names any gitlink (`160000`) whose folder in
/// `folder` is non-empty, matching `git worktree remove`'s submodule refusal.
async fn has_initialized_submodule(folder: &Path, listed: &[u8]) -> bool {
    for path in submodules(listed) {
        let Some(dir) = file_in(folder, path) else {
            return true;
        };
        if let Ok(mut entries) = tokio::fs::read_dir(&dir).await
            && entries.next_entry().await.ok().flatten().is_some()
        {
            return true;
        }
    }
    false
}

/// The path of each gitlink (submodule, mode `160000`) in `git ls-files -v -s -z` output.
fn submodules(listed: &[u8]) -> impl Iterator<Item = &[u8]> {
    listed.split(|&byte| byte == 0).filter_map(|entry| {
        let tab = entry.iter().position(|&byte| byte == b'\t')?;
        let mut fields = entry[..tab].split(|&byte| byte == b' ');
        let _tag = fields.next()?;
        let mode = fields.next()?;
        (mode == b"160000").then_some(&entry[tab + 1..])
    })
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
