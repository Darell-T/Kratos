//! Removing a worktree Kratos made once every thread in it is archived (D16). Keep moves a clean
//! checkout into a same-volume trash folder so late saves through open handles survive until
//! Kratos empties expired trash; Discard deletes immediately. Branches always stay.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use kratos_protocol::{Cleanup, LeftFiles, Ownership, Place, Timestamp, WorktreeId};
use kratos_store::{KratosWorktree, Removal, RemovalEnd, StoreError};
use serde::{Deserialize, Serialize};

use super::git;
use super::{
    PlaceError, TRASH_FOLDER, WORKTREES_FOLDER, Worktrees, ignore_everything_in,
    is_same_checkout_at, is_same_checkout_dir, resolved,
};
use crate::store_thread::StoreThread;

const TRASH_RETENTION: Duration = Duration::from_hours(7 * 24);
const TRASH_SLOT_LEN: usize = WORKTREES_FOLDER.len() - TRASH_FOLDER.len() + 1;
/// Replaces `<slot>/.git` on Keep so git run in a trashed worktree fails with "not a git
/// repository" instead of walking up to the project's `.git`.
const TRASH_GIT_SENTINEL: &str = "gitdir: .kratos-archived-worktree\n";
const GITLINK_MODE: &[u8] = b"160000";

enum KeepCheck {
    Clean,
    Dirty(Vec<String>),
    HasSubmodule,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrashedWorktree {
    worktree_id: String,
    original_path: String,
    branch: Option<String>,
    trashed_at: Timestamp,
}

struct TrashSlot {
    dir: PathBuf,
    meta: PathBuf,
}

impl Worktrees {
    /// Removes the worktree at `place` when Kratos made it and no unarchived thread works in it.
    /// The project folder and borrowed worktrees are never removed.
    pub(crate) async fn clean_up(
        &self,
        store: &StoreThread,
        place: &Place,
        files: LeftFiles,
    ) -> Result<Cleanup, StoreError> {
        match place {
            Place::Worktree(worktree) if worktree.ownership == Ownership::CreatedByKratos => {
                let recorded = KratosWorktree {
                    id: worktree.id.clone(),
                    path: worktree.path.clone(),
                };
                self.remove_worktree(store, &recorded, files).await
            }
            Place::Worktree(_) | Place::ProjectFolder => Ok(Cleanup::NotOwned),
        }
    }

    /// Finishes worktrees left unfinished by an earlier engine and empties expired trash entries.
    pub async fn sweep(&self, store: &StoreThread, unfinished: Vec<KratosWorktree>) {
        for worktree in unfinished {
            let path = &worktree.path;
            match self
                .remove_worktree(store, &worktree, LeftFiles::Keep)
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
        self.empty_expired_trash(store).await;
    }

    async fn remove_worktree(
        &self,
        store: &StoreThread,
        recorded: &KratosWorktree,
        files: LeftFiles,
    ) -> Result<Cleanup, StoreError> {
        let begin = recorded.id.clone();
        let repository = match store.call(move |store| store.begin_removal(&begin)).await? {
            Removal::InUse => return Ok(Cleanup::InUse),
            Removal::Done => return Ok(Cleanup::Removed),
            Removal::Started { repository } => repository,
        };
        // Checked after `removing` is recorded so no new thread can start inside `path` unseen.
        let cleanup = if holds_a_thread(store, Path::new(&recorded.path)).await? {
            Cleanup::InUse
        } else {
            self.remove(Path::new(&repository), recorded, files)
                .await
                .unwrap_or_else(|error| Cleanup::Blocked {
                    error: error.to_string(),
                })
        };
        let end = match cleanup {
            Cleanup::Removed | Cleanup::NotOwned => RemovalEnd::Removed,
            Cleanup::InUse | Cleanup::Kept { .. } | Cleanup::UnsavedCommit { .. } => {
                RemovalEnd::Kept
            }
            Cleanup::Blocked { .. } => return Ok(cleanup),
        };
        let id = recorded.id.clone();
        store.call(move |store| store.end_removal(&id, end)).await?;
        Ok(cleanup)
    }

    async fn remove(
        &self,
        repository: &Path,
        recorded: &KratosWorktree,
        files: LeftFiles,
    ) -> Result<Cleanup, PlaceError> {
        let lock = self.lock_of(repository);
        let _one_at_a_time = lock.lock().await;
        let folder = Path::new(recorded.path.as_str());
        match find_our_admin(repository, recorded).await? {
            Some(admin) => {
                self.remove_registered(folder, &admin, recorded, files)
                    .await
            }
            None => self.remove_unregistered(folder).await,
        }
    }

    async fn remove_registered(
        &self,
        folder: &Path,
        admin: &Path,
        recorded: &KratosWorktree,
        files: LeftFiles,
    ) -> Result<Cleanup, PlaceError> {
        if is_same_checkout_at(folder, recorded).await {
            return self
                .remove_registered_live(folder, admin, recorded, files)
                .await;
        }
        let outcome = self.remove_unregistered(folder).await?;
        remove_tree(admin).await?;
        Ok(outcome)
    }

    async fn remove_registered_live(
        &self,
        folder: &Path,
        admin: &Path,
        recorded: &KratosWorktree,
        files: LeftFiles,
    ) -> Result<Cleanup, PlaceError> {
        let branch = self.branch_of(folder).await?;
        if branch.is_none()
            && let Some(commit) = self.unsaved_head(folder).await?
        {
            return Ok(Cleanup::UnsavedCommit { commit });
        }
        if files == LeftFiles::Discard {
            remove_tree(folder).await?;
            remove_tree(admin).await?;
            return Ok(Cleanup::Removed);
        }
        match self.check_clean_for_keep(folder).await? {
            KeepCheck::Clean => move_to_trash(folder, admin, recorded, branch).await,
            KeepCheck::Dirty(left) => Ok(Cleanup::Kept { files: left }),
            KeepCheck::HasSubmodule => Err(PlaceError::SubmodulesPresent {
                path: recorded.path.clone(),
            }),
        }
    }

    async fn remove_unregistered(&self, folder: &Path) -> Result<Cleanup, PlaceError> {
        if self
            .locate(folder)
            .await?
            .is_some_and(|worktree| worktree.top == folder)
        {
            return Ok(Cleanup::NotOwned);
        }
        if let Err(source) = tokio::fs::remove_dir(folder).await
            && source.kind() != io::ErrorKind::NotFound
        {
            return Err(PlaceError::CannotRemove {
                path: folder.display().to_string(),
                source,
            });
        }
        Ok(Cleanup::Removed)
    }

    async fn check_clean_for_keep(&self, folder: &Path) -> Result<KeepCheck, PlaceError> {
        let status = [
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ];
        let status = self.git.run(folder, &status, git::QUICK).await?;
        let left: Vec<String> = status.lines().map(str::to_owned).collect();
        if !left.is_empty() {
            return Ok(KeepCheck::Dirty(left));
        }
        let listed = self
            .git
            .run_bytes(folder, &["ls-files", "-v", "-s", "-z"], git::QUICK)
            .await?;
        if has_initialized_submodule(folder, &listed).await {
            return Ok(KeepCheck::HasSubmodule);
        }
        Ok(KeepCheck::Clean)
    }

    /// The worktree's detached `HEAD` when no ref holds it, so only the worktree keeps it
    /// reachable.
    async fn unsaved_head(&self, folder: &Path) -> Result<Option<String>, PlaceError> {
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

    async fn empty_expired_trash(&self, store: &StoreThread) {
        let Ok(workspaces) = store.call(|store| store.list_workspaces()).await else {
            return;
        };
        let mut seen = HashSet::new();
        for workspace in workspaces {
            let Ok(root) = resolved(Path::new(&workspace.path)).await else {
                continue;
            };
            let trash_dir = root.join(TRASH_FOLDER);
            if resolved(&trash_dir).await.ok().as_deref() != Some(trash_dir.as_path())
                || !seen.insert(trash_dir.clone())
            {
                continue;
            }
            let repo_lock = self
                .locate(&root)
                .await
                .ok()
                .flatten()
                .map(|repo| self.lock_of(&repo.shared));
            let _one_at_a_time = match &repo_lock {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            empty_expired_in(&trash_dir).await;
        }
    }
}

async fn move_to_trash(
    folder: &Path,
    admin: &Path,
    recorded: &KratosWorktree,
    branch: Option<String>,
) -> Result<Cleanup, PlaceError> {
    let trash_dir = trash_root(folder)?;
    ignore_everything_in(&trash_dir).await?;
    let slot = pick_free_trash_slot(&trash_dir, recorded).await?;
    if let Err(source) = tokio::fs::rename(folder, &slot.dir).await {
        if source.kind() == io::ErrorKind::NotFound {
            remove_tree(admin).await?;
            return Ok(Cleanup::Removed);
        }
        return Err(PlaceError::CannotRemove {
            path: recorded.path.clone(),
            source,
        });
    }
    write_trash_meta(&slot.meta, recorded, branch).await?;
    remove_tree(admin).await?;
    let git_file = slot.dir.join(".git");
    tokio::fs::write(&git_file, TRASH_GIT_SENTINEL)
        .await
        .map_err(|source| PlaceError::io(&git_file, source))?;
    Ok(Cleanup::Removed)
}

async fn empty_expired_in(trash_dir: &Path) {
    let Some(cutoff) = SystemTime::now().checked_sub(TRASH_RETENTION) else {
        return;
    };
    let Ok(mut entries) = tokio::fs::read_dir(trash_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some(slot) = expired_trash_slot(trash_dir, &entry, cutoff).await {
            expire_trash_slot(&slot).await;
        }
    }
}

async fn expired_trash_slot(
    trash_dir: &Path,
    entry: &tokio::fs::DirEntry,
    cutoff: SystemTime,
) -> Option<TrashSlot> {
    if !entry.file_type().await.is_ok_and(|kind| kind.is_file()) {
        return None;
    }
    let meta = entry.path();
    let stem = meta
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".json"))?;
    if !is_valid_trash_slot(stem) {
        return None;
    }
    let dir = trash_dir.join(stem);
    let bytes = tokio::fs::read(&meta).await.ok()?;
    let parsed: TrashedWorktree = serde_json::from_slice(&bytes).ok()?;
    (parsed.trashed_at.instant() <= cutoff).then_some(TrashSlot { dir, meta })
}

async fn expire_trash_slot(slot: &TrashSlot) {
    match tokio::fs::symlink_metadata(&slot.dir).await {
        Ok(meta) if !meta.file_type().is_symlink() && meta.is_dir() => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let _ = tokio::fs::remove_file(&slot.meta).await;
            return;
        }
        _ => return,
    }
    if remove_tree(&slot.dir).await.is_ok() {
        let _ = tokio::fs::remove_file(&slot.meta).await;
    }
}

fn is_valid_trash_slot(stem: &str) -> bool {
    stem.len() == TRASH_SLOT_LEN
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

async fn find_our_admin(
    repository: &Path,
    recorded: &KratosWorktree,
) -> Result<Option<PathBuf>, PlaceError> {
    let records = repository.join("worktrees");
    let io_err = |source| PlaceError::io(&records, source);
    let mut entries = match tokio::fs::read_dir(&records).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        entries => entries.map_err(io_err)?,
    };
    let expected = Path::new(&recorded.path).join(".git");
    while let Some(entry) = entries.next_entry().await.map_err(io_err)? {
        let record = entry.path();
        if !is_same_checkout_dir(&record, recorded).await {
            continue;
        }
        let Ok(gitdir) = tokio::fs::read_to_string(record.join("gitdir")).await else {
            continue;
        };
        if record.join(gitdir.trim_end_matches(['\r', '\n'])) == expected {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

fn trash_root(folder: &Path) -> Result<PathBuf, PlaceError> {
    folder
        .parent()
        .filter(|parent| {
            parent.file_name().and_then(|name| name.to_str()) == Some(WORKTREES_FOLDER)
        })
        .and_then(Path::parent)
        .map(|repo_root| repo_root.join(TRASH_FOLDER))
        .ok_or_else(|| PlaceError::CannotRemove {
            path: folder.display().to_string(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "worktree is outside .kratos-worktrees",
            ),
        })
}

fn slot_candidates(id: &WorktreeId) -> Vec<String> {
    let mut alnum: String = id
        .as_str()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    alnum.extend(std::iter::repeat_n(
        '0',
        TRASH_SLOT_LEN.saturating_sub(alnum.len()),
    ));
    alnum
        .as_bytes()
        .as_chunks::<TRASH_SLOT_LEN>()
        .0
        .iter()
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect()
}

async fn is_absent(path: &Path) -> bool {
    matches!(
        tokio::fs::symlink_metadata(path).await,
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

async fn pick_free_trash_slot(
    trash_dir: &Path,
    recorded: &KratosWorktree,
) -> Result<TrashSlot, PlaceError> {
    for slot in slot_candidates(&recorded.id) {
        let dir = trash_dir.join(&slot);
        let meta = trash_dir.join(format!("{slot}.json"));
        if is_absent(&dir).await && is_absent(&meta).await {
            return Ok(TrashSlot { dir, meta });
        }
    }
    Err(PlaceError::CannotRemove {
        path: recorded.path.clone(),
        source: io::Error::new(io::ErrorKind::AlreadyExists, "no free trash slot"),
    })
}

async fn write_trash_meta(
    meta_path: &Path,
    recorded: &KratosWorktree,
    branch: Option<String>,
) -> Result<(), PlaceError> {
    let trashed_at =
        Timestamp::now().map_err(|error| PlaceError::io(meta_path, io::Error::other(error)))?;
    let payload = TrashedWorktree {
        worktree_id: recorded.id.as_str().to_owned(),
        original_path: recorded.path.clone(),
        branch,
        trashed_at,
    };
    let bytes = serde_json::to_vec_pretty(&payload).expect("trash metadata serializes");
    tokio::fs::write(meta_path, bytes)
        .await
        .map_err(|source| PlaceError::io(meta_path, source))
}

/// Avoids Git for Windows's 260-character path limit when `core.longpaths` is false.
async fn remove_tree(path: &Path) -> Result<(), PlaceError> {
    match tokio::fs::remove_dir_all(path).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(PlaceError::CannotRemove {
            path: path.display().to_string(),
            source: error,
        }),
        _ => Ok(()),
    }
}

/// Whether `listed` (`git ls-files -v -s -z`) names any gitlink whose folder in `folder` is
/// non-empty, matching `git worktree remove`'s submodule refusal.
async fn has_initialized_submodule(folder: &Path, listed: &[u8]) -> bool {
    for entry in git::ls_files_entries(listed) {
        if entry.mode != Some(GITLINK_MODE) {
            continue;
        }
        let Some(dir) = git::file_in(folder, entry.path) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trash_slot_is_never_longer_than_a_one_char_worktree_path() {
        let folder = Path::new("/repo/.kratos-worktrees/w");
        let id = WorktreeId::new("fb5c58e7-bd79-4078-88e7-be4ccff40ceb".to_owned());
        let slot = trash_root(folder).unwrap().join(&slot_candidates(&id)[0]);
        assert!(
            slot.to_string_lossy().len() <= folder.to_string_lossy().len(),
            "{} > {}",
            slot.display(),
            folder.display()
        );
    }

    #[test]
    fn trash_slot_validator_rejects_path_traversal_and_non_slot_names() {
        assert!(is_valid_trash_slot("fb5c5"));
        for bad in [
            "..", ".", "../a", "ab/cd", "ABCDE", "fb5c", "fb5c56", ".....",
        ] {
            assert!(!is_valid_trash_slot(bad), "accepted {bad}");
        }
    }

    #[tokio::test]
    async fn empty_expired_in_ignores_dotdot_json_traversal_payload() {
        // Catches `Path::with_extension("")` turning `.kratos-trash/...json` into `<trash>/..`
        // (the repository root) and wiping the repository on startup.
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let trash = repo.join(TRASH_FOLDER);
        std::fs::create_dir_all(&trash).unwrap();
        std::fs::write(repo.join("keep.txt"), "repo file\n").unwrap();
        let payload = serde_json::json!({
            "worktreeId": "00000000-0000-0000-0000-000000000000",
            "originalPath": repo.join(WORKTREES_FOLDER).join("w"),
            "branch": "w",
            "trashedAt": "2020-01-01T00:00:00.000Z"
        })
        .to_string();
        std::fs::write(trash.join("...json"), &payload).unwrap();
        std::fs::write(trash.join("..json"), &payload).unwrap();

        empty_expired_in(&trash).await;

        assert_eq!(
            std::fs::read_to_string(repo.join("keep.txt")).unwrap(),
            "repo file\n"
        );
    }
}
