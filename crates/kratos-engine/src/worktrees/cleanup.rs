//! Removing a worktree Kratos made once every thread in it is archived (D16). Keep moves a clean
//! checkout into a same-volume trash folder so late saves through open handles survive until
//! Kratos empties expired trash; Discard deletes immediately. Branches always stay.

use std::collections::HashSet;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use kratos_protocol::{Cleanup, LeftFiles, Ownership, Place, Timestamp, WorktreeId};
use kratos_store::{KratosWorktree, Removal, RemovalEnd, StoreError, UnfinishedWorktree};
use serde::{Deserialize, Serialize};

use super::git;
use super::index_lock::copy_preserving_mtime_if_present;
use super::{
    PlaceError, TRASH_FOLDER, WORKTREES_FOLDER, Worktrees, ensure_our_lock, ignore_everything_in,
    is_same_checkout_at, is_same_checkout_dir, resolved,
};
use crate::store_thread::StoreThread;

const TRASH_RETENTION: Duration = Duration::from_hours(7 * 24);
const TRASH_SLOT_LEN: usize = WORKTREES_FOLDER.len() - TRASH_FOLDER.len() + 1;
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
                self.remove_worktree(store, &worktree.id, &worktree.path, files)
                    .await
            }
            Place::Worktree(_) | Place::ProjectFolder => Ok(Cleanup::NotOwned),
        }
    }

    /// Finishes worktrees left unfinished by an earlier engine and empties expired trash entries.
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
        self.empty_expired_trash(store, TRASH_RETENTION).await;
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
        // Checked after `removing` is recorded so no new thread can start inside `path` unseen.
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
            Cleanup::Removed | Cleanup::NotOwned => RemovalEnd::Removed,
            Cleanup::Kept { .. } | Cleanup::UnsavedCommit { .. } => RemovalEnd::Kept,
            Cleanup::InUse | Cleanup::Blocked { .. } => return Ok(cleanup),
        };
        let id = id.clone();
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
        let trash_dir = trash_root(folder)?;
        let admin = find_our_admin(repository, recorded).await?;
        if let Some(slot) = find_trashed_entry(&trash_dir, recorded).await? {
            return self
                .finish_trashed(&slot, admin.as_deref(), recorded, files)
                .await;
        }
        match admin {
            Some(admin) => {
                self.remove_registered(folder, &trash_dir, &admin, recorded, files)
                    .await
            }
            None => self.remove_unregistered(folder).await,
        }
    }

    async fn remove_registered(
        &self,
        folder: &Path,
        trash_dir: &Path,
        admin: &Path,
        recorded: &KratosWorktree,
        files: LeftFiles,
    ) -> Result<Cleanup, PlaceError> {
        ensure_our_lock(admin, &recorded.path).await?;
        if is_same_checkout_at(folder, recorded).await {
            return self
                .remove_registered_live(folder, trash_dir, admin, recorded, files)
                .await;
        }
        let outcome = self.remove_unregistered(folder).await?;
        remove_tree(admin).await?;
        Ok(outcome)
    }

    async fn remove_registered_live(
        &self,
        folder: &Path,
        trash_dir: &Path,
        admin: &Path,
        recorded: &KratosWorktree,
        files: LeftFiles,
    ) -> Result<Cleanup, PlaceError> {
        if let Some(commit) = self.unsaved_head(folder).await? {
            return Ok(Cleanup::UnsavedCommit { commit });
        }
        if files == LeftFiles::Keep {
            match self.check_clean_for_keep(folder).await? {
                KeepCheck::Clean => {}
                KeepCheck::Dirty(left) => return Ok(Cleanup::Kept { files: left }),
                KeepCheck::HasSubmodule => {
                    return Err(PlaceError::SubmodulesPresent {
                        path: recorded.path.clone(),
                    });
                }
            }
        }
        ensure_our_lock(admin, &recorded.path).await?;
        if !is_same_checkout_at(folder, recorded).await {
            return Ok(Cleanup::NotOwned);
        }
        self.move_and_finish(folder, trash_dir, admin, recorded, files)
            .await
    }

    async fn move_and_finish(
        &self,
        folder: &Path,
        trash_dir: &Path,
        admin: &Path,
        recorded: &KratosWorktree,
        files: LeftFiles,
    ) -> Result<Cleanup, PlaceError> {
        let branch = self.branch_of(folder).await.ok().flatten();
        ignore_everything_in(trash_dir).await?;
        let slot = pick_free_trash_slot(trash_dir, recorded).await?;
        // Write metadata before moving so a crash right after rename stays identifiable even if a
        // replacement checkout overwrites git's admin folder at the vacated path.
        write_trash_meta(&slot.meta, recorded, branch).await?;
        match tokio::fs::rename(folder, &slot.dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let _ = remove_file_if_present(&slot.meta).await;
                remove_tree(admin).await?;
                return Ok(Cleanup::Removed);
            }
            Err(source) => {
                let _ = remove_file_if_present(&slot.meta).await;
                return Err(PlaceError::CannotRemove {
                    path: recorded.path.clone(),
                    source,
                });
            }
        }
        self.finish_trashed(&slot, Some(admin), recorded, files)
            .await
    }

    async fn finish_trashed(
        &self,
        slot: &TrashSlot,
        admin: Option<&Path>,
        recorded: &KratosWorktree,
        files: LeftFiles,
    ) -> Result<Cleanup, PlaceError> {
        if let Some(admin) = admin {
            ensure_our_lock(admin, &recorded.path).await?;
        }
        match files {
            LeftFiles::Keep => self.finish_keep_trashed(slot, admin, recorded).await?,
            LeftFiles::Discard => Self::finish_discard_trashed(slot, admin).await?,
        }
        Ok(Cleanup::Removed)
    }

    async fn finish_keep_trashed(
        &self,
        slot: &TrashSlot,
        admin: Option<&Path>,
        recorded: &KratosWorktree,
    ) -> Result<(), PlaceError> {
        if tokio::fs::symlink_metadata(&slot.meta).await.is_err() {
            let branch = self.branch_of(&slot.dir).await.ok().flatten();
            write_trash_meta(&slot.meta, recorded, branch).await?;
        }
        if let Some(admin) = admin {
            remove_tree(admin).await?;
        }
        remove_file_if_present(&slot.dir.join(".git")).await
    }

    async fn finish_discard_trashed(
        slot: &TrashSlot,
        admin: Option<&Path>,
    ) -> Result<(), PlaceError> {
        remove_tree(&slot.dir).await?;
        remove_file_if_present(&slot.meta).await?;
        if let Some(admin) = admin {
            remove_tree(admin).await?;
        }
        Ok(())
    }

    async fn remove_unregistered(&self, folder: &Path) -> Result<Cleanup, PlaceError> {
        if self
            .locate(folder)
            .await?
            .is_some_and(|worktree| worktree.top == folder)
        {
            return Ok(Cleanup::NotOwned);
        }
        remove_empty_dir(folder).await?;
        Ok(Cleanup::Removed)
    }

    async fn check_clean_for_keep(&self, folder: &Path) -> Result<KeepCheck, PlaceError> {
        let listed = self
            .git
            .run_bytes(folder, &["ls-files", "-v", "-s", "-z"], git::QUICK)
            .await?;
        let overlooked = self.overlooked_edits(folder, &listed).await?;
        let status = [
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ];
        let status = self.git.run(folder, &status, git::QUICK).await?;
        let mut left: Vec<String> = status.lines().map(str::to_owned).collect();
        left.extend(overlooked);
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

    /// Edits `git status` does not show because the file is marked assume-unchanged or
    /// skip-worktree.
    async fn overlooked_edits(
        &self,
        folder: &Path,
        listed: &[u8],
    ) -> Result<Vec<String>, PlaceError> {
        let mut edited = Vec::new();
        let mut on_disk = Vec::new();
        for path in overlooked(listed) {
            let Some(file) = file_in(folder, path) else {
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
        let own = self.index_of(folder).await?;
        let index = self.scratch_name();
        copy_preserving_mtime_if_present(&own, &index).await?;
        let changed = self.changed_once_unmarked(folder, &index, &on_disk).await;
        let _ = tokio::fs::remove_file(&index).await;
        edited.extend(changed?);
        Ok(edited)
    }

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

    async fn empty_expired_trash(&self, store: &StoreThread, retention: Duration) {
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
            empty_expired_in(&trash_dir, retention).await;
        }
    }
}

async fn empty_expired_in(trash_dir: &Path, retention: Duration) {
    let Some(cutoff) = SystemTime::now().checked_sub(retention) else {
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
    let io_err = |source| PlaceError::Io {
        path: records.display().to_string(),
        source,
    };
    let mut entries = match tokio::fs::read_dir(&records).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        entries => entries.map_err(io_err)?,
    };
    let expected = normalize_lexical(&Path::new(&recorded.path).join(".git"));
    while let Some(entry) = entries.next_entry().await.map_err(io_err)? {
        let record = entry.path();
        if !is_same_checkout_dir(&record, recorded).await {
            continue;
        }
        let Ok(gitdir) = tokio::fs::read_to_string(record.join("gitdir")).await else {
            continue;
        };
        let resolved_gitdir =
            normalize_lexical(&record.join(gitdir.trim_end_matches(['\r', '\n'])));
        if same_lexical_path(&resolved_gitdir, &expected) {
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

async fn is_our_trash_meta(meta: &Path, recorded: &KratosWorktree) -> bool {
    if !tokio::fs::symlink_metadata(meta)
        .await
        .is_ok_and(|meta| !meta.file_type().is_symlink() && meta.is_file())
    {
        return false;
    }
    tokio::fs::read(meta)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<TrashedWorktree>(&bytes).ok())
        .is_some_and(|parsed| parsed.worktree_id == recorded.id.as_str())
}

async fn find_trashed_entry(
    trash_dir: &Path,
    recorded: &KratosWorktree,
) -> Result<Option<TrashSlot>, PlaceError> {
    for slot in slot_candidates(&recorded.id) {
        let dir = trash_dir.join(&slot);
        let Ok(dir_meta) = tokio::fs::symlink_metadata(&dir).await else {
            continue;
        };
        if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
            continue;
        }
        let meta = trash_dir.join(format!("{slot}.json"));
        if is_our_trash_meta(&meta, recorded).await || is_same_checkout_at(&dir, recorded).await {
            return Ok(Some(TrashSlot { dir, meta }));
        }
    }
    Ok(None)
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
        if is_absent(&dir).await
            && (is_absent(&meta).await || is_our_trash_meta(&meta, recorded).await)
        {
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
    let trashed_at = Timestamp::now().map_err(|error| PlaceError::Io {
        path: meta_path.display().to_string(),
        source: io::Error::other(error),
    })?;
    let payload = TrashedWorktree {
        worktree_id: recorded.id.as_str().to_owned(),
        original_path: recorded.path.clone(),
        branch,
        trashed_at,
    };
    let bytes = serde_json::to_vec_pretty(&payload).expect("trash metadata serializes");
    tokio::fs::write(meta_path, bytes)
        .await
        .map_err(|source| PlaceError::Io {
            path: meta_path.display().to_string(),
            source,
        })
}

async fn remove_empty_dir(folder: &Path) -> Result<(), PlaceError> {
    match tokio::fs::remove_dir(folder).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(PlaceError::CannotRemove {
            path: folder.display().to_string(),
            source: error,
        }),
        _ => Ok(()),
    }
}

async fn remove_file_if_present(path: &Path) -> Result<(), PlaceError> {
    match tokio::fs::remove_file(path).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(PlaceError::Io {
            path: path.display().to_string(),
            source: error,
        }),
        _ => Ok(()),
    }
}

/// Removes `path` recursively through Rust's filesystem calls, avoiding Git for Windows's
/// 260-character path limit when `core.longpaths` is false.
async fn remove_tree(path: &Path) -> Result<(), PlaceError> {
    match tokio::fs::remove_dir_all(path).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(PlaceError::CannotRemove {
            path: path.display().to_string(),
            source: error,
        }),
        _ => Ok(()),
    }
}

/// Resolves `.` and `..` without touching the filesystem so relative `gitdir` entries match while
/// the worktree folder is offline.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    out
}

fn same_lexical_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// Whether `listed` (`git ls-files -v -s -z`) names any gitlink whose folder in `folder` is
/// non-empty, matching `git worktree remove`'s submodule refusal.
async fn has_initialized_submodule(folder: &Path, listed: &[u8]) -> bool {
    for (_, mode, path) in ls_files_entries(listed) {
        if mode != GITLINK_MODE {
            continue;
        }
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

fn ls_files_entries(listed: &[u8]) -> impl Iterator<Item = (&[u8], &[u8], &[u8])> {
    listed.split(|&byte| byte == 0).filter_map(|entry| {
        let tab = entry.iter().position(|&byte| byte == b'\t')?;
        let mut fields = entry[..tab].split(|&byte| byte == b' ');
        let tag = fields.next()?;
        let mode = fields.next()?;
        Some((tag, mode, &entry[tab + 1..]))
    })
}

/// Whether an unarchived thread works at `worktree` or in a folder inside it.
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

fn overlooked(listed: &[u8]) -> impl Iterator<Item = &[u8]> {
    ls_files_entries(listed).filter_map(|(tag, _, path)| {
        let is_overlooked = tag == b"S" || tag.iter().all(u8::is_ascii_lowercase);
        is_overlooked.then_some(path)
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

#[cfg(unix)]
pub(super) fn file_in(folder: &Path, path: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Some(folder.join(std::ffi::OsStr::from_bytes(path)))
}

#[cfg(not(unix))]
pub(super) fn file_in(folder: &Path, path: &[u8]) -> Option<PathBuf> {
    std::str::from_utf8(path).ok().map(|path| folder.join(path))
}

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

    #[test]
    fn relative_gitdir_normalizes_lexically_while_offline() {
        let admin = Path::new("/tmp/repo/.git/worktrees/portable");
        let target = normalize_lexical(&admin.join("../../../.kratos-worktrees/portable/.git"));
        let expected = normalize_lexical(Path::new("/tmp/repo/.kratos-worktrees/portable/.git"));
        assert!(same_lexical_path(&target, &expected));
        assert_eq!(normalize_lexical(Path::new("/../repo")), Path::new("/repo"));
    }

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

        empty_expired_in(&trash, TRASH_RETENTION).await;

        assert_eq!(
            std::fs::read_to_string(repo.join("keep.txt")).unwrap(),
            "repo file\n"
        );
    }
}
