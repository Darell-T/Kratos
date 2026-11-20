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

/// Whether Kratos may delete a worktree once no thread uses it (D16).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Ownership {
    /// Kratos made it, so it may remove it under D16's rules.
    CreatedByKratos,
    /// It existed before Kratos used it, so Kratos never removes it.
    Borrowed,
}
