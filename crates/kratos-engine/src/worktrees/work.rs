//! A thread's finished work (D27, D55): its diff, a commit of it, and a pull request for it.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use kratos_protocol::{ThreadDiff, ThreadId};
use tokio::process::Command;
use uuid::Uuid;

use super::git::{self, CHECKOUT, Capped};
use super::{PlaceError, Worktrees, resolved};
use crate::store_thread::StoreThread;

/// The most patch text a diff returns, cut at a whole line. A client shows the rest by reading the
/// folder itself.
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;

/// The number of the last event of each thread that is not archived, when a commit looked.
type Seen = HashMap<ThreadId, u64>;

/// A commit git is in the middle of making, which the next commit ends.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Underway {
    Merge,
    CherryPick,
}

impl fmt::Display for Underway {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Underway::Merge => "merge",
            Underway::CherryPick => "cherry-pick",
        })
    }
}

impl Worktrees {
    /// What `folder` holds that `base` does not, or that its `HEAD` does not without a base, and
    /// nothing from the rest of the repository. The files are staged in a scratch index, so new files show
    /// and the user's staged changes stay as they were.
    pub(crate) async fn diff(
        &self,
        folder: &Path,
        base: Option<&str>,
    ) -> Result<ThreadDiff, PlaceError> {
        let base = match base {
            Some(base) => base.to_owned(),
            None => self.head_commit(folder).await?,
        };
        let index = self.scratch_index(folder).await?;
        let patch = self.staged_patch(folder, &base, &index).await;
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
        base: &str,
        index: &Path,
    ) -> Result<Capped, PlaceError> {
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

    /// A scratch copy of `folder`'s worktree index, for git to stage into without touching the
    /// user's own. The copy holds each file's last known state, so git hashes only the files that
    /// changed since. A worktree with no index yet gets none.
    pub(super) async fn scratch_index(&self, folder: &Path) -> Result<PathBuf, PlaceError> {
        let index = self.scratch.join(format!("index-{}", Uuid::new_v4()));
        let own_index = self.index_of(folder).await?;
        match tokio::fs::copy(&own_index, &index).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(PlaceError::Io {
                path: index.display().to_string(),
                source: error,
            }),
            _ => Ok(index),
        }
    }

    /// Commits everything in `folder` as the user's git identity, with `message` and nothing
    /// added to it, and returns the new commit. A project can be a folder inside its repository,
    /// so changes outside `folder`, staged or not, stay as they were. Refused while a thread may
    /// be editing what the commit would take.
    pub(crate) async fn commit(
        &self,
        store: &StoreThread,
        folder: &Path,
        message: &str,
    ) -> Result<String, PlaceError> {
        let repository = self
            .locate(folder)
            .await?
            .ok_or_else(|| PlaceError::NotARepository(folder.display().to_string()))?;
        let lock = self.lock_of(&repository.shared);
        let _one_at_a_time = lock.lock().await;
        let is_whole_checkout = resolved(folder)
            .await
            .is_ok_and(|folder| folder == repository.top);
        let underway = self.underway(folder).await?;
        if let Some(underway) = underway
            && !is_whole_checkout
        {
            return Err(PlaceError::PartialCommitDuring(underway));
        }
        let index = self.scratch_index(folder).await?;
        // Staged once the lock is held, since the commit may have waited for it behind another
        // git command of the repository while a turn started.
        let committed = async {
            self.stage(store, folder, &repository.top, is_whole_checkout, &index)
                .await?;
            self.commit_index(folder, &index, message, underway).await
        }
        .await;
        let _ = tokio::fs::remove_file(&index).await;
        committed
    }

    /// Stages `folder` in `index`, a scratch copy of its worktree's index, while no thread can be
    /// editing it. A commit of `index` then holds the files as git read them here, since git never
    /// reads them again, and a refused commit leaves the user's index as it was.
    async fn stage(
        &self,
        store: &StoreThread,
        folder: &Path,
        top: &Path,
        is_whole_checkout: bool,
        index: &Path,
    ) -> Result<(), PlaceError> {
        let seen = self.refuse_while_edited(store, folder, top, None).await?;
        if !is_whole_checkout {
            // The commit holds the folder's changes on top of HEAD, and nothing staged elsewhere.
            let to_head = ["reset", "--quiet", "--", ":/"];
            self.git
                .run_in_index(folder, &to_head, git::QUICK, index)
                .await?;
        }
        let add = ["add", "--all", "--", "."];
        self.git.run_in_index(folder, &add, CHECKOUT, index).await?;
        self.refuse_while_edited(store, folder, top, Some(&seen))
            .await?;
        Ok(())
    }

    /// Commits what `index` holds, and returns the new commit. A merge `underway` goes in even
    /// when it changes no file, as git records it with both parents.
    async fn commit_index(
        &self,
        folder: &Path,
        index: &Path,
        message: &str,
        underway: Option<Underway>,
    ) -> Result<String, PlaceError> {
        let staged = ["diff", "--cached", "--quiet"];
        match self
            .git
            .run_in_index(folder, &staged, git::QUICK, index)
            .await
        {
            Ok(_) if matches!(underway, Some(Underway::Merge)) => {}
            Ok(_) => return Err(PlaceError::NothingToCommit),
            Err(error) if error.exited_with(1) => {}
            Err(error) => return Err(error.into()),
        }
        let commit = ["commit", "--quiet", "--message", message];
        self.git
            .run_in_index(folder, &commit, git::QUICK, index)
            .await?;
        // The user's index still holds the committed files as they were before the commit.
        let to_head = ["reset", "--quiet", "--", "."];
        self.git.run(folder, &to_head, git::QUICK).await?;
        Ok(self
            .git
            .run(folder, &["rev-parse", "HEAD"], git::QUICK)
            .await?)
    }

    /// Refuses while a thread may be editing files a commit of `folder` would take: a turn runs
    /// or is starting there, or, with `seen`, the thread did anything since, such as a whole
    /// turn. Its folder must hold `folder` or lie inside it, in the checkout at `top`. Returns
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
            let moved_on =
                seen.is_some_and(|seen| seen.get(&thread.thread_id) != Some(&thread.last_seq));
            if (thread.is_busy || moved_on)
                && self.shares_files(&folder, top, &thread.folder).await?
            {
                return Err(PlaceError::BeingEdited);
            }
        }
        Ok(threads
            .into_iter()
            .map(|thread| (thread.thread_id, thread.last_seq))
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

    /// The commit git is in the middle of making in `folder`'s worktree, if any.
    async fn underway(&self, folder: &Path) -> Result<Option<Underway>, PlaceError> {
        let args = [
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "MERGE_HEAD",
            "--git-path",
            "CHERRY_PICK_HEAD",
        ];
        let paths = self.git.run(folder, &args, git::QUICK).await?;
        for (path, underway) in paths.lines().zip([Underway::Merge, Underway::CherryPick]) {
            if tokio::fs::symlink_metadata(path).await.is_ok() {
                return Ok(Some(underway));
            }
        }
        Ok(None)
    }

    /// The worktree's own index file.
    async fn index_of(&self, folder: &Path) -> Result<PathBuf, PlaceError> {
        let args = ["rev-parse", "--path-format=absolute", "--git-path", "index"];
        Ok(PathBuf::from(
            self.git.run(folder, &args, git::QUICK).await?,
        ))
    }

    /// Pushes the branch `folder` has checked out to `origin`, then opens a pull request for it
    /// with the user's own `gh` and returns its address. A branch that is `origin`'s default is
    /// refused before the push, which would put the work on it unreviewed.
    pub(crate) async fn pull_request(
        &self,
        folder: &Path,
        title: &str,
        body: &str,
    ) -> Result<String, PlaceError> {
        let branch = self
            .branch_of(folder)
            .await?
            .ok_or_else(|| PlaceError::NoBranch(folder.display().to_string()))?;
        if self.default_branch(folder).await? == branch {
            return Err(PlaceError::OnDefaultBranch(branch));
        }
        let lock = self.lock_of_folder(folder).await?;
        let _one_at_a_time = lock.lock().await;
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
