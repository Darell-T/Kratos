//! A thread's finished work (D27, D55): its diff, a commit of it, and a pull request for it.

use std::io;
use std::path::Path;
use std::process::Stdio;

use kratos_protocol::ThreadDiff;
use tokio::process::Command;
use uuid::Uuid;

use super::git::{self, CHECKOUT, Capped};
use super::{PlaceError, Worktrees};

/// The most patch text a diff returns, cut at a whole line. A client shows the rest by reading the
/// folder itself.
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;

impl Worktrees {
    /// What `folder` holds that `base` does not, or that its `HEAD` does not without a base. The
    /// files are staged in a scratch index, so new files show and the user's staged changes stay
    /// as they were.
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
            .run_in_index(folder, &["add", "--all"], CHECKOUT, index)
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
        ];
        Ok(self
            .git
            .read_in_index(folder, &args, git::QUICK, index, MAX_PATCH_BYTES)
            .await?)
    }

    /// Commits everything in `folder` as the user's git identity, with `message` and nothing
    /// added to it, and returns the new commit.
    pub(crate) async fn commit(&self, folder: &Path, message: &str) -> Result<String, PlaceError> {
        self.git.run(folder, &["add", "--all"], CHECKOUT).await?;
        let staged = ["diff", "--cached", "--quiet"];
        match self.git.run(folder, &staged, git::QUICK).await {
            Ok(_) => return Err(PlaceError::NothingToCommit),
            Err(error) if error.exited_with(1) => {}
            Err(error) => return Err(error.into()),
        }
        let commit = ["commit", "--quiet", "--message", message];
        self.git.run(folder, &commit, git::QUICK).await?;
        Ok(self
            .git
            .run(folder, &["rev-parse", "HEAD"], git::QUICK)
            .await?)
    }

    /// Pushes the branch `folder` has checked out to `origin`, then opens a pull request for it
    /// with the user's own `gh` and returns its address.
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
