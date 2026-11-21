use serde::{Deserialize, Serialize};

use crate::RequiredText;

string_id! {
    /// A worktree's id, issued by the engine when it first records the worktree.
    #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    WorktreeId
}

/// Where a new thread works. Without one, it works in its project folder (D26).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum PlaceRequest {
    /// The project folder itself, so the agent edits the user's real files.
    #[default]
    ProjectFolder,
    /// A git worktree Kratos makes on a new branch named after the thread.
    NewWorktree,
    /// A worktree of the project's repository that already exists. Kratos borrows it and never
    /// deletes it.
    ExistingWorktree { path: RequiredText },
}

/// Where a thread works.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Place {
    ProjectFolder,
    Worktree(Worktree),
}

/// A git worktree one or more threads work in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Worktree {
    pub id: WorktreeId,
    /// The worktree's top folder, with links resolved.
    pub path: String,
    /// `None` when the worktree has no branch checked out.
    pub branch: Option<String>,
    pub ownership: Ownership,
    /// The commit Kratos made the worktree from. `None` for a borrowed worktree.
    pub base: Option<String>,
}

/// A worktree of a project's repository, from `GET /api/workspaces/{id}/worktrees`. A new thread
/// may work in it with [`PlaceRequest::ExistingWorktree`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectWorktree {
    /// The worktree's top folder, with links resolved.
    pub path: String,
    /// `None` when the worktree has no branch checked out.
    pub branch: Option<String>,
    /// [`Ownership::CreatedByKratos`] for a worktree Kratos made, which it removes once its last
    /// thread is archived. Any other is borrowed, and Kratos never removes it.
    pub ownership: Ownership,
}

/// Whether Kratos may delete a worktree once no thread uses it (D16).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Ownership {
    /// Kratos made it, so it may remove it under D16's rules.
    CreatedByKratos,
    /// It existed before Kratos used it, so Kratos never removes it.
    Borrowed,
}

/// `GET /api/threads/{id}/diff`: everything the thread's folder holds that its base commit does
/// not, untracked files included and files git ignores left out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDiff {
    /// The worktree's base for a worktree Kratos made, else the folder's `HEAD`.
    pub base: String,
    /// The patch `git diff` prints, cut at a size limit when `truncated`.
    pub patch: String,
    pub truncated: bool,
}

/// `POST /api/threads/{id}/commit`: commits everything in the thread's folder as the user's git
/// identity, with this message and nothing added to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommitThread {
    pub message: RequiredText,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Committed {
    pub commit: String,
}

/// `POST /api/threads/{id}/pull-request`: pushes the thread's branch to `origin` and opens a pull
/// request for it with the user's own `gh`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreatePullRequest {
    pub title: RequiredText,
    #[serde(default)]
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub url: String,
}

/// `POST /api/threads/{id}/archive`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArchiveThread {
    #[serde(default)]
    pub files: LeftFiles,
}

/// What happens to files a worktree removal would delete: changed and untracked files, and files
/// git ignores, such as `node_modules` (D16).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LeftFiles {
    /// Keep the worktree when it holds any.
    #[default]
    Keep,
    /// Remove the worktree and the files with it.
    Discard,
}

/// The answer to an archive. The thread is archived whatever happened to its worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedThread {
    pub cleanup: Cleanup,
}

/// What archiving did with the thread's folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Cleanup {
    /// The thread worked in its project folder or a borrowed worktree, which Kratos never removes.
    NotOwned,
    /// Another thread still works in the worktree.
    InUse,
    /// Kratos removed the worktree. Its branch stays.
    Removed,
    /// Kratos kept the worktree because removing it would delete these files, each as
    /// `git status --porcelain` prints it. Archive again with [`LeftFiles::Discard`] to remove it.
    Kept { files: Vec<String> },
    /// Kratos kept the worktree because its `HEAD` is `commit`, which no branch or other ref
    /// holds, so removing the worktree would lose it. Create a branch at it, then archive again.
    /// Discarding files does not remove it.
    UnsavedCommit { commit: String },
    /// Git could not remove the worktree, for example because a program holds a file in it. No
    /// thread may start there until an archive of one of its threads removes it.
    Blocked { error: String },
}
