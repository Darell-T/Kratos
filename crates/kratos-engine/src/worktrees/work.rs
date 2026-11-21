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
use super::index_lock::{IndexLock, PreparedReplacement};
use super::{PlaceError, Worktrees, check_still_there, resolved};
use crate::store_thread::StoreThread;

/// The most patch text a diff returns, cut at a whole line. A client shows the rest by reading the
/// folder itself.
const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;

/// The number of the event that started each thread's last turn, when a commit looked.
type Seen = HashMap<ThreadId, u64>;

/// Per-entry index flags read from `git ls-files -v --stage -z` that `git update-index
/// --index-info` resets and Kratos must preserve.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EntryFlags {
    assume_unchanged: bool,
    skip_worktree: bool,
}

impl EntryFlags {
    fn from_tag(tag: &[u8]) -> EntryFlags {
        EntryFlags {
            assume_unchanged: !tag.is_empty() && tag.iter().all(u8::is_ascii_lowercase),
            skip_worktree: tag.eq_ignore_ascii_case(b"S"),
        }
    }

    fn merge(&mut self, other: EntryFlags) {
        self.assume_unchanged |= other.assume_unchanged;
        self.skip_worktree |= other.skip_worktree;
    }
}

/// One path's stage records and flags in an index.
#[derive(Debug, Default, PartialEq, Eq)]
struct PathEntry<'listed> {
    /// Each entry as `mode object stage`, the form `git update-index --index-info` reads back.
    stages: Vec<&'listed [u8]>,
    flags: EntryFlags,
}

/// Each path's entries in an index, keyed by repo-relative path bytes.
type Entries<'listed> = BTreeMap<&'listed [u8], PathEntry<'listed>>;

/// The updates to apply to the real index, together with flags to restore on updated entries.
struct ReconciledIndex<'listed> {
    info: Vec<u8>,
    assume_unchanged: Vec<&'listed [u8]>,
    skip_worktree: Vec<&'listed [u8]>,
}

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

/// Holds the worktree's `index.lock` across `git commit`, either plain or prepared to replace the
/// real index atomically once the commit lands.
enum CommitLock {
    Plain(IndexLock),
    Prepared(PreparedReplacement),
}

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
        // Read before the index, so a git command that changes both while Kratos copies the index
        // changes the tip too.
        let tip = self.tip(folder).await?;
        refuse_partial_during(tip.underway(), is_whole_checkout)?;
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
            self.commit_index(folder, &repository.top, &indexes, &tip, message)
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

    /// Commits what `indexes.staged` holds on top of `tip`, and returns the new commit. Kratos
    /// holds git's lock on the worktree's index from its look at the tip until the index holds the
    /// committed files, as git's own commit does, so no other git command commits, starts or ends
    /// a merge, or changes the index meanwhile.
    async fn commit_index(
        &self,
        folder: &Path,
        top: &Path,
        indexes: &Indexes,
        tip: &Tip,
        message: &str,
    ) -> Result<String, PlaceError> {
        self.refuse_unless_staged(folder, &indexes.staged, tip.has_merge())
            .await?;
        let index_lock = IndexLock::take(&indexes.own).await?;
        if self.tip(folder).await? != *tip {
            return Err(PlaceError::ChangedMeanwhile);
        }
        let is_updated = self.write_updated_index(folder, top, indexes).await?;
        let commit_lock = if is_updated {
            CommitLock::Prepared(index_lock.prepare_replacement(&indexes.updated).await?)
        } else {
            CommitLock::Plain(index_lock)
        };
        let commit = ["commit", "--quiet", "--message", message];
        self.git
            .run_in_index(folder, &commit, git::QUICK, &indexes.staged)
            .await?;
        let committed = self
            .git
            .run(folder, &["rev-parse", "HEAD"], git::QUICK)
            .await?;
        match commit_lock {
            CommitLock::Prepared(replacement) => {
                replacement.commit().await.map_err(|source| {
                    PlaceError::IndexNotUpdatedAfterCommit {
                        commit: committed.clone(),
                        path: indexes.own.display().to_string(),
                        source,
                    }
                })?;
            }
            CommitLock::Plain(lock) => drop(lock),
        }
        Ok(committed)
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

    /// Writes `indexes.updated`: the worktree's index as it is now, with each file of `folder` the
    /// commit takes set to what it commits, unless the user concurrently staged that path or a
    /// colliding ancestor/descendant path. Preserves per-entry index flags (`assume-unchanged` and
    /// `skip-worktree`) on entries updated from `indexes.own`. Returns whether the index changes at
    /// all.
    async fn write_updated_index(
        &self,
        folder: &Path,
        top: &Path,
        indexes: &Indexes,
    ) -> Result<bool, PlaceError> {
        let list = ["ls-files", "--full-name", "-v", "--stage", "-z", "--", "."];
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
        let reconciled = reconcile_index(&before, &now, &staged);
        if reconciled.info.is_empty() {
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
            .run_in_index_with_input(
                folder,
                &update,
                reconciled.info,
                git::QUICK,
                &indexes.updated,
            )
            .await?;
        for (flag, paths) in [
            ("--assume-unchanged", &reconciled.assume_unchanged),
            ("--skip-worktree", &reconciled.skip_worktree),
        ] {
            if paths.is_empty() {
                continue;
            }
            let input: Vec<u8> = paths
                .iter()
                .flat_map(|path| path.iter().copied().chain([0]))
                .collect();
            // `--full-name` paths are relative to `top`, and `update-index --stdin` resolves paths
            // relative to its working directory.
            self.git
                .run_in_index_with_input(
                    top,
                    &["update-index", flag, "-z", "--stdin"],
                    input,
                    git::QUICK,
                    &indexes.updated,
                )
                .await?;
        }
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

/// Each path's entries and flags in `git ls-files -v --stage -z` output.
fn entries(listed: &[u8]) -> Entries<'_> {
    let mut entries = Entries::new();
    for record in listed.split(|&byte| byte == 0) {
        let Some(tab) = record.iter().position(|&byte| byte == b'\t') else {
            continue;
        };
        let meta = &record[..tab];
        let Some(space) = meta.iter().position(|&byte| byte == b' ') else {
            continue;
        };
        let tag = &meta[..space];
        let stage = &meta[space + 1..];
        let entry = entries.entry(&record[tab + 1..]).or_default();
        entry.stages.push(stage);
        entry.flags.merge(EntryFlags::from_tag(tag));
    }
    entries
}

fn stages_of<'a>(entries: &'a Entries<'a>, path: &[u8]) -> Option<&'a [&'a [u8]]> {
    entries.get(path).map(|entry| entry.stages.as_slice())
}

/// Whether `now` contains any ancestor or descendant of `path` that changed since `before`, so
/// inserting `path` into `now` would evict concurrent user staging through a file/directory
/// collision.
fn has_concurrent_df_collision(path: &[u8], before: &Entries<'_>, now: &Entries<'_>) -> bool {
    for (index, &byte) in path.iter().enumerate() {
        if byte == b'/' {
            let ancestor = &path[..index];
            if now.contains_key(ancestor) && stages_of(now, ancestor) != stages_of(before, ancestor)
            {
                return true;
            }
        }
    }
    let mut prefix = Vec::with_capacity(path.len() + 1);
    prefix.extend_from_slice(path);
    prefix.push(b'/');
    for (&descendant, entry) in now.range(prefix.as_slice()..) {
        if !descendant.starts_with(&prefix) {
            break;
        }
        if Some(entry.stages.as_slice()) != stages_of(before, descendant) {
            return true;
        }
    }
    false
}

/// Builds `git update-index -z --index-info` input and flag restorations for every path of `before`
/// and `staged` that was not concurrently changed in `now` (either directly or through a
/// file/directory collision). Deletions are emitted before additions so non-concurrent file/folder
/// replacements apply cleanly.
fn reconcile_index<'a>(
    before: &Entries<'a>,
    now: &Entries<'a>,
    staged: &Entries<'a>,
) -> ReconciledIndex<'a> {
    let mut deletions = Vec::new();
    let mut additions = Vec::new();
    let mut assume_unchanged = Vec::new();
    let mut skip_worktree = Vec::new();
    let paths: BTreeSet<&[u8]> = before.keys().chain(staged.keys()).copied().collect();
    for path in paths {
        let current = stages_of(now, path);
        let target = stages_of(staged, path);
        if current != stages_of(before, path) || current == target {
            continue;
        }
        match (target, current) {
            (Some(target_stages), _) => {
                if has_concurrent_df_collision(path, before, now) {
                    continue;
                }
                for stage in target_stages {
                    additions.extend([*stage, b"\t", path, b"\0"].concat());
                }
                if let Some(current_entry) = now.get(path) {
                    if current_entry.flags.assume_unchanged {
                        assume_unchanged.push(path);
                    }
                    if current_entry.flags.skip_worktree {
                        skip_worktree.push(path);
                    }
                }
            }
            // Mode 0 tells git to drop the path, and the object is read but not used.
            (None, Some(current_stages)) => {
                let object_and_stage = current_stages[0].splitn(2, |&byte| byte == b' ').nth(1);
                deletions.extend(
                    [
                        b"0 ".as_slice(),
                        object_and_stage.unwrap_or_default(),
                        b"\t",
                        path,
                        b"\0",
                    ]
                    .concat(),
                );
            }
            (None, None) => {}
        }
    }
    let mut info = deletions;
    info.extend(additions);
    ReconciledIndex {
        info,
        assume_unchanged,
        skip_worktree,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(items: &[&str]) -> Vec<u8> {
        items.join("\0").into_bytes()
    }

    #[test]
    fn the_index_takes_the_committed_files_the_user_did_not_stage_again() {
        let before = listed(&[
            "H 100644 aaa 0\tkept.txt",
            "H 100644 bbb 0\tcommitted.txt",
            "H 100644 ccc 0\tdeleted.txt",
        ]);
        let now = listed(&[
            "H 100644 ddd 0\tkept.txt",
            "H 100644 bbb 0\tcommitted.txt",
            "H 100644 ccc 0\tdeleted.txt",
        ]);
        let staged = listed(&[
            "H 100644 eee 0\tkept.txt",
            "H 100644 fff 0\tcommitted.txt",
            "H 100644 ggg 0\tadded.txt",
        ]);

        let reconciled = reconcile_index(&entries(&before), &entries(&now), &entries(&staged));

        let expected = listed(&[
            "0 ccc 0\tdeleted.txt",
            "100644 ggg 0\tadded.txt",
            "100644 fff 0\tcommitted.txt",
            "",
        ]);
        assert_eq!(
            String::from_utf8_lossy(&reconciled.info),
            String::from_utf8_lossy(&expected)
        );
        assert!(reconciled.assume_unchanged.is_empty());
        assert!(reconciled.skip_worktree.is_empty());
    }

    #[test]
    fn concurrent_file_directory_collisions_skip_both_ancestor_and_descendant_overwrites() {
        let before = listed(&["H 100644 aaa 0\tcomponent/other.txt"]);
        let now = listed(&[
            "H 100644 aaa 0\tcomponent/other.txt",
            "H 100644 uuu 0\tcomponent/newtree",
            "H 100644 vvv 0\tcomponent/nested/child",
        ]);
        let staged = listed(&[
            "H 100644 bbb 0\tcomponent/other.txt",
            "H 100644 sss 0\tcomponent/newtree/child",
            "H 100644 ttt 0\tcomponent/nested",
        ]);

        let reconciled = reconcile_index(&entries(&before), &entries(&now), &entries(&staged));

        let expected = listed(&["100644 bbb 0\tcomponent/other.txt", ""]);
        assert_eq!(
            String::from_utf8_lossy(&reconciled.info),
            String::from_utf8_lossy(&expected)
        );
    }

    #[test]
    fn non_concurrent_file_to_directory_replacement_deletes_before_adding() {
        let before = listed(&["H 100644 aaa 0\tcomponent/item"]);
        let now = listed(&["H 100644 aaa 0\tcomponent/item"]);
        let staged = listed(&["H 100644 bbb 0\tcomponent/item/child"]);

        let reconciled = reconcile_index(&entries(&before), &entries(&now), &entries(&staged));

        let expected = listed(&[
            "0 aaa 0\tcomponent/item",
            "100644 bbb 0\tcomponent/item/child",
            "",
        ]);
        assert_eq!(
            String::from_utf8_lossy(&reconciled.info),
            String::from_utf8_lossy(&expected)
        );
    }

    #[test]
    fn index_flags_on_updated_entries_are_preserved() {
        let before = listed(&[
            "H 100644 aaa 0\tassumed.txt",
            "S 100644 bbb 0\tsparse.txt",
            "H 100644 ccc 0\tboth.txt",
            "h 100644 ddd 0\tdeleted.txt",
        ]);
        let now = listed(&[
            "h 100644 aaa 0\tassumed.txt",
            "S 100644 bbb 0\tsparse.txt",
            "s 100644 ccc 0\tboth.txt",
            "h 100644 ddd 0\tdeleted.txt",
        ]);
        let staged = listed(&[
            "H 100644 111 0\tassumed.txt",
            "H 100644 222 0\tsparse.txt",
            "H 100644 333 0\tboth.txt",
        ]);

        let reconciled = reconcile_index(&entries(&before), &entries(&now), &entries(&staged));

        assert_eq!(
            reconciled.assume_unchanged,
            [&b"assumed.txt"[..], b"both.txt"]
        );
        assert_eq!(reconciled.skip_worktree, [&b"both.txt"[..], b"sparse.txt"]);
    }
}
