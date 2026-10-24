//! What each role may do without asking (D84, D93, D20, D18). The planner reads and never changes
//! anything. The reviewer writes only in its own worktree. The implementer follows the Team's
//! permission, and a protected folder and a destructive command ask under every permission.

use kratos_protocol::Permission;
use kratos_teams::members::Role;
use kratos_teams::permissions::{Action, ActionKind, Approval, Place, Risk, approval};

use Approval::{Allow, Ask, Deny};

fn write(place: Place) -> Action {
    Action {
        kind: ActionKind::Write,
        place,
        risk: Risk::Ordinary,
    }
}

fn command(place: Place, risk: Risk) -> Action {
    Action {
        kind: ActionKind::Command,
        place,
        risk,
    }
}

fn read(place: Place) -> Action {
    Action {
        kind: ActionKind::Read,
        place,
        risk: Risk::Ordinary,
    }
}

const PERMISSIONS: [Permission; 3] = [
    Permission::AskMe,
    Permission::ApproveForMe,
    Permission::FullAccess,
];

#[test]
fn reading_is_never_asked_about_and_never_refused() {
    for role in [Role::Planner, Role::Implementer, Role::Reviewer] {
        for permission in PERMISSIONS {
            assert_eq!(
                approval(role, permission, read(Place::InsideWorktree)),
                Allow
            );
        }
    }
}

#[test]
fn the_planner_never_changes_anything_whatever_the_teams_permission_says() {
    for permission in PERMISSIONS {
        assert_eq!(
            approval(Role::Planner, permission, write(Place::InsideWorktree)),
            Deny
        );
        assert_eq!(
            approval(
                Role::Planner,
                permission,
                command(Place::InsideWorktree, Risk::Ordinary)
            ),
            Deny
        );
    }
}

#[test]
fn the_reviewer_writes_only_inside_its_own_worktree_and_a_flagged_command_goes_to_the_user() {
    let reviewer = |action| approval(Role::Reviewer, Permission::FullAccess, action);

    assert_eq!(reviewer(write(Place::InsideWorktree)), Allow);
    assert_eq!(reviewer(write(Place::OutsideWorktree)), Deny);
    assert_eq!(reviewer(write(Place::ProtectedFolder)), Deny);
    assert_eq!(
        reviewer(command(Place::InsideWorktree, Risk::Ordinary)),
        Allow
    );
    assert_eq!(
        reviewer(command(Place::InsideWorktree, Risk::PotentiallyDangerous)),
        Ask
    );
    assert_eq!(
        reviewer(command(Place::InsideWorktree, Risk::Destructive)),
        Ask
    );
}

#[test]
fn ask_me_asks_before_every_action_that_changes_something() {
    let asks = |action| approval(Role::Implementer, Permission::AskMe, action);

    assert_eq!(asks(write(Place::InsideWorktree)), Ask);
    assert_eq!(asks(command(Place::InsideWorktree, Risk::Ordinary)), Ask);
    assert_eq!(asks(write(Place::OutsideWorktree)), Ask);
}

#[test]
fn approve_for_me_asks_only_about_commands_flagged_as_potentially_dangerous() {
    let policy = |action| approval(Role::Implementer, Permission::ApproveForMe, action);

    assert_eq!(policy(write(Place::InsideWorktree)), Allow);
    assert_eq!(
        policy(command(Place::InsideWorktree, Risk::Ordinary)),
        Allow
    );
    assert_eq!(
        policy(command(Place::InsideWorktree, Risk::PotentiallyDangerous)),
        Ask
    );
    assert_eq!(
        policy(command(Place::InsideWorktree, Risk::Destructive)),
        Ask
    );
}

#[test]
fn full_access_asks_only_about_destructive_commands() {
    let policy = |action| approval(Role::Implementer, Permission::FullAccess, action);

    assert_eq!(policy(write(Place::InsideWorktree)), Allow);
    assert_eq!(
        policy(command(Place::InsideWorktree, Risk::Ordinary)),
        Allow
    );
    assert_eq!(
        policy(command(Place::InsideWorktree, Risk::PotentiallyDangerous)),
        Allow
    );
    assert_eq!(
        policy(command(Place::InsideWorktree, Risk::Destructive)),
        Ask
    );
}

#[test]
fn a_write_outside_the_worktree_asks_only_under_ask_me() {
    assert_eq!(
        approval(
            Role::Implementer,
            Permission::AskMe,
            write(Place::OutsideWorktree)
        ),
        Ask
    );
    assert_eq!(
        approval(
            Role::Implementer,
            Permission::ApproveForMe,
            write(Place::OutsideWorktree)
        ),
        Allow
    );
    assert_eq!(
        approval(
            Role::Implementer,
            Permission::FullAccess,
            write(Place::OutsideWorktree)
        ),
        Allow
    );
}

#[test]
fn a_flagged_command_outside_the_worktree_asks_under_every_permission() {
    for permission in PERMISSIONS {
        assert_eq!(
            approval(
                Role::Implementer,
                permission,
                command(Place::OutsideWorktree, Risk::PotentiallyDangerous)
            ),
            Ask,
            "{permission:?}"
        );
        assert_eq!(
            approval(
                Role::Implementer,
                permission,
                command(Place::OutsideWorktree, Risk::Destructive)
            ),
            Ask,
            "{permission:?}"
        );
    }
}

#[test]
fn a_protected_folder_asks_under_every_permission_even_for_an_ordinary_write() {
    for permission in PERMISSIONS {
        assert_eq!(
            approval(Role::Implementer, permission, write(Place::ProtectedFolder)),
            Ask,
            "{permission:?}"
        );
    }
}

#[test]
fn approve_for_me_is_not_full_access_a_dangerous_command_tells_them_apart() {
    let dangerous = command(Place::InsideWorktree, Risk::PotentiallyDangerous);

    assert_ne!(
        approval(Role::Implementer, Permission::ApproveForMe, dangerous),
        approval(Role::Implementer, Permission::FullAccess, dangerous)
    );
}
