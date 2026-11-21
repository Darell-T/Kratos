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
use super::{
    PlaceError, TRASH_FOLDER, WORKTREES_FOLDER, Worktrees, ensure_our_lock, ignore_everything_in,
    is_same_checkout_at, is_same_checkout_dir, resolved,
};
use crate::store_thread::StoreThread;

/// How long a trashed worktree waits before startup sweep deletes it (measured from both trash
/// time and the newest file modification inside the trashed checkout).
const TRASH_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Length of each deterministic trash slot name inside [`TRASH_FOLDER`]. Because `.kratos-trash`
/// is four characters shorter than `.kratos-worktrees`, a five-character slot keeps every trashed
/// path no longer than even a one-character worktree name.
const TRASH_SLOT_LEN: usize = 5;

/// Outcome of checking whether a worktree may be archived under [`LeftFiles::Keep`].
enum KeepCheck {
    Clean,
    Dirty(Vec<String>),
    HasSubmodule,
}

/// Metadata written beside a trashed worktree (`<trash>/<slot>.json`) so restarts recognize it
/// after git's admin folder is gone and the user can locate and restore it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrashedWorktree {
    worktree_id: String,
    original_path: String,
    branch: Option<String>,
    trashed_at: Timestamp,
}

/// A slot inside `<repo>/.kratos-trash/` and its sibling metadata file.
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

    /// Reconciles disk, trash, and git's admin record for `recorded` under the repository lock.
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
        if self.is_live_checkout(folder).await? {
            remove_tree(admin).await?;
            return Ok(Cleanup::NotOwned);
        }
        remove_empty_dir(folder).await?;
        remove_tree(admin).await?;
        Ok(Cleanup::Removed)
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
        match rename_long(folder, &slot.dir).await {
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
        if self.is_live_checkout(folder).await? {
            return Ok(Cleanup::NotOwned);
        }
        remove_empty_dir(folder).await?;
        Ok(Cleanup::Removed)
    }

    async fn is_live_checkout(&self, folder: &Path) -> Result<bool, PlaceError> {
        Ok(self
            .locate(folder)
            .await?
            .is_some_and(|worktree| worktree.top == folder))
    }

    /// Checks whether `folder` has any file or initialized submodule that [`LeftFiles::Keep`] must
    /// preserve. Runs `ls-files` before `status` so `status` is the final scan right before moving.
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
    /// skip-worktree. Compares in an mtime-preserved scratch index copy so racy-git checks stay
    /// active and mode/type changes show as git reports them.
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
        let index = self.scratch_index(folder).await?;
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
            let trash_dir = Path::new(&workspace.path).join(TRASH_FOLDER);
            if !seen.insert(trash_dir.clone()) {
                continue;
            }
            let Ok(meta) = tokio::fs::symlink_metadata(&trash_dir).await else {
                continue;
            };
            if !meta.file_type().is_symlink() && meta.is_dir() {
                empty_expired_in(&trash_dir, retention).await;
            }
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
        let meta_path = entry.path();
        if meta_path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = tokio::fs::read(&meta_path).await else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice::<TrashedWorktree>(&bytes) else {
            continue;
        };
        if meta.trashed_at.instant() > cutoff {
            continue;
        }
        let dir = meta_path.with_extension("");
        if has_entry_newer_than(&dir, cutoff).await {
            continue;
        }
        if remove_tree(&dir).await.is_ok() {
            let _ = tokio::fs::remove_file(&meta_path).await;
        }
    }
}

/// Whether `dir` contains any file modified after `cutoff`, protecting late saves from expiry.
async fn has_entry_newer_than(dir: &Path, cutoff: SystemTime) -> bool {
    let dir = long_path(dir);
    tokio::task::spawn_blocking(move || has_entry_newer_than_sync(&dir, cutoff))
        .await
        .unwrap_or(true)
}

fn has_entry_newer_than_sync(dir: &Path, cutoff: SystemTime) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            return true;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if has_entry_newer_than_sync(&entry.path(), cutoff) {
                return true;
            }
        } else if meta.modified().map_or(true, |modified| modified > cutoff) {
            return true;
        }
    }
    false
}

/// Finds the git worktree admin folder under `repository/worktrees` carrying `recorded`'s Kratos
/// id whose lexically normalized `gitdir` (handling relative `..` links offline) matches
/// `recorded.path/.git`.
async fn find_our_admin(
    repository: &Path,
    recorded: &KratosWorktree,
) -> Result<Option<PathBuf>, PlaceError> {
    let records = repository.join("worktrees");
    let mut entries = match tokio::fs::read_dir(&records).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        entries => entries.map_err(|source| PlaceError::Io {
            path: records.display().to_string(),
            source,
        })?,
    };
    let expected = normalize_lexical(&Path::new(&recorded.path).join(".git"));
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|source| PlaceError::Io {
            path: records.display().to_string(),
            source,
        })?
    {
        let record = entry.path();
        if !is_same_checkout_dir(&record, recorded).await {
            continue;
        }
        let Ok(gitdir) = tokio::fs::read_to_string(record.join("gitdir")).await else {
            continue;
        };
        let resolved_gitdir = normalize_lexical(&record.join(gitdir.trim()));
        if same_lexical_path(&resolved_gitdir, &expected) {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

fn trash_root(folder: &Path) -> Result<PathBuf, PlaceError> {
    let parent = folder.parent().ok_or_else(|| PlaceError::CannotRemove {
        path: folder.display().to_string(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "worktree has no parent folder"),
    })?;
    if parent.file_name().and_then(|name| name.to_str()) == Some(WORKTREES_FOLDER)
        && let Some(repo_root) = parent.parent()
    {
        return Ok(repo_root.join(TRASH_FOLDER));
    }
    Ok(parent.join(TRASH_FOLDER))
}

fn slot_candidates(id: &WorktreeId) -> Vec<String> {
    let alnum: String = id
        .as_str()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    let mut slots = Vec::new();
    for chunk in alnum.as_bytes().chunks(TRASH_SLOT_LEN) {
        if chunk.len() == TRASH_SLOT_LEN {
            slots.push(String::from_utf8_lossy(chunk).into_owned());
        }
    }
    if slots.is_empty() {
        let mut padded = alnum;
        while padded.len() < TRASH_SLOT_LEN {
            padded.push('0');
        }
        slots.push(padded);
    }
    slots
}

async fn is_our_trash_meta(meta: &Path, recorded: &KratosWorktree) -> bool {
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
        if tokio::fs::symlink_metadata(&dir).await.is_err() {
            continue;
        }
        let meta = trash_dir.join(format!("{slot}.json"));
        if is_our_trash_meta(&meta, recorded).await || is_same_checkout_at(&dir, recorded).await {
            return Ok(Some(TrashSlot { dir, meta }));
        }
    }
    Ok(None)
}

async fn pick_free_trash_slot(
    trash_dir: &Path,
    recorded: &KratosWorktree,
) -> Result<TrashSlot, PlaceError> {
    for slot in slot_candidates(&recorded.id) {
        let dir = trash_dir.join(&slot);
        let meta = trash_dir.join(format!("{slot}.json"));
        if tokio::fs::symlink_metadata(&dir).await.is_err()
            && (tokio::fs::symlink_metadata(&meta).await.is_err()
                || is_our_trash_meta(&meta, recorded).await)
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
    match tokio::fs::remove_dir(long_path(folder)).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(PlaceError::CannotRemove {
            path: folder.display().to_string(),
            source: error,
        }),
        _ => Ok(()),
    }
}

async fn remove_file_if_present(path: &Path) -> Result<(), PlaceError> {
    match tokio::fs::remove_file(long_path(path)).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(PlaceError::Io {
            path: path.display().to_string(),
            source: error,
        }),
        _ => Ok(()),
    }
}

async fn rename_long(from: &Path, to: &Path) -> io::Result<()> {
    tokio::fs::rename(long_path(from), long_path(to)).await
}

/// Removes `path` recursively using Kratos's own filesystem calls (with extended-length paths and
/// read-only clearing on Windows), avoiding git's 260-character path limit.
async fn remove_tree(path: &Path) -> Result<(), PlaceError> {
    let shown = path.display().to_string();
    let target = long_path(path);
    tokio::task::spawn_blocking(move || remove_tree_sync(&target))
        .await
        .map_err(|error| PlaceError::CannotRemove {
            path: shown.clone(),
            source: io::Error::other(error),
        })?
        .map_err(|source| PlaceError::CannotRemove {
            path: shown,
            source,
        })
}

fn remove_tree_sync(target: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            clear_readonly_recursive(target);
            match std::fs::remove_dir_all(target) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            }
        }
    }
}

fn clear_readonly_recursive(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    make_owner_writable(path, &meta);
    if meta.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for entry in entries.flatten() {
            clear_readonly_recursive(&entry.path());
        }
    }
}

#[cfg(windows)]
fn make_owner_writable(path: &Path, meta: &std::fs::Metadata) {
    let mut perms = meta.permissions();
    if perms.readonly() {
        #[expect(
            clippy::permissions_set_readonly_false,
            reason = "Windows clears FILE_ATTRIBUTE_READONLY without changing POSIX world-write bits"
        )]
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(unix)]
fn make_owner_writable(path: &Path, meta: &std::fs::Metadata) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = meta.permissions();
    let extra = if meta.is_dir() { 0o700 } else { 0o200 };
    let mode = perms.mode();
    if mode & extra != extra {
        perms.set_mode(mode | extra);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(windows)]
fn long_path(path: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path.to_path_buf();
    }
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if text.starts_with(r"\\?\") || text.starts_with(r"\\.\") {
        return path.to_path_buf();
    }
    let normalized = text.replace('/', "\\");
    if let Some(unc) = normalized.strip_prefix(r"\\") {
        PathBuf::from(format!(r"\\?\UNC\{unc}"))
    } else {
        PathBuf::from(format!(r"\\?\{normalized}"))
    }
}

#[cfg(not(windows))]
fn long_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Resolves `.` and `..` without touching the filesystem so relative `gitdir` entries match while
/// the worktree folder is offline.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(component);
                }
            }
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

/// The path of each index entry in `git ls-files -v -s -z` output marked assume-unchanged or
/// skip-worktree.
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

#[cfg(unix)]
fn file_in(folder: &Path, path: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Some(folder.join(std::ffi::OsStr::from_bytes(path)))
}

#[cfg(not(unix))]
fn file_in(folder: &Path, path: &[u8]) -> Option<PathBuf> {
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
}
