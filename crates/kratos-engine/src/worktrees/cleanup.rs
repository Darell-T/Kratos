//! Removing a worktree Kratos made, once every thread that worked in it is archived (D16). Its
//! branch stays, and so do files the removal would delete unless the user chose to discard them.

use std::io;
use std::path::{Path, PathBuf};

use kratos_protocol::{Cleanup, LeftFiles, Ownership, Place, WorktreeId};
use kratos_store::{KratosWorktree, Removal, RemovalEnd, StoreError, UnfinishedWorktree};

use super::git::{self, CHECKOUT};
use super::{PlaceError, Worktrees, is_same_checkout, resolved};
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
        if holds_a_project(store, Path::new(path)).await? {
            return Ok(Cleanup::InUse);
        }
        let begin = id.clone();
        let (repository, git_dir) =
            match store.call(move |store| store.begin_removal(&begin)).await? {
                Removal::InUse => return Ok(Cleanup::InUse),
                Removal::Done => return Ok(Cleanup::Removed),
                Removal::Started {
                    repository,
                    git_dir,
                } => (repository, git_dir),
            };
        let recorded = KratosWorktree {
            id: id.clone(),
            path: path.to_owned(),
            git_dir,
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
            return remove_leftover(repository, folder).await;
        }
        if !is_same_checkout(recorded).await {
            return Ok(Cleanup::NotOwned);
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
            .run_bytes(folder, &["ls-files", "-v", "-s", "-z"], git::QUICK)
            .await?;
        let mut edited = Vec::new();
        let mut on_disk = Vec::new();
        for (object, path) in overlooked(&listed) {
            let Some(file) = file_in(folder, path) else {
                // Kratos cannot read the file, so it cannot tell that the file is unchanged.
                edited.push(edit_line(path));
                continue;
            };
            // A skip-worktree file a sparse checkout left out has nothing on disk to lose.
            if tokio::fs::symlink_metadata(file).await.is_ok() {
                on_disk.push((object, path));
            }
        }
        if on_disk.is_empty() {
            return Ok(edited);
        }
        let mut paths = Vec::new();
        for (_, path) in &on_disk {
            paths.extend(quoted(path));
            paths.push(b'\n');
        }
        let hash = ["hash-object", "--stdin-paths"];
        let hashed = self
            .git
            .run_with_input(folder, &hash, paths, git::QUICK)
            .await?;
        let changed = on_disk
            .iter()
            .zip(hashed.lines())
            .filter(|((object, _), now)| *object != now.as_bytes())
            .map(|((_, path), _)| edit_line(path));
        edited.extend(changed);
        Ok(edited)
    }
}

/// Removes what is left of a worktree that is no longer one, such as the folder a removal git could
/// not finish leaves after deleting its `.git` file. Git would read the project's repository there,
/// so Kratos removes the folder only while it is empty. `repository` is the shared git folder.
async fn remove_leftover(repository: &Path, folder: &Path) -> Result<Cleanup, PlaceError> {
    match tokio::fs::remove_dir(folder).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(PlaceError::CannotRemove {
                path: folder.display().to_string(),
                source: error,
            });
        }
        _ => {}
    }
    forget(repository, folder).await?;
    Ok(Cleanup::Removed)
}

/// Deletes git's record of the worktree at `folder`, whose folder is gone, as `git worktree prune`
/// would for it alone. Prune also drops the record of every other worktree out of reach for now,
/// such as one on a drive that is not plugged in. A locked record stays, as prune leaves it.
async fn forget(repository: &Path, folder: &Path) -> Result<(), PlaceError> {
    let io_error = |path: &Path| {
        let path = path.display().to_string();
        move |source| PlaceError::Io { path, source }
    };
    let records = repository.join("worktrees");
    let mut entries = match tokio::fs::read_dir(&records).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        entries => entries.map_err(io_error(&records))?,
    };
    let checkout = folder.join(".git");
    while let Some(entry) = entries.next_entry().await.map_err(io_error(&records))? {
        let record = entry.path();
        let Ok(gitdir) = tokio::fs::read_to_string(record.join("gitdir")).await else {
            continue;
        };
        let is_locked = tokio::fs::symlink_metadata(record.join("locked"))
            .await
            .is_ok();
        if record.join(gitdir.trim()) == checkout && !is_locked {
            tokio::fs::remove_dir_all(&record)
                .await
                .map_err(io_error(&record))?;
        }
    }
    Ok(())
}

/// Whether a thread that is not archived works in a project folder at `worktree` or inside it,
/// as when the user opens a worktree Kratos made as a project of its own. Project folders are
/// stored as the user gave them, so each is resolved before the comparison.
async fn holds_a_project(store: &StoreThread, worktree: &Path) -> Result<bool, StoreError> {
    let folders = store.call(|store| store.project_folders_in_use()).await?;
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

/// The object and path of each index entry in `git ls-files -v -s -z` output that git is told to
/// overlook: a lowercase tag marks assume-unchanged, and `S` marks skip-worktree. Paths stay as
/// git wrote them, since a file name need not be UTF-8.
fn overlooked(listed: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    listed.split(|&byte| byte == 0).filter_map(|entry| {
        let tab = entry.iter().position(|&byte| byte == b'\t')?;
        let mut fields = entry[..tab].split(|&byte| byte == b' ');
        let tag = fields.next()?;
        let object = fields.nth(1)?;
        let is_overlooked = tag == b"S" || tag.iter().all(u8::is_ascii_lowercase);
        is_overlooked.then_some((object, &entry[tab + 1..]))
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

/// `path` as a status line for an edited file, such as ` M config.txt`.
fn edit_line(path: &[u8]) -> String {
    format!(" M {}", String::from_utf8_lossy(path))
}

/// `path` in the C-style quotes `git hash-object --stdin-paths` reads, so a newline, a carriage
/// return git would trim, or a leading quote stays part of the name. Other bytes pass as they are.
fn quoted(path: &[u8]) -> Vec<u8> {
    let mut quoted = vec![b'"'];
    for &byte in path {
        match byte {
            b'"' | b'\\' => quoted.extend([b'\\', byte]),
            byte if byte.is_ascii_control() => {
                quoted.extend(format!("\\{byte:03o}").into_bytes());
            }
            byte => quoted.push(byte),
        }
    }
    quoted.push(b'"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_path_escapes_what_git_would_read_as_quoting_or_a_line_end() {
        assert_eq!(quoted(b"plain.txt"), b"\"plain.txt\"");
        assert_eq!(
            quoted(b"say \"hi\"\\back\nline\rend\xff"),
            b"\"say \\\"hi\\\"\\\\back\\012line\\015end\xff\""
        );
    }

    #[test]
    fn only_files_git_is_told_to_overlook_are_listed() {
        let listed = b"H 100644 1111 0\tplain.txt\0h 100644 2222 0\tassumed.txt\0\
                      S 100644 3333 0\tsparse.txt\0s 100644 4444 0\tboth.txt\0";

        assert_eq!(
            overlooked(listed).collect::<Vec<_>>(),
            [
                (&b"2222"[..], &b"assumed.txt"[..]),
                (b"3333", b"sparse.txt"),
                (b"4444", b"both.txt")
            ]
        );
    }
}
