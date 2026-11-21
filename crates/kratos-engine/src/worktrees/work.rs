//! A thread's finished work (D27, D55): its diff, a commit of it, and a pull request for it.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use kratos_protocol::{ThreadDiff, ThreadId};
use kratos_store::ProviderThread;
use tokio::process::Command;
use uuid::Uuid;

use super::git::{self, CHECKOUT, Capped};
use super::index_lock::{IndexLock, PreparedReplacement, copy_preserving_mtime};
use super::{PlaceError, Worktrees, check_still_there, resolved};
use crate::store_thread::StoreThread;

/// The most patch text a diff returns, cut at a whole line. A client shows the rest by reading the
/// folder itself.
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;

/// The number of the event that started each thread's last turn, when a commit looked.
type Seen = HashMap<ThreadId, u64>;

/// Where `HEAD` points in a worktree: either a symbolic branch (possibly unborn) or a detached
/// commit.
#[derive(Debug, PartialEq, Eq)]
enum HeadTarget {
    Branch {
        reference: String,
        oid: Option<String>,
    },
    Detached(String),
}

impl HeadTarget {
    fn oid(&self) -> Option<&str> {
        match self {
            HeadTarget::Branch { oid, .. } => oid.as_deref(),
            HeadTarget::Detached(oid) => Some(oid.as_str()),
        }
    }
}

/// A git commit operation in progress together with its target OIDs (`MERGE_HEAD`,
/// `CHERRY_PICK_HEAD`, or `REVERT_HEAD`).
#[derive(Debug, PartialEq, Eq)]
struct PendingCommit {
    kind: Underway,
    heads: Vec<String>,
}

/// What a commit goes on top of: `HEAD` (symbolic branch or detached commit, plus OID) and any
/// pending merge, cherry-pick, or revert with its target OIDs.
#[derive(Debug, PartialEq, Eq)]
struct Tip {
    head: HeadTarget,
    pending: Vec<PendingCommit>,
}

impl Tip {
    fn underway(&self) -> Option<Underway> {
        self.pending.first().map(|pending| pending.kind)
    }

    fn has_merge(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.kind == Underway::Merge)
    }
}

/// Worktree state captured before preparing a commit and re-verified under git's index lock before
/// publication.
struct CommitBase {
    tip: Tip,
    whole_index: Vec<u8>,
}

/// The private scratch index and tree prepared before acquiring git's `index.lock`.
struct PreparedCommit {
    staged: PathBuf,
    updated: PathBuf,
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

impl Worktrees {
    /// What `folder` holds that `base` does not, or that its `HEAD` does not without a base, and
    /// nothing from the rest of the repository. Built in a fresh private index from `HEAD` so new
    /// files show, the user's staging stays untouched, and same-size edits inside git's timestamp
    /// window are never masked by a copied stat cache.
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
        let index = self.scratch_name();
        let patch = self
            .staged_patch(folder, head.as_deref(), &base, &index)
            .await;
        let _ = tokio::fs::remove_file(&index).await;
        let patch = patch?;
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
            self.git
                .run_in_index(folder, &["read-tree", head], git::QUICK, index)
                .await?;
        }
        self.git
            .run_in_index(folder, &["add", "--all", "--", "."], CHECKOUT, index)
            .await?;
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

    /// A scratch copy of `folder`'s worktree index with its original modified time preserved, for
    /// inspecting overlooked (`assume-unchanged` / `skip-worktree`) files without hiding racy-git
    /// edits.
    pub(super) async fn scratch_index(&self, folder: &Path) -> Result<PathBuf, PlaceError> {
        let own = self.index_of(folder).await?;
        let copy = self.scratch_name();
        copy_preserving_mtime(&own, &copy).await?;
        Ok(copy)
    }

    fn scratch_name(&self) -> PathBuf {
        self.scratch.join(format!("index-{}", Uuid::new_v4()))
    }

    /// Commits everything in `folder` as the user's git identity, with `message` and nothing
    /// added to it, and returns the new commit. Builds the commit tree in a fresh private index
    /// and publishes only if `HEAD`, pending operations, and the whole real index stay unchanged
    /// under git's index lock.
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
        let resolved_folder = resolved_or_given(folder).await;
        let is_whole_checkout = resolved_folder == repository.top;
        let base = self
            .capture_commit_base(folder, &resolved_folder, &repository.top, is_whole_checkout)
            .await?;
        let staged = self.scratch_name();
        let updated = self.scratch_name();
        let committed = async {
            let tree = self
                .prepare_commit_tree(store, thread, &repository.top, &base.tip, &staged)
                .await?;
            let prepared = PreparedCommit {
                staged: staged.clone(),
                updated: updated.clone(),
                tree,
            };
            self.publish_commit(store, thread, &repository.top, &base, &prepared, message)
                .await
        }
        .await;
        for scratch in [&staged, &updated] {
            let _ = tokio::fs::remove_file(scratch).await;
        }
        committed
    }

    async fn capture_commit_base(
        &self,
        folder: &Path,
        resolved_folder: &Path,
        top: &Path,
        is_whole_checkout: bool,
    ) -> Result<CommitBase, PlaceError> {
        let tip = self.tip(folder).await?;
        refuse_partial_during(tip.underway(), is_whole_checkout)?;
        let whole_index = self.whole_index(top).await?;
        if !is_whole_checkout && has_staged_ancestor(&whole_index, resolved_folder, top) {
            return Err(PlaceError::StagedAncestorConflict(
                folder.display().to_string(),
            ));
        }
        Ok(CommitBase { tip, whole_index })
    }

    /// Stages `folder` into a fresh private index rooted at `tip`'s `HEAD` while no overlapping
    /// thread edits it, then writes and returns the resulting tree OID.
    async fn prepare_commit_tree(
        &self,
        store: &StoreThread,
        thread: &ProviderThread,
        top: &Path,
        tip: &Tip,
        staged: &Path,
    ) -> Result<String, PlaceError> {
        let folder = Path::new(&thread.folder);
        let seen = self.refuse_while_edited(store, folder, top, None).await?;
        if let Some(head_oid) = tip.head.oid() {
            self.git
                .run_in_index(top, &["read-tree", head_oid], git::QUICK, staged)
                .await?;
        }
        self.git
            .run_in_index(folder, &["add", "--all", "--", "."], CHECKOUT, staged)
            .await?;
        self.refuse_while_edited(store, folder, top, Some(&seen))
            .await?;
        check_still_there(store, &thread.place).await?;
        self.refuse_unless_staged(folder, staged, tip.has_merge())
            .await?;
        Ok(self
            .git
            .run_in_index(folder, &["write-tree"], git::QUICK, staged)
            .await?)
    }

    /// Acquires git's native `index.lock`, re-verifies that neither the tip nor any entry or flag
    /// in the whole index changed while preparing, and prepares the updated index replacement.
    async fn prepare_locked_index(
        &self,
        store: &StoreThread,
        thread: &ProviderThread,
        top: &Path,
        base: &CommitBase,
        prepared: &PreparedCommit,
    ) -> Result<(PathBuf, PreparedReplacement), PlaceError> {
        let folder = Path::new(&thread.folder);
        let own = self.index_of(folder).await?;
        let index_lock = IndexLock::take(&own).await?;
        if self.tip(folder).await? != base.tip || self.whole_index(top).await? != base.whole_index {
            return Err(PlaceError::ChangedMeanwhile);
        }
        check_still_there(store, &thread.place).await?;
        copy_preserving_mtime(&own, &prepared.updated).await?;
        self.git
            .run_in_index(
                folder,
                &["reset", "--quiet", &prepared.tree, "--", "."],
                git::QUICK,
                &prepared.updated,
            )
            .await?;
        let replacement = index_lock.prepare_replacement(&prepared.updated).await?;
        Ok((own, replacement))
    }

    /// Publishes the commit through `prepared.staged` while holding git's `index.lock`, then
    /// atomically installs the prepared index replacement.
    async fn publish_commit(
        &self,
        store: &StoreThread,
        thread: &ProviderThread,
        top: &Path,
        base: &CommitBase,
        prepared: &PreparedCommit,
        message: &str,
    ) -> Result<String, PlaceError> {
        let folder = Path::new(&thread.folder);
        let (own, replacement) = self
            .prepare_locked_index(store, thread, top, base, prepared)
            .await?;
        let lock_path = replacement.lock_path().display().to_string();
        self.git
            .run_in_index(
                folder,
                &["commit", "--quiet", "--message", message],
                git::QUICK,
                &prepared.staged,
            )
            .await?;
        let committed = self
            .git
            .run(folder, &["rev-parse", "HEAD"], git::QUICK)
            .await?;
        replacement
            .commit()
            .await
            .map_err(|source| PlaceError::IndexNotUpdatedAfterCommit {
                commit: committed.clone(),
                path: own.display().to_string(),
                lock: lock_path,
                folder: folder.display().to_string(),
                source,
            })?;
        Ok(committed)
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
    /// or is starting there, or, with `seen`, the thread started a turn since, even one that
    /// ended. Its folder must hold `folder` or lie inside it, in the checkout at `top`. Returns
    /// what it saw, for the next look.
    async fn refuse_while_edited(
        &self,
        store: &StoreThread,
        folder: &Path,
        top: &Path,
        seen: Option<&Seen>,
    ) -> Result<Seen, PlaceError> {
        let threads = store.call(|store| store.thread_activity()).await?;
        let folder = resolved_or_given(folder).await;
        for thread in &threads {
            // A thread made since the last look counts from 0, as if it had been there idle.
            let moved_on = seen.is_some_and(|seen| {
                seen.get(&thread.thread_id).copied().unwrap_or(0) != thread.last_turn
            });
            if (thread.is_busy || moved_on)
                && self.shares_files(&folder, top, &thread.folder).await?
            {
                return Err(PlaceError::BeingEdited);
            }
        }
        Ok(threads
            .into_iter()
            .map(|thread| (thread.thread_id, thread.last_turn))
            .collect())
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

    /// The pending commit operations in `folder`'s worktree together with their target OIDs.
    async fn pending_commits(&self, folder: &Path) -> Result<Vec<PendingCommit>, PlaceError> {
        let mut args = vec!["rev-parse", "--path-format=absolute"];
        for (_, file) in Underway::FILES {
            args.extend(["--git-path", file]);
        }
        let paths = self.git.run(folder, &args, git::QUICK).await?;
        let mut pending = Vec::new();
        for (path, (kind, _)) in paths.lines().zip(Underway::FILES) {
            match tokio::fs::read_to_string(path).await {
                Ok(content) => {
                    let heads = content
                        .lines()
                        .map(|line| line.trim().to_owned())
                        .filter(|line| !line.is_empty())
                        .collect();
                    pending.push(PendingCommit { kind, heads });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(PlaceError::Io {
                        path: path.to_owned(),
                        source,
                    });
                }
            }
        }
        Ok(pending)
    }

    /// What a commit in `folder`'s worktree goes on top of: both the symbolic branch (or detached
    /// state) and commit OID of `HEAD`, plus every pending merge, cherry-pick, or revert and its
    /// target OIDs.
    async fn tip(&self, folder: &Path) -> Result<Tip, PlaceError> {
        let symref = ["symbolic-ref", "--quiet", "HEAD"];
        let reference = match self.git.run(folder, &symref, git::QUICK).await {
            Ok(reference) => Some(reference),
            Err(error) if error.exited_with(1) => None,
            Err(error) => return Err(error.into()),
        };
        let rev = ["rev-parse", "--verify", "--quiet", "HEAD"];
        let oid = match self.git.run(folder, &rev, git::QUICK).await {
            Ok(head) => Some(head),
            Err(error) if error.exited_with(1) => None,
            Err(error) => return Err(error.into()),
        };
        let head = match (reference, oid) {
            (Some(reference), oid) => HeadTarget::Branch { reference, oid },
            (None, Some(oid)) => HeadTarget::Detached(oid),
            (None, None) => {
                return Err(PlaceError::UnreadableGit {
                    command: "rev-parse --verify --quiet HEAD".to_owned(),
                    output: "HEAD is detached with no commit".to_owned(),
                });
            }
        };
        Ok(Tip {
            head,
            pending: self.pending_commits(folder).await?,
        })
    }

    /// The worktree's own index file.
    async fn index_of(&self, folder: &Path) -> Result<PathBuf, PlaceError> {
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

/// `path` with links and `..` resolved, or as given when it is gone.
async fn resolved_or_given(path: &Path) -> PathBuf {
    resolved(path).await.unwrap_or_else(|_| path.to_owned())
}

/// Refuses a commit of only a folder while git is in the middle of a commit `underway`, which the
/// commit would conclude without the rest of its changes.
fn refuse_partial_during(
    underway: Option<Underway>,
    is_whole_checkout: bool,
) -> Result<(), PlaceError> {
    match underway {
        Some(underway) if !is_whole_checkout => Err(PlaceError::PartialCommitDuring(underway)),
        _ => Ok(()),
    }
}

/// Whether `whole_index` (`git ls-files --full-name -v --stage -z`) stages a file at `folder`
/// itself or at any ancestor directory between `top` and `folder`.
fn has_staged_ancestor(whole_index: &[u8], folder: &Path, top: &Path) -> bool {
    let Ok(relative) = folder.strip_prefix(top) else {
        return false;
    };
    let rel = git_relative_bytes(relative);
    if rel.is_empty() {
        return false;
    }
    whole_index.split(|&byte| byte == 0).any(|record| {
        let Some(tab) = record.iter().position(|&byte| byte == b'\t') else {
            return false;
        };
        let path = &record[tab + 1..];
        path == rel.as_slice() || (rel.starts_with(path) && rel.get(path.len()) == Some(&b'/'))
    })
}

fn git_relative_bytes(relative: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for component in relative.components() {
        if let Component::Normal(part) = component {
            if !bytes.is_empty() {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(part.as_encoded_bytes());
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_ancestor_check_catches_folder_and_parents_only() {
        let index = b"H 100644 aaa 0\tbase.txt\0H 100644 bbb 0\tpackages/app\0\
                      H 100644 ccc 0\tcomp\0";
        let top = Path::new("/repo");

        assert!(has_staged_ancestor(
            index,
            &top.join("packages").join("app"),
            top
        ));
        assert!(has_staged_ancestor(
            index,
            &top.join("packages").join("app").join("src"),
            top
        ));
        assert!(!has_staged_ancestor(index, &top.join("component"), top));
        assert!(!has_staged_ancestor(index, &top.join("packages"), top));
    }

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
}
