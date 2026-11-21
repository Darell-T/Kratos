//! A thread's finished work (D27, D55): its diff, a commit of it, and a pull request for it.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use kratos_protocol::ThreadDiff;
use tokio::process::Command;
use uuid::Uuid;

use super::git::{self, CHECKOUT, Capped};
use super::{PlaceError, Worktrees, resolved};
use crate::store_thread::StoreThread;

/// The most patch text a diff returns, cut at a whole line. A client shows the rest by reading the
/// folder itself.
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;

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
        let index = self.scratch.join(format!("index-{}", Uuid::new_v4()));
        let own_index = self.index_of(folder).await?;
        // The worktree's own index holds each file's last known state, so git hashes only the
        // files that changed since.
        match tokio::fs::copy(&own_index, &index).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(PlaceError::Io {
                    path: index.display().to_string(),
                    source: error,
                });
            }
            _ => {}
        }
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

    /// Refuses while a turn runs, or is starting, in a thread that may be editing files a commit
    /// of `folder` would take: one folder holds the other, and both are in the checkout at `top`.
    /// Folders are compared with links and `..` resolved, so another path to a folder is caught. A
    /// worktree inside a project folder is a checkout of its own, so its turns do not count.
    async fn refuse_while_edited(
        &self,
        store: &StoreThread,
        folder: &Path,
        top: &Path,
    ) -> Result<(), PlaceError> {
        let busy = store.call(|store| store.busy_folders()).await?;
        let folder = resolved_or_given(folder).await;
        for other in busy {
            let other = resolved_or_given(Path::new(&other)).await;
            let overlaps = other.starts_with(&folder) || folder.starts_with(&other);
            if overlaps
                && self
                    .locate(&other)
                    .await?
                    .is_some_and(|found| found.top == top)
            {
                return Err(PlaceError::BeingEdited);
            }
        }
        Ok(())
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
        // Staged once the lock is held, since the commit may have waited for it behind another
        // git command of the repository while a turn started.
        self.stage(store, folder, &repository.top).await?;
        let staged = ["diff", "--cached", "--quiet", "--", "."];
        match self.git.run(folder, &staged, git::QUICK).await {
            Ok(_) => return Err(PlaceError::NothingToCommit),
            Err(error) if error.exited_with(1) => {}
            Err(error) => return Err(error.into()),
        }
        let mut commit = vec!["commit", "--quiet", "--message", message];
        // Naming the folder commits only its files, which git refuses while a merge is in
        // progress. The whole checkout needs no name, so a thread there can conclude a merge.
        let is_whole_checkout = resolved(folder)
            .await
            .is_ok_and(|folder| folder == repository.top);
        if !is_whole_checkout {
            commit.extend(["--", "."]);
        }
        self.git.run(folder, &commit, git::QUICK).await?;
        Ok(self
            .git
            .run(folder, &["rev-parse", "HEAD"], git::QUICK)
            .await?)
    }

    /// Stages everything in `folder` once no thread may be editing it. A turn that starts while git
    /// reads the files may have half-written one, so then the index goes back to what it held and
    /// the commit is refused.
    async fn stage(
        &self,
        store: &StoreThread,
        folder: &Path,
        top: &Path,
    ) -> Result<(), PlaceError> {
        self.refuse_while_edited(store, folder, top).await?;
        let index = self.index_of(folder).await?;
        let saved = self.scratch.join(format!("saved-index-{}", Uuid::new_v4()));
        let had_index = match tokio::fs::copy(&index, &saved).await {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(source) => {
                return Err(PlaceError::Io {
                    path: index.display().to_string(),
                    source,
                });
            }
        };
        let add = ["add", "--all", "--", "."];
        let staged = match self.git.run(folder, &add, CHECKOUT).await {
            Ok(_) => self.refuse_while_edited(store, folder, top).await,
            Err(error) => Err(error.into()),
        };
        let restored = match (&staged, had_index) {
            (Ok(()), _) => Ok(()),
            (Err(_), true) => tokio::fs::copy(&saved, &index).await.map(drop),
            (Err(_), false) => tokio::fs::remove_file(&index).await,
        };
        let _ = tokio::fs::remove_file(&saved).await;
        restored.map_err(|source| PlaceError::Io {
            path: index.display().to_string(),
            source,
        })?;
        staged
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
