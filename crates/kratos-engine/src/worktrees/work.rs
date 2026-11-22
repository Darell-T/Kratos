//! A thread's finished work (D27, D55): its diff, a commit of it, and a pull request for it.

use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use kratos_protocol::ThreadDiff;
use kratos_store::ProviderThread;
use tokio::process::Command;
use uuid::Uuid;

use super::cleanup;
use super::git::{self, CHECKOUT, Capped};
use super::index_lock::{IndexLock, PreparedReplacement, copy_preserving_mtime_if_present};
use super::{PlaceError, Worktrees, check_still_there, resolved};
use crate::store_thread::StoreThread;

/// The most patch text a diff returns, cut at a whole line. A client shows the rest by reading the
/// folder itself.
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;

struct CommitBase {
    head: Option<String>,
    has_merge: bool,
    whole_index: Vec<u8>,
}

struct PreparedCommit<'a> {
    staged: &'a Path,
    tree: String,
}

/// A commit git is in the middle of making, which the next commit ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Underway {
    Merge,
    CherryPick,
    Revert,
}

impl Underway {
    /// The file git keeps in the worktree's git folder while each is underway, which `git commit`
    /// deletes as it ends it.
    const FILES: [(Underway, &'static str); 3] = [
        (Underway::Merge, "MERGE_HEAD"),
        (Underway::CherryPick, "CHERRY_PICK_HEAD"),
        (Underway::Revert, "REVERT_HEAD"),
    ];
}

impl fmt::Display for Underway {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Underway::Merge => "merge",
            Underway::CherryPick => "cherry-pick",
            Underway::Revert => "revert",
        })
    }
}

pub(super) struct ScratchIndex(PathBuf);

impl Deref for ScratchIndex {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Worktrees {
    /// What `folder` holds that `base` does not, or that its `HEAD` does not without a base, and
    /// nothing from the rest of the repository. Built in a fresh private index from `HEAD` so new
    /// files show, the user's staging stays untouched, and a copied stat cache cannot hide a
    /// same-size edit.
    pub(crate) async fn diff(
        &self,
        folder: &Path,
        base: Option<&str>,
    ) -> Result<ThreadDiff, PlaceError> {
        let head = match self.head_commit(folder).await {
            Ok(head) => Some(head),
            Err(PlaceError::NoCommits) => None,
            Err(error) => return Err(error),
        };
        let base = match base {
            Some(base) => base.to_owned(),
            None => head.clone().ok_or(PlaceError::NoCommits)?,
        };
        let index = self.scratch_index();
        let patch = self
            .staged_patch(folder, head.as_deref(), &base, &index)
            .await?;
        Ok(ThreadDiff {
            base,
            patch: patch.text,
            truncated: patch.is_cut,
        })
    }

    async fn staged_patch(
        &self,
        folder: &Path,
        head: Option<&str>,
        base: &str,
        index: &Path,
    ) -> Result<Capped, PlaceError> {
        if let Some(head) = head {
            let listed = self
                .git
                .run_bytes(folder, &["ls-files", "-v", "-z"], git::QUICK)
                .await?;
            self.seed_private_index(folder, head, &listed, index)
                .await?;
        }
        self.stage_folder(folder, index).await?;
        // A diff or text-conversion program the user's config names would run outside every
        // sandbox, on files an agent wrote.
        let args = [
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            base,
            "--",
            ".",
        ];
        Ok(self
            .git
            .read_in_index(folder, &args, git::QUICK, index, MAX_PATCH_BYTES)
            .await?)
    }

    pub(super) fn scratch_index(&self) -> ScratchIndex {
        ScratchIndex(self.scratch.join(format!("index-{}", Uuid::new_v4())))
    }

    pub(crate) async fn commit(
        &self,
        store: &StoreThread,
        thread: &ProviderThread,
        message: &str,
    ) -> Result<String, PlaceError> {
        let folder = Path::new(&thread.folder);
        let repository = self
            .locate(folder)
            .await?
            .ok_or_else(|| PlaceError::NotARepository(folder.display().to_string()))?;
        let lock = self.lock_of(&repository.shared);
        let _one_at_a_time = lock.lock().await;
        let own = self.index_of(folder).await?;
        let index_lock = IndexLock::take(&own).await?;
        let resolved_folder = resolved_or_given(folder).await;
        let is_whole_checkout = resolved_folder == repository.top;
        let base = self
            .capture_commit_base(folder, &repository.top, is_whole_checkout)
            .await?;
        let staged = self.scratch_index();
        let tree = self
            .prepare_commit_tree(store, thread, &repository.top, &base, &staged)
            .await?;
        let prepared = PreparedCommit {
            staged: &staged,
            tree,
        };
        self.publish_commit(store, thread, &own, index_lock, &prepared, message)
            .await
    }

    async fn capture_commit_base(
        &self,
        folder: &Path,
        top: &Path,
        is_whole_checkout: bool,
    ) -> Result<CommitBase, PlaceError> {
        let pending = self.pending_commits(folder).await?;
        if let Some(&underway) = pending.first()
            && !is_whole_checkout
        {
            return Err(PlaceError::PartialCommitDuring(underway));
        }
        let head = match self.head_commit(folder).await {
            Ok(head) => Some(head),
            Err(PlaceError::NoCommits) => None,
            Err(error) => return Err(error),
        };
        let whole_index = self.whole_index(top).await?;
        Ok(CommitBase {
            head,
            has_merge: pending.contains(&Underway::Merge),
            whole_index,
        })
    }

    async fn prepare_commit_tree(
        &self,
        store: &StoreThread,
        thread: &ProviderThread,
        top: &Path,
        base: &CommitBase,
        staged: &Path,
    ) -> Result<String, PlaceError> {
        let folder = Path::new(&thread.folder);
        self.refuse_while_edited(store, folder, top).await?;
        if let Some(head_oid) = base.head.as_deref() {
            self.seed_private_index(top, head_oid, &base.whole_index, staged)
                .await?;
        }
        self.stage_folder(folder, staged).await?;
        check_still_there(store, &thread.place).await?;
        self.refuse_unless_staged(folder, staged, base.has_merge)
            .await?;
        Ok(self
            .git
            .run_in_index(folder, &["write-tree"], git::QUICK, staged)
            .await?)
    }

    async fn stage_folder(&self, folder: &Path, index: &Path) -> Result<(), PlaceError> {
        self.git
            .run_in_index(folder, &["add", "--all", "--", "."], CHECKOUT, index)
            .await?;
        // Carry over force-added ignored files that `git add --all` skips, excluding
        // `skip-worktree` entries because `git add --force` rejects sparse-excluded paths.
        let args = [
            "ls-files",
            "-v",
            "--cached",
            "--ignored",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ];
        let listed = self.git.run_bytes(folder, &args, git::QUICK).await?;
        let mut forced = Vec::new();
        for path in non_sparse_paths(&listed) {
            let Some(file) = cleanup::file_in(folder, path) else {
                continue;
            };
            if tokio::fs::symlink_metadata(&file)
                .await
                .is_ok_and(|meta| meta.is_file() || meta.file_type().is_symlink())
            {
                forced.extend_from_slice(b":(literal)");
                forced.extend_from_slice(path);
                forced.push(0);
            }
        }
        if !forced.is_empty() {
            let args = [
                "add",
                "--force",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ];
            self.git
                .run_in_index_with_input(folder, &args, forced, CHECKOUT, index)
                .await?;
        }
        Ok(())
    }

    /// Reapplies `skip-worktree` bits from `listed` so sparse-checkout files absent on disk are
    /// not treated as deletions by `git add --all`.
    async fn seed_private_index(
        &self,
        cwd: &Path,
        head: &str,
        listed: &[u8],
        index: &Path,
    ) -> Result<(), PlaceError> {
        self.git
            .run_in_index(cwd, &["read-tree", head], git::QUICK, index)
            .await?;
        let skip_paths = skip_worktree_input(listed);
        if !skip_paths.is_empty() {
            let args = ["update-index", "--skip-worktree", "-z", "--stdin"];
            self.git
                .run_in_index_with_input(cwd, &args, skip_paths, git::QUICK, index)
                .await?;
        }
        Ok(())
    }

    async fn prepare_locked_index(
        &self,
        store: &StoreThread,
        thread: &ProviderThread,
        own: &Path,
        index_lock: IndexLock,
        prepared: &PreparedCommit<'_>,
    ) -> Result<PreparedReplacement, PlaceError> {
        let folder = Path::new(&thread.folder);
        check_still_there(store, &thread.place).await?;
        let updated = self.scratch_index();
        copy_preserving_mtime_if_present(own, &updated).await?;
        self.git
            .run_in_index(
                folder,
                &["reset", "--quiet", &prepared.tree, "--", "."],
                git::QUICK,
                &updated,
            )
            .await?;
        index_lock.prepare_replacement(&updated).await
    }

    async fn publish_commit(
        &self,
        store: &StoreThread,
        thread: &ProviderThread,
        own: &Path,
        index_lock: IndexLock,
        prepared: &PreparedCommit<'_>,
        message: &str,
    ) -> Result<String, PlaceError> {
        let folder = Path::new(&thread.folder);
        let replacement = self
            .prepare_locked_index(store, thread, own, index_lock, prepared)
            .await?;
        let lock_path = replacement.lock_path().display().to_string();
        self.git
            .run_in_index(
                folder,
                &["commit", "--quiet", "--message", message],
                git::QUICK,
                prepared.staged,
            )
            .await?;
        let head = self
            .git
            .run(folder, &["rev-parse", "HEAD"], git::QUICK)
            .await;
        if let Err(source) = replacement.commit().await {
            return Err(PlaceError::IndexNotUpdatedAfterCommit {
                commit: head.unwrap_or_else(|_| "HEAD".to_owned()),
                path: own.display().to_string(),
                lock: lock_path,
                folder: folder.display().to_string(),
                source,
            });
        }
        Ok(head?)
    }

    /// Snapshot of every entry, stage, and flag in the checkout's real index, immune to stat-only
    /// refreshes from commands such as `git status`.
    async fn whole_index(&self, top: &Path) -> Result<Vec<u8>, PlaceError> {
        Ok(self
            .git
            .run_bytes(
                top,
                &["ls-files", "--full-name", "-v", "--stage", "-z"],
                git::QUICK,
            )
            .await?)
    }

    /// Refuses a commit of `staged` that would change no file, except to conclude a merge, which
    /// git records with both parents.
    async fn refuse_unless_staged(
        &self,
        folder: &Path,
        staged: &Path,
        has_merge: bool,
    ) -> Result<(), PlaceError> {
        let args = ["diff", "--cached", "--quiet"];
        match self
            .git
            .run_in_index(folder, &args, git::QUICK, staged)
            .await
        {
            Ok(_) if has_merge => Ok(()),
            Ok(_) => Err(PlaceError::NothingToCommit),
            Err(error) if error.exited_with(1) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Refuses while a thread may be editing files a commit of `folder` would take: a turn runs
    /// or is starting there. Its folder must hold `folder` or lie inside it, in the checkout at
    /// `top`.
    async fn refuse_while_edited(
        &self,
        store: &StoreThread,
        folder: &Path,
        top: &Path,
    ) -> Result<(), PlaceError> {
        let threads = store.call(|store| store.thread_activity()).await?;
        let folder = resolved_or_given(folder).await;
        for thread in &threads {
            if thread.is_busy && self.shares_files(&folder, top, &thread.folder).await? {
                return Err(PlaceError::BeingEdited);
            }
        }
        Ok(())
    }

    /// Whether an agent working in `other` can edit files of `folder`, which is in the checkout at
    /// `top`: one folder holds the other, and both are in that checkout. Folders are compared
    /// with links and `..` resolved, so another path to a folder counts. A worktree inside a
    /// project folder is a checkout of its own.
    async fn shares_files(
        &self,
        folder: &Path,
        top: &Path,
        other: &str,
    ) -> Result<bool, PlaceError> {
        let other = resolved_or_given(Path::new(other)).await;
        if !(other.starts_with(folder) || folder.starts_with(&other)) {
            return Ok(false);
        }
        Ok(self
            .locate(&other)
            .await?
            .is_some_and(|found| found.top == top))
    }

    async fn pending_commits(&self, folder: &Path) -> Result<Vec<Underway>, PlaceError> {
        let mut args = vec!["rev-parse", "--path-format=absolute"];
        for (_, file) in Underway::FILES {
            args.extend(["--git-path", file]);
        }
        let paths = self.git.run(folder, &args, git::QUICK).await?;
        let mut pending = Vec::new();
        for (path, (kind, _)) in paths.lines().zip(Underway::FILES) {
            if tokio::fs::try_exists(path)
                .await
                .map_err(|source| PlaceError::Io {
                    path: path.to_owned(),
                    source,
                })?
            {
                pending.push(kind);
            }
        }
        Ok(pending)
    }

    /// The worktree's own index file.
    pub(super) async fn index_of(&self, folder: &Path) -> Result<PathBuf, PlaceError> {
        let args = ["rev-parse", "--path-format=absolute", "--git-path", "index"];
        Ok(PathBuf::from(
            self.git.run(folder, &args, git::QUICK).await?,
        ))
    }

    /// Pushes the branch the thread's folder has checked out to `origin`, then opens a pull
    /// request for it with the user's own `gh` and returns its address. A branch that is
    /// `origin`'s default is refused before the push, which would put the work on it unreviewed.
    pub(crate) async fn pull_request(
        &self,
        store: &StoreThread,
        thread: &ProviderThread,
        title: &str,
        body: &str,
    ) -> Result<String, PlaceError> {
        let folder = Path::new(&thread.folder);
        let lock = self.lock_of_folder(folder).await?;
        let _one_at_a_time = lock.lock().await;
        let branch = self
            .branch_of(folder)
            .await?
            .ok_or_else(|| PlaceError::NoBranch(folder.display().to_string()))?;
        if self.default_branch(folder).await? == branch {
            return Err(PlaceError::OnDefaultBranch(branch));
        }
        // Checked once the branch is read, as another checkout may have taken the worktree's
        // path while the pull request waited, and its branch is not the thread's to publish.
        check_still_there(store, &thread.place).await?;
        let push = ["push", "--set-upstream", "origin", &branch];
        self.git.run(folder, &push, CHECKOUT).await?;
        // `=` keeps a title or body that starts with a dash from reading as a flag.
        let args = [
            "pr".to_owned(),
            "create".to_owned(),
            format!("--title={title}"),
            format!("--body={body}"),
            format!("--head={branch}"),
        ];
        let created = self.gh(folder, &args).await?;
        Ok(created.lines().last().unwrap_or_default().trim().to_owned())
    }

    /// The branch `origin`'s `HEAD` names, which `gh` opens a pull request against. A remote that
    /// names none, as an empty one does, is refused: a push there would publish the thread's
    /// branch as its first branch, which GitHub makes the default.
    async fn default_branch(&self, folder: &Path) -> Result<String, PlaceError> {
        let args = ["ls-remote", "--symref", "origin", "HEAD"];
        let listed = self.git.run(folder, &args, CHECKOUT).await?;
        listed
            .lines()
            .find_map(|line| {
                let branch = line
                    .strip_prefix("ref: refs/heads/")?
                    .strip_suffix("\tHEAD")?;
                Some(branch.to_owned())
            })
            .ok_or(PlaceError::NoDefaultBranch)
    }

    async fn gh(&self, folder: &Path, args: &[String]) -> Result<String, PlaceError> {
        let mut gh = Command::new(&self.gh);
        gh.current_dir(folder)
            .args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = match tokio::time::timeout(CHECKOUT, gh.output()).await {
            Err(_) => return Err(PlaceError::Gh("gh did not finish in time".to_owned())),
            Ok(Err(error)) => return Err(PlaceError::NoGh(error)),
            Ok(Ok(output)) => output,
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(PlaceError::Gh(stderr));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

async fn resolved_or_given(path: &Path) -> PathBuf {
    resolved(path).await.unwrap_or_else(|_| path.to_owned())
}

fn non_sparse_paths(listed: &[u8]) -> impl Iterator<Item = &[u8]> {
    listed.split(|&byte| byte == 0).filter_map(|record| {
        let (&tag, rest) = record.split_first()?;
        let path = rest.strip_prefix(b" ")?;
        (!matches!(tag, b'S' | b's') && !path.is_empty()).then_some(path)
    })
}

fn skip_worktree_input(listed: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in listed.split(|&byte| byte == 0) {
        let Some(&tag) = record.first() else {
            continue;
        };
        if tag != b'S' && tag != b's' {
            continue;
        }
        let path = match record.iter().position(|&byte| byte == b'\t') {
            Some(tab) => &record[tab + 1..],
            None if record.get(1) == Some(&b' ') => &record[2..],
            None => continue,
        };
        out.extend_from_slice(path);
        out.push(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn partial_commit_error_recommends_scoped_reset_after_lock_cleanup() {
        let error = PlaceError::IndexNotUpdatedAfterCommit {
            commit: "abc123".to_owned(),
            path: "/repo/.git/index".to_owned(),
            lock: "/repo/.git/index.lock".to_owned(),
            folder: "/repo/component".to_owned(),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "busy"),
        };
        let message = error.to_string();

        assert!(message.contains("abc123"), "{message}");
        assert!(message.contains("/repo/.git/index.lock"), "{message}");
        assert!(
            message.contains("git -C \"/repo/component\" reset --mixed HEAD -- ."),
            "{message}"
        );
        assert!(
            !message.contains("`git reset`"),
            "must never advise a repository-wide git reset: {message}"
        );
    }

    #[test]
    fn skip_worktree_input_extracts_sparse_paths_with_or_without_stage_fields() {
        let staged =
            b"H 100644 111 0\ta.txt\0S 100644 222 0\tsparse/b.txt\0s 100644 333 0\tc.txt\0";
        assert_eq!(skip_worktree_input(staged), b"sparse/b.txt\0c.txt\0");
        let short = b"H a.txt\0S sparse/b.txt\0h assumed.txt\0";
        assert_eq!(skip_worktree_input(short), b"sparse/b.txt\0");
    }
}
