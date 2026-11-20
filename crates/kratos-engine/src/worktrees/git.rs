use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// How long an ordinary git command may take, such as reading a branch name.
pub(super) const QUICK: Duration = Duration::from_secs(30);

/// How long `git worktree add` may take. Checking out a large repository writes every tracked
/// file, and users saw removals of big worktrees time out (Appendix I, L12).
pub(super) const CHECKOUT: Duration = Duration::from_secs(600);

/// Runs Kratos's own git commands with the repository's hooks turned off, so a hook a repository
/// ships never runs outside every sandbox when Kratos makes a worktree or a branch.
#[derive(Debug)]
pub(super) struct Git {
    hooks_off: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GitError {
    #[error("Kratos could not run git. Install git and put it on PATH. ({0})")]
    Missing(std::io::Error),
    #[error("`git {command}` failed: {stderr}")]
    Failed {
        command: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("`git {command}` took longer than {} seconds, so Kratos stopped it", .budget.as_secs())]
    TimedOut { command: String, budget: Duration },
}

impl GitError {
    /// Whether git ran and exited with `code`.
    pub(super) fn exited_with(&self, wanted: i32) -> bool {
        matches!(self, GitError::Failed { code: Some(code), .. } if *code == wanted)
    }
}

impl Git {
    /// `hooks_off` is an empty folder that git reads as the hooks folder.
    pub(super) fn new(hooks_off: PathBuf) -> Git {
        Git { hooks_off }
    }

    /// Runs `git -C folder args…` and returns its standard output without the final newline.
    pub(super) async fn run(
        &self,
        folder: &Path,
        args: &[&str],
        budget: Duration,
    ) -> Result<String, GitError> {
        self.run_with(folder, args, budget, None).await
    }

    /// [`Git::run`] with `index` as git's index file instead of the worktree's own, so staging
    /// there leaves the user's staged changes as they were.
    pub(super) async fn run_with_index(
        &self,
        folder: &Path,
        args: &[&str],
        budget: Duration,
        index: &Path,
    ) -> Result<String, GitError> {
        self.run_with(folder, args, budget, Some(index)).await
    }

    async fn run_with(
        &self,
        folder: &Path,
        args: &[&str],
        budget: Duration,
        index: Option<&Path>,
    ) -> Result<String, GitError> {
        let command = args.join(" ");
        let mut git = Command::new("git");
        if let Some(index) = index {
            git.env("GIT_INDEX_FILE", index);
        }
        git.arg("-c")
            .arg(format!("core.hooksPath={}", self.hooks_off.display()))
            .arg("-C")
            .arg(folder)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = match tokio::time::timeout(budget, git.output()).await {
            Err(_) => return Err(GitError::TimedOut { command, budget }),
            Ok(Err(error)) => return Err(GitError::Missing(error)),
            Ok(Ok(output)) => output,
        };
        if !output.status.success() {
            return Err(GitError::Failed {
                command,
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.trim_end_matches(['\r', '\n']).to_owned())
    }
}
