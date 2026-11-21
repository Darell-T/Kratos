use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

/// How long an ordinary git command may take, such as reading a branch name.
pub(super) const QUICK: Duration = Duration::from_secs(30);

/// How long `git worktree add` may take. Checking out a large repository writes every tracked
/// file, and users saw removals of big worktrees time out (Appendix I, L12).
pub(super) const CHECKOUT: Duration = Duration::from_secs(600);

/// The most of git's error output kept for a message. The rest is read and dropped, so git never
/// waits on a full pipe.
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Runs Kratos's own git commands with the repository's hooks turned off, so a hook a repository
/// ships never runs outside every sandbox when Kratos makes a worktree or a branch.
#[derive(Debug)]
pub(super) struct Git {
    hooks_off: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GitError {
    #[error("Kratos could not run git. Install git and put it on PATH. ({0})")]
    Missing(io::Error),
    #[error("Kratos could not read what git wrote: {0}")]
    Read(io::Error),
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

/// What a command wrote, up to a limit.
pub(super) struct Capped {
    pub text: String,
    /// Whether git wrote more than the limit, so `text` ends at the last whole line within it.
    pub is_cut: bool,
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
        self.output(folder, args, budget, None).await
    }

    /// [`Git::run`] with `index` as git's index file instead of the worktree's own, so staging
    /// there leaves the user's staged changes as they were.
    pub(super) async fn run_in_index(
        &self,
        folder: &Path,
        args: &[&str],
        budget: Duration,
        index: &Path,
    ) -> Result<String, GitError> {
        self.output(folder, args, budget, Some(index)).await
    }

    async fn output(
        &self,
        folder: &Path,
        args: &[&str],
        budget: Duration,
        index: Option<&Path>,
    ) -> Result<String, GitError> {
        let command = args.join(" ");
        let mut git = self.command(folder, args, index);
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

    /// [`Git::run_in_index`] that keeps at most `max_bytes` of standard output, as git wrote it,
    /// and stops git once it writes more.
    pub(super) async fn read_in_index(
        &self,
        folder: &Path,
        args: &[&str],
        budget: Duration,
        index: &Path,
        max_bytes: usize,
    ) -> Result<Capped, GitError> {
        let command = args.join(" ");
        let mut git = self.command(folder, args, Some(index));
        git.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = git.spawn().map_err(GitError::Missing)?;
        let read = match tokio::time::timeout(budget, read_child(child, max_bytes)).await {
            Err(_) => return Err(GitError::TimedOut { command, budget }),
            Ok(Err(error)) => return Err(GitError::Read(error)),
            Ok(Ok(read)) => read,
        };
        if !read.is_cut && !read.status.success() {
            return Err(GitError::Failed {
                command,
                code: read.status.code(),
                stderr: String::from_utf8_lossy(&read.stderr).trim().to_owned(),
            });
        }
        Ok(capped_text(&read.stdout, read.is_cut, max_bytes))
    }

    fn command(&self, folder: &Path, args: &[&str], index: Option<&Path>) -> Command {
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
        git
    }
}

/// What a child wrote and how it ended.
struct ChildOutput {
    stdout: Vec<u8>,
    /// Whether the child wrote more than the limit, so it was stopped.
    is_cut: bool,
    stderr: Vec<u8>,
    status: ExitStatus,
}

/// Reads `child`'s output, keeping at most `max_bytes` of standard output and stopping the child
/// once it writes more. Standard error is read at the same time, so the child never waits on a
/// full pipe.
async fn read_child(mut child: Child, max_bytes: usize) -> io::Result<ChildOutput> {
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let out = async {
        let mut stdout = stdout;
        let read = read_capped(&mut stdout, max_bytes).await?;
        // The kill reaches only the child. Closing the pipe also stops a process it started that
        // writes there, such as the git that Git for Windows' launcher runs, which would otherwise
        // hold standard error open while it waits on a pipe nobody reads.
        drop(stdout);
        if read.1 {
            child.start_kill()?;
        }
        io::Result::Ok(read)
    };
    let err = async {
        let (kept, is_cut) = read_capped(&mut stderr, MAX_STDERR_BYTES).await?;
        if is_cut {
            tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
        }
        io::Result::Ok(kept)
    };
    let ((stdout, is_cut), stderr) = tokio::try_join!(out, err)?;
    let status = child.wait().await?;
    Ok(ChildOutput {
        stdout,
        is_cut,
        stderr,
        status,
    })
}

/// `bytes` as text of at most `max_bytes`, which ends at the last whole line within the limit when
/// git wrote more (`is_cut`) or when decoding made it longer: each byte that is not UTF-8 becomes
/// a three-byte replacement character.
fn capped_text(bytes: &[u8], is_cut: bool, max_bytes: usize) -> Capped {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    let is_cut = is_cut || text.len() > max_bytes;
    if is_cut {
        let within = &text.as_bytes()[..text.len().min(max_bytes)];
        let whole_lines = within
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |end| end + 1);
        text.truncate(whole_lines);
    }
    Capped { text, is_cut }
}

/// Reads `reader` until it ends or has given more than `max_bytes`, and keeps at most `max_bytes`
/// of it. Returns what it kept and whether there was more.
async fn read_capped(
    reader: &mut (impl AsyncRead + Unpin),
    max_bytes: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok((kept, false));
        }
        let room = max_bytes - kept.len();
        if read > room {
            kept.extend_from_slice(&chunk[..room]);
            return Ok((kept, true));
        }
        kept.extend_from_slice(&chunk[..read]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reading_stops_at_the_limit_however_much_more_there_is() {
        // Catches a reader that buffers all of git's output before applying the limit: on an
        // endless stream it never returns.
        let mut endless = tokio::io::repeat(b'x');
        let read = tokio::time::timeout(Duration::from_secs(10), read_capped(&mut endless, 1000))
            .await
            .expect("reading an endless stream stops at the limit")
            .unwrap();

        assert_eq!((read.0.len(), read.1), (1000, true));
    }

    #[tokio::test]
    async fn a_child_started_through_a_launcher_still_stops_at_the_limit() {
        // Catches stopping only the launcher, as Git for Windows' `cmd\git.exe` is: the git it
        // started keeps the pipes open, blocked on output nobody reads, until the budget ends.
        let repository = tempfile::TempDir::new().unwrap();
        let folder = repository.path();
        std::fs::write(folder.join("big.txt"), "x".repeat(1024 * 1024)).unwrap();
        let identity = ["-c", "user.name=Test", "-c", "user.email=test@example.com"];
        for args in [
            &["init", "--quiet"][..],
            &["add", "big.txt"],
            &[&identity[..], &["commit", "--quiet", "--message", "big"]].concat(),
        ] {
            let mut setup = std::process::Command::new("git");
            setup.arg("-C").arg(folder).args(args);
            assert!(setup.status().unwrap().success(), "git {args:?}");
        }
        let mut launcher = launcher_of(folder);
        launcher.stdout(Stdio::piped()).stderr(Stdio::piped());

        let child = launcher.spawn().unwrap();
        let read = tokio::time::timeout(Duration::from_secs(10), read_child(child, 1000))
            .await
            .expect("reading stops once the launcher's git wrote past the limit")
            .unwrap();

        assert_eq!((read.stdout.len(), read.is_cut), (1000, true));
    }

    /// A shell that runs git as its own child and waits for it, as a launcher does.
    fn launcher_of(folder: &Path) -> Command {
        if cfg!(windows) {
            let mut shell = Command::new("cmd");
            shell
                .args(["/C", "git", "-C"])
                .arg(folder)
                .args(["cat-file", "blob", "HEAD:big.txt"]);
            shell
        } else {
            let mut shell = Command::new("sh");
            // The `:` after git keeps the shell from replacing itself with git.
            shell
                .args(["-c", "git -C \"$0\" cat-file blob HEAD:big.txt; :"])
                .arg(folder);
            shell
        }
    }

    #[tokio::test]
    async fn output_at_the_limit_is_kept_whole() {
        let mut exact: &[u8] = &[b'x'; 1000];
        let read = read_capped(&mut exact, 1000).await.unwrap();

        assert_eq!((read.0.len(), read.1), (1000, false));
    }
}
