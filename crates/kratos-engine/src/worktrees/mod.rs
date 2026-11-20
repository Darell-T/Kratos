//! Where a new thread works: its project folder, a worktree Kratos makes, or a worktree that
//! already exists. Kratos's git commands for one repository run one at a time, because git fails
//! instead of waiting when another git holds one of the repository's lock files. Two threads that
//! start together still get different names without the lock: `git branch` refuses a taken name,
//! and the store refuses a second record at one path.

mod git;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kratos_protocol::{PlaceRequest, WorktreeId};
use kratos_store::{Checkout, NewPlace, StoreError};

use crate::store_thread::StoreThread;
use git::{CHECKOUT, Git};

pub(crate) use git::GitError;

/// The folder in a project that holds the worktrees Kratos makes for it, one per branch.
pub const WORKTREES_FOLDER: &str = ".kratos-worktrees";

/// The most suffixed names, `-2` to `-100`, tried for a new branch before giving up.
const MAX_NAME_TRIES: u32 = 100;

/// The longest branch name made from a thread's title, before any `-2` suffix.
const MAX_NAME_CHARS: usize = 40;

pub struct Worktrees {
    git: Git,
    /// One lock per repository, keyed by its shared git folder.
    repositories: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

/// Why a thread cannot work where it asked. Every message says what to do in plain words
/// (Appendix I, L12).
#[derive(Debug, thiserror::Error)]
pub(crate) enum PlaceError {
    #[error("{0} is not an existing folder.")]
    NotAFolder(String),
    #[error(
        "{0} is not in a git repository, so it has no worktrees. Start the thread in the project \
         folder instead."
    )]
    NotARepository(String),
    #[error(
        "Worktrees need the project folder to be the top folder of its git repository, {top}, \
         but the project is {project}."
    )]
    ProjectNotAtTop { project: String, top: String },
    #[error(
        "The repository has no commits yet, so a new worktree has nothing to start from. Make a \
         first commit, then try again."
    )]
    NoCommits,
    #[error("{0} is not a git worktree.")]
    NotAWorktree(String),
    #[error("{path} is inside the worktree {top}. Name the worktree's top folder.")]
    InsideWorktree { path: String, top: String },
    #[error("{0} is a worktree of a different repository than the project.")]
    OtherRepository(String),
    #[error("{0} is the project folder. Start the thread in the project folder instead.")]
    IsProjectFolder(String),
    #[error("Every branch name from {0} to {0}-{MAX_NAME_TRIES} is taken. Rename the thread.")]
    NoFreeName(String),
    #[error(
        "{0} is a link to another folder, so Kratos will not make worktrees through it. Remove the          link, or start the thread in the project folder."
    )]
    LinkedFolder(String),
    #[error("Kratos cannot read what git answered to `git {command}`: {output}")]
    UnreadableGit { command: String, output: String },
    #[error("Kratos cannot use the path {0}, because it is not valid Unicode.")]
    NotUnicode(String),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Kratos could not prepare {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
}

/// A repository as git names it: its main worktree's top folder and the git folder all its
/// worktrees share.
struct Repository {
    top: PathBuf,
    shared: PathBuf,
}

impl Worktrees {
    /// Makes the empty folder in Kratos's data folder that git reads as the hooks folder, so no
    /// hook runs.
    ///
    /// # Errors
    /// Returns why the folder could not be made.
    pub fn open(kratos_home: &Path) -> io::Result<Worktrees> {
        let hooks_off = kratos_home.join("git-hooks-off");
        std::fs::create_dir_all(&hooks_off)?;
        Ok(Worktrees {
            git: Git::new(hooks_off),
            repositories: Mutex::default(),
        })
    }

    /// Checks the requested place against git, and makes the worktree when the request asks for a
    /// new one. `title` names a new worktree's branch.
    pub(crate) async fn prepare(
        &self,
        store: &StoreThread,
        project: &Path,
        request: &PlaceRequest,
        title: &str,
    ) -> Result<NewPlace, PlaceError> {
        match request {
            PlaceRequest::ProjectFolder => Ok(NewPlace::ProjectFolder),
            PlaceRequest::ExistingWorktree { path } => self
                .borrow(project, Path::new(path.as_str()))
                .await
                .map(NewPlace::Borrowed),
            PlaceRequest::NewWorktree => self.create(store, project, title).await,
        }
    }

    async fn borrow(&self, project: &Path, path: &Path) -> Result<Checkout, PlaceError> {
        let repository = self.repository_of(project).await?;
        let worktree = self.worktree_at(path).await?;
        let shown = worktree.top.display().to_string();
        if worktree.shared != repository.shared {
            return Err(PlaceError::OtherRepository(shown));
        }
        if worktree.top == repository.top {
            return Err(PlaceError::IsProjectFolder(shown));
        }
        Ok(Checkout {
            branch: self.branch_of(&worktree.top).await?,
            path: text(&worktree.top)?,
            repository: text(&repository.shared)?,
        })
    }

    /// The worktree whose top folder is `path`.
    async fn worktree_at(&self, path: &Path) -> Result<Repository, PlaceError> {
        let shown = path.display().to_string();
        let path = resolved(path)
            .await
            .map_err(|_| PlaceError::NotAFolder(shown.clone()))?;
        let worktree = self
            .locate(&path)
            .await?
            .ok_or(PlaceError::NotAWorktree(shown.clone()))?;
        if worktree.top != path {
            return Err(PlaceError::InsideWorktree {
                path: shown,
                top: worktree.top.display().to_string(),
            });
        }
        Ok(worktree)
    }

    async fn create(
        &self,
        store: &StoreThread,
        project: &Path,
        title: &str,
    ) -> Result<NewPlace, PlaceError> {
        let repository = self.repository_of(project).await?;
        let lock = self.lock_of(&repository.shared);
        let _one_at_a_time = lock.lock().await;
        let base = self.head_commit(&repository.top).await?;
        let folder = repository.top.join(WORKTREES_FOLDER);
        ignore_everything_in(&folder).await?;
        let stem = branch_stem(title);
        for name in candidate_names(&stem) {
            let path = folder.join(&name);
            let made = self.make(store, &repository, &path, &name, &base).await?;
            if let Some(id) = made {
                return Ok(NewPlace::Created(id));
            }
        }
        Err(PlaceError::NoFreeName(stem))
    }

    async fn head_commit(&self, top: &Path) -> Result<String, PlaceError> {
        let args = ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"];
        self.git.run(top, &args, git::QUICK).await.map_err(|error| {
            if error.exited_with(1) {
                PlaceError::NoCommits
            } else {
                PlaceError::Git(error)
            }
        })
    }

    /// Records and makes the worktree at `path` on new branch `name`. `Ok(None)` when the path or
    /// the name is taken, so the caller tries the next name.
    async fn make(
        &self,
        store: &StoreThread,
        repository: &Repository,
        path: &Path,
        name: &str,
        base: &str,
    ) -> Result<Option<WorktreeId>, PlaceError> {
        if tokio::fs::symlink_metadata(path).await.is_ok() {
            return Ok(None);
        }
        let checkout = Checkout {
            path: text(path)?,
            repository: text(&repository.shared)?,
            branch: Some(name.to_owned()),
        };
        let id = match store
            .call(move |store| store.reserve_worktree(&checkout))
            .await
        {
            Err(StoreError::WorktreeTaken(_)) => return Ok(None),
            reserved => reserved?,
        };
        let made = self.check_out(&repository.top, path, name, base).await;
        if let Ok(true) = made {
            return Ok(Some(id));
        }
        // A checkout stopped partway leaves a folder, which keeps its reservation so a later
        // start knows the folder is Kratos's.
        if tokio::fs::symlink_metadata(path).await.is_err() {
            store
                .call(move |store| store.release_reservation(&id))
                .await?;
        }
        made.map(|_| None)
    }

    /// Makes branch `name` at `base` and checks it out at `path`. `Ok(false)` when the branch name
    /// is taken, so the caller tries the next name.
    async fn check_out(
        &self,
        top: &Path,
        path: &Path,
        name: &str,
        base: &str,
    ) -> Result<bool, PlaceError> {
        if let Err(error) = self
            .git
            .run(top, &["branch", "--", name, base], git::QUICK)
            .await
        {
            // The pattern matches the branch itself and any branch in a folder of that name, such
            // as `fix/login` for `fix`, which git also refuses.
            let reference = format!("refs/heads/{name}");
            let args = [
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                &reference,
            ];
            let found = self.git.run(top, &args, git::QUICK).await?;
            return if found.is_empty() {
                Err(error.into())
            } else {
                Ok(false)
            };
        }
        let path_text = text(path)?;
        let added = self
            .git
            .run(top, &["worktree", "add", "--", &path_text, name], CHECKOUT)
            .await;
        if let Err(error) = added {
            // The branch is Kratos's own and still at `base`, so nothing of the user's is lost.
            let _ = self
                .git
                .run(top, &["branch", "-D", "--", name], git::QUICK)
                .await;
            return Err(error.into());
        }
        Ok(true)
    }

    /// The repository whose worktree the project folder is, which must be its top folder.
    async fn repository_of(&self, project: &Path) -> Result<Repository, PlaceError> {
        let shown = project.display().to_string();
        let project = resolved(project)
            .await
            .map_err(|_| PlaceError::NotAFolder(shown.clone()))?;
        let found = self
            .locate(&project)
            .await?
            .ok_or_else(|| PlaceError::NotARepository(shown.clone()))?;
        if found.top != project {
            return Err(PlaceError::ProjectNotAtTop {
                project: shown,
                top: found.top.display().to_string(),
            });
        }
        Ok(found)
    }

    /// The worktree `folder` is in, or `None` when it is in no git repository.
    async fn locate(&self, folder: &Path) -> Result<Option<Repository>, PlaceError> {
        let args = [
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ];
        let shown = match self.git.run(folder, &args, git::QUICK).await {
            Ok(shown) => shown,
            Err(error) if error.exited_with(128) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut lines = shown.lines();
        let (Some(top), Some(shared), None) = (lines.next(), lines.next(), lines.next()) else {
            return Err(PlaceError::UnreadableGit {
                command: args.join(" "),
                output: shown,
            });
        };
        Ok(Some(Repository {
            top: resolved_from_git(top).await?,
            shared: resolved_from_git(shared).await?,
        }))
    }

    async fn branch_of(&self, worktree: &Path) -> Result<Option<String>, PlaceError> {
        let args = ["symbolic-ref", "--short", "--quiet", "HEAD"];
        match self.git.run(worktree, &args, git::QUICK).await {
            Ok(branch) => Ok(Some(branch)),
            Err(error) if error.exited_with(1) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn lock_of(&self, repository: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut repositories = self.repositories.lock().expect("lock poisoned");
        repositories
            .entry(repository.to_owned())
            .or_default()
            .clone()
    }
}

/// Makes `folder` with a `.gitignore` that ignores everything in it, itself included, so the
/// worktrees inside never show as changes in the project and no project file is edited.
async fn ignore_everything_in(folder: &Path) -> Result<(), PlaceError> {
    let io_error = |source| PlaceError::Io {
        path: folder.display().to_string(),
        source,
    };
    if tokio::fs::symlink_metadata(folder).await.is_ok() {
        // A repository can commit the folder as a link, and git would check out through it.
        if resolved(folder).await.ok().as_deref() != Some(folder) {
            return Err(PlaceError::LinkedFolder(folder.display().to_string()));
        }
    } else {
        tokio::fs::create_dir(folder).await.map_err(io_error)?;
    }
    let ignore = folder.join(".gitignore");
    if tokio::fs::symlink_metadata(&ignore).await.is_err() {
        tokio::fs::write(&ignore, "*\n").await.map_err(io_error)?;
    }
    Ok(())
}

/// The branch name a thread titled `title` starts from: lowercase ASCII letters and digits, with
/// a dash for each run of anything else, such as `Fix the login bug!` to `fix-the-login-bug`.
fn branch_stem(title: &str) -> String {
    let mut stem = String::new();
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            stem.push(character.to_ascii_lowercase());
        } else if !stem.is_empty() && !stem.ends_with('-') {
            stem.push('-');
        }
        if stem.len() == MAX_NAME_CHARS {
            break;
        }
    }
    let stem = stem.trim_end_matches('-');
    if stem.is_empty() {
        "thread".to_owned()
    } else {
        stem.to_owned()
    }
}

/// `stem`, then `stem-2` to `stem-100`.
fn candidate_names(stem: &str) -> impl Iterator<Item = String> {
    std::iter::once(stem.to_owned())
        .chain((2..=MAX_NAME_TRIES).map(move |suffix| format!("{stem}-{suffix}")))
}

/// `path` with links resolved, and on Windows without the `\\?\` prefix that git and agents do not
/// expect on an ordinary drive path.
async fn resolved(path: &Path) -> io::Result<PathBuf> {
    let path = tokio::fs::canonicalize(path).await?;
    if !tokio::fs::metadata(&path).await?.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a folder"));
    }
    Ok(without_verbatim_prefix(path))
}

async fn resolved_from_git(path: &str) -> Result<PathBuf, PlaceError> {
    resolved(Path::new(path))
        .await
        .map_err(|source| PlaceError::Io {
            path: path.to_owned(),
            source,
        })
}

fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

fn text(path: &Path) -> Result<String, PlaceError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| PlaceError::NotUnicode(path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_becomes_a_short_lowercase_branch_name() {
        assert_eq!(branch_stem("Fix the login bug!"), "fix-the-login-bug");
        assert_eq!(branch_stem("  --Ünïcode only--  "), "n-code-only");
        assert_eq!(branch_stem("日本語"), "thread");
        let long = branch_stem(&"word ".repeat(20));
        assert!(
            long.len() <= MAX_NAME_CHARS && !long.ends_with('-'),
            "{long}"
        );
    }
}
