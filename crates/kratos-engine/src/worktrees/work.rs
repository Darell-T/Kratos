//! A thread's finished work (D27, D55): its diff, a commit of it, and a pull request for it.

use std::io;
use std::path::Path;
use std::process::Stdio;

use kratos_protocol::ThreadDiff;
use tokio::process::Command;
use uuid::Uuid;

use super::git::{self, CHECKOUT, Capped};
use super::{PlaceError, Worktrees, resolved};

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
        let args = ["rev-parse", "--path-format=absolute", "--git-path", "index"];
        let own_index = self.git.run(folder, &args, git::QUICK).await?;
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

    /// Whether a turn running in one of the `running` folders may be editing files a commit of
    /// `folder` would take: one folder holds the other and both are in the same checkout. A
    /// worktree inside a project folder is a checkout of its own, so its turns do not count.
    pub(crate) async fn is_being_edited(
        &self,
        folder: &Path,
        running: &[String],
    ) -> Result<bool, PlaceError> {
        let overlapping: Vec<&Path> = running
            .iter()
            .map(Path::new)
            .filter(|other| other.starts_with(folder) || folder.starts_with(other))
            .collect();
        if overlapping.is_empty() {
            return Ok(false);
        }
        let checkout = self.locate(folder).await?.map(|found| found.top);
        for other in overlapping {
            if self.locate(other).await?.map(|found| found.top) == checkout {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Commits everything in `folder` as the user's git identity, with `message` and nothing
    /// added to it, and returns the new commit. A project can be a folder inside its repository,
    /// so changes outside `folder`, staged or not, stay as they were.
    pub(crate) async fn commit(&self, folder: &Path, message: &str) -> Result<String, PlaceError> {
        let repository = self
            .locate(folder)
            .await?
            .ok_or_else(|| PlaceError::NotARepository(folder.display().to_string()))?;
        let lock = self.lock_of(&repository.shared);
        let _one_at_a_time = lock.lock().await;
        self.git
            .run(folder, &["add", "--all", "--", "."], CHECKOUT)
            .await?;
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
        if self.default_branch(folder).await?.as_ref() == Some(&branch) {
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

    /// The branch `origin`'s `HEAD` names, which `gh` opens a pull request against, or `None` when
    /// the remote names none, as an empty one does.
    async fn default_branch(&self, folder: &Path) -> Result<Option<String>, PlaceError> {
        let args = ["ls-remote", "--symref", "origin", "HEAD"];
        let listed = self.git.run(folder, &args, CHECKOUT).await?;
        Ok(listed.lines().find_map(|line| {
            let branch = line
                .strip_prefix("ref: refs/heads/")?
                .strip_suffix("\tHEAD")?;
            Some(branch.to_owned())
        }))
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
