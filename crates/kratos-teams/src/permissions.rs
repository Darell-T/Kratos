//! What each role may do without asking, and what it must ask the user first (D84, D93).
//!
//! The planner reads and never changes anything. The reviewer writes only inside its own worktree, to
//! leave probe files. The implementer follows the Team's permission:
//!
//! | Permission     | Asks the user about |
//! | -------------- | ------------------- |
//! | Ask me         | every action that changes something |
//! | Approve for me | commands flagged as potentially dangerous, by the provider's check or by Kratos's |
//! | Full access    | destructive commands, the ones Kratos's dangerous-command check flags (D18) |
//!
//! A write outside the worktree asks only under Ask me, unless the command is flagged as dangerous or
//! destructive. A write to a protected folder asks under every permission (D20).
//!
//! These rules are pure. The engine calls `approval` where the action runs, so the policy the user sees
//! is the policy in effect, and a permission change cannot reach a turn already in progress, because the
//! permission is frozen in the task's policy.

use kratos_protocol::Permission;

use crate::members::Role;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Read,
    Write,
    Command,
}

/// Where an action lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    InsideWorktree,
    OutsideWorktree,
    /// A Kratos, Claude, Codex, or credential folder (D20).
    ProtectedFolder,
}

/// How dangerous a command is. Kratos runs the dangerous-command check on every command (D18).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    Ordinary,
    /// Flagged by the provider's own check or by Kratos's.
    PotentiallyDangerous,
    /// Flagged by Kratos's dangerous-command check, such as a forced delete.
    Destructive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action {
    pub kind: ActionKind,
    pub place: Place,
    pub risk: Risk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Allow,
    /// The user decides.
    Ask,
    /// Not allowed for this role, whatever the user says.
    Deny,
}

/// What `role` may do with `action`. `permission` is the Team's, and only the implementer follows it.
pub fn approval(role: Role, permission: Permission, action: Action) -> Approval {
    if action.kind == ActionKind::Read {
        return Approval::Allow;
    }
    match role {
        Role::Planner => Approval::Deny,
        Role::Reviewer => reviewer_approval(action),
        Role::Implementer => implementer_approval(permission, action),
    }
}

/// The reviewer works in its own worktree only, and a flagged command still goes to the user.
fn reviewer_approval(action: Action) -> Approval {
    if action.place != Place::InsideWorktree {
        return Approval::Deny;
    }
    if action.risk == Risk::Ordinary {
        Approval::Allow
    } else {
        Approval::Ask
    }
}

fn implementer_approval(permission: Permission, action: Action) -> Approval {
    let flagged = match permission {
        Permission::AskMe => true,
        Permission::ApproveForMe => action.risk >= Risk::PotentiallyDangerous,
        Permission::FullAccess => action.risk == Risk::Destructive,
    };
    let leaves_worktree = action.place == Place::OutsideWorktree
        && (permission == Permission::AskMe || action.risk >= Risk::PotentiallyDangerous);
    if flagged || leaves_worktree || action.place == Place::ProtectedFolder {
        Approval::Ask
    } else {
        Approval::Allow
    }
}
