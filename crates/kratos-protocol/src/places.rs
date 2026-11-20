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
    /// Git could not remove the worktree, for example because a program holds a file in it. No
    /// thread may start there until an archive of one of its threads removes it.
    Blocked { error: String },
}
