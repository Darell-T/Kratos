//! A thread's finished work (D27, D55): its diff, a commit of it, and a pull request for it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use kratos_protocol::{ThreadDiff, ThreadId};
use kratos_store::ProviderThread;
use tokio::process::Command;
use uuid::Uuid;

use super::git::{self, CHECKOUT, Capped};
use super::index_lock::IndexLock;
use super::{PlaceError, Worktrees, check_still_there, resolved};
use crate::store_thread::StoreThread;

/// The most patch text a diff returns, cut at a whole line. A client shows the rest by reading the
/// folder itself.
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;

/// The number of the event that started each thread's last turn, when a commit looked.
type Seen = HashMap<ThreadId, u64>;

/// Each path's entries in an index, as `mode object stage`, the form `git update-index
/// --index-info` reads back.
type Entries<'listed> = BTreeMap<&'listed [u8], Vec<&'listed [u8]>>;

/// The index files of one commit: the worktree's own, and scratch files Kratos makes.
struct Indexes {
    own: PathBuf,
    /// A copy of `own` from before the commit staged anything.
    before: PathBuf,
    /// What the commit takes: a copy of `own`, staged into.
    staged: PathBuf,
    /// `own` with the committed files, which becomes the worktree's index.
    updated: PathBuf,
}

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
        let own_index = self.index_of(folder).await?;
        self.scratch_copy(&own_index).await
    }

    /// A scratch copy of the index file `index`, or a name for one when `index` does not exist.
    async fn scratch_copy(&self, index: &Path) -> Result<PathBuf, PlaceError> {
        let copy = self.scratch_name();
        match tokio::fs::copy(index, &copy).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(PlaceError::Io {
                path: copy.display().to_string(),
                source: error,
            }),
            _ => Ok(copy),
        }
    }

    fn scratch_name(&self) -> PathBuf {
        self.scratch.join(format!("index-{}", Uuid::new_v4()))
    }

    /// Commits everything in `folder` as the user's git identity, with `message` and nothing
    /// added to it, and returns the new commit. A project can be a folder inside its repository,
    /// so changes outside `folder`, staged or not, stay as they were. Refused while a thread may
    /// be editing what the commit would take.
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
        let is_whole_checkout = resolved(folder)
            .await
            .is_ok_and(|folder| folder == repository.top);
        let underway = self.underway(folder).await?;
        refuse_partial_during(underway, is_whole_checkout)?;
        // Read before the index, so a commit that lands while Kratos copies it moves HEAD on.
        let head = self.head(folder).await?;
        let indexes = self.indexes_of(folder).await?;
        // Staged once the lock is held, since the commit may have waited for it behind another
        // git command of the repository while a turn started.
        let committed = async {
            self.stage(
                store,
                folder,
                &repository.top,
                is_whole_checkout,
                &indexes.staged,
            )
            .await?;
            // Checked again just before git writes, as another checkout may have taken the
            // worktree's path while the commit waited or staged.
            check_still_there(store, &thread.place).await?;
            self.commit_index(folder, &indexes, head.as_deref(), message, underway)
                .await
        }
        .await;
        for scratch in [&indexes.before, &indexes.staged, &indexes.updated] {
            let _ = tokio::fs::remove_file(scratch).await;
        }
        committed
    }

    /// The index files of a commit in `folder`'s worktree, with the scratch copies made.
    async fn indexes_of(&self, folder: &Path) -> Result<Indexes, PlaceError> {
        let own = self.index_of(folder).await?;
        let before = self.scratch_copy(&own).await?;
        Ok(Indexes {
            staged: self.scratch_copy(&before).await?,
            updated: self.scratch_name(),
            own,
            before,
        })
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

    /// Commits what `indexes.staged` holds on top of `head`, and returns the new commit. Kratos
    /// holds git's lock on the worktree's index from its look at HEAD until the index holds the
    /// committed files, as git's own commit does, so no other commit lands and the index changes
    /// only here.
    async fn commit_index(
        &self,
        folder: &Path,
        indexes: &Indexes,
        head: Option<&str>,
        message: &str,
        underway: Option<Underway>,
    ) -> Result<String, PlaceError> {
        self.refuse_unless_staged(folder, &indexes.staged, underway)
            .await?;
        let index_lock = IndexLock::take(&indexes.own).await?;
        if self.head(folder).await?.as_deref() != head {
            return Err(PlaceError::HeadMoved);
        }
        let is_updated = self.write_updated_index(folder, indexes).await?;
        let commit = ["commit", "--quiet", "--message", message];
        self.git
            .run_in_index(folder, &commit, git::QUICK, &indexes.staged)
            .await?;
        let committed = self
            .git
            .run(folder, &["rev-parse", "HEAD"], git::QUICK)
            .await;
        if is_updated && let Err(error) = index_lock.replace_with(&indexes.updated).await {
            // The commit is made, so the caller hears of it rather than of this.
            eprintln!(
                "Kratos committed, but could not update the index at {}: {error}",
                indexes.own.display()
            );
        }
        Ok(committed?)
    }

    /// Refuses a commit of `staged` that would change no file, except to conclude a merge
    /// `underway`, which git records with both parents.
    async fn refuse_unless_staged(
        &self,
        folder: &Path,
        staged: &Path,
        underway: Option<Underway>,
    ) -> Result<(), PlaceError> {
        let args = ["diff", "--cached", "--quiet"];
        match self
            .git
            .run_in_index(folder, &args, git::QUICK, staged)
            .await
        {
            Ok(_) if matches!(underway, Some(Underway::Merge)) => Ok(()),
            Ok(_) => Err(PlaceError::NothingToCommit),
            Err(error) if error.exited_with(1) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Writes `indexes.updated`: the worktree's index as it is now, with each file of `folder` the
    /// commit takes set to what it commits, unless the user staged another version of the file
    /// since Kratos copied the index. Returns whether the index changes at all.
    async fn write_updated_index(
        &self,
        folder: &Path,
        indexes: &Indexes,
    ) -> Result<bool, PlaceError> {
        let list = ["ls-files", "--full-name", "--stage", "-z", "--", "."];
        let mut listed = Vec::new();
        for index in [&indexes.before, &indexes.own, &indexes.staged] {
            listed.push(
                self.git
                    .run_bytes_in_index(folder, &list, git::QUICK, index)
                    .await?,
            );
        }
        let [before, now, staged] =
            [&listed[0], &listed[1], &listed[2]].map(|listed| entries(listed));
        let info = index_info(&before, &now, &staged);
        if info.is_empty() {
            return Ok(false);
        }
        // A repository that never staged a file has no index yet, and git makes one.
        match tokio::fs::copy(&indexes.own, &indexes.updated).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(PlaceError::Io {
                    path: indexes.updated.display().to_string(),
                    source: error,
                });
            }
            _ => {}
        }
        let update = ["update-index", "-z", "--index-info"];
        self.git
            .run_in_index_with_input(folder, &update, info, git::QUICK, &indexes.updated)
            .await?;
        Ok(true)
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

    /// The commit `folder`'s worktree has checked out, or `None` on a branch with no commit yet.
    async fn head(&self, folder: &Path) -> Result<Option<String>, PlaceError> {
        let args = ["rev-parse", "--verify", "--quiet", "HEAD"];
        match self.git.run(folder, &args, git::QUICK).await {
            Ok(head) => Ok(Some(head)),
            Err(error) if error.exited_with(1) => Ok(None),
            Err(error) => Err(error.into()),
        }
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

/// Each path's entries in `git ls-files --stage -z` output.
fn entries(listed: &[u8]) -> Entries<'_> {
    let mut entries = Entries::new();
    for entry in listed.split(|&byte| byte == 0) {
        if let Some(tab) = entry.iter().position(|&byte| byte == b'\t') {
            entries
                .entry(&entry[tab + 1..])
                .or_default()
                .push(&entry[..tab]);
        }
    }
    entries
}

/// `git update-index -z --index-info` input that sets each path of `before` and `staged` to its
/// entries in `staged`, where the index holds `now` and still has the path as in `before`.
fn index_info(before: &Entries, now: &Entries, staged: &Entries) -> Vec<u8> {
    let mut info = Vec::new();
    let paths: BTreeSet<&[u8]> = before.keys().chain(staged.keys()).copied().collect();
    for path in paths {
        let current = now.get(path);
        if current != before.get(path) || current == staged.get(path) {
            continue;
        }
        let entries = match (staged.get(path), current) {
            (Some(entries), _) => entries.iter().map(|entry| entry.to_vec()).collect(),
            // Mode 0 tells git to drop the path, and the object is read but not used.
            (None, Some(current)) => {
                let object_and_stage = current[0].splitn(2, |&byte| byte == b' ').nth(1);
                vec![[b"0 ", object_and_stage.unwrap_or_default()].concat()]
            }
            (None, None) => Vec::new(),
        };
        for entry in entries {
            info.extend([&entry[..], b"\t", path, b"\0"].concat());
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_takes_the_committed_files_the_user_did_not_stage_again() {
        let listed = |entries: &[&str]| entries.join("\0").into_bytes();
        let before = listed(&[
            "100644 aaa 0\tkept.txt",
            "100644 bbb 0\tcommitted.txt",
            "100644 ccc 0\tdeleted.txt",
        ]);
        let now = listed(&[
            "100644 ddd 0\tkept.txt",
            "100644 bbb 0\tcommitted.txt",
            "100644 ccc 0\tdeleted.txt",
        ]);
        let staged = listed(&[
            "100644 eee 0\tkept.txt",
            "100644 fff 0\tcommitted.txt",
            "100644 ggg 0\tadded.txt",
        ]);

        let info = index_info(&entries(&before), &entries(&now), &entries(&staged));

        let expected = listed(&[
            "100644 ggg 0\tadded.txt",
            "100644 fff 0\tcommitted.txt",
            "0 ccc 0\tdeleted.txt",
            "",
        ]);
        assert_eq!(
            String::from_utf8_lossy(&info),
            String::from_utf8_lossy(&expected)
        );
    }
}
