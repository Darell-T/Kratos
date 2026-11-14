//! When an agent's file write asks the user, the same for a Claude tool and a Codex patch. Both
//! write as the user, outside any sandbox, so where the file lands decides.

use std::io;
use std::path::Path;

use kratos_protocol::{AskReason, Permission, ThreadSettings};

use super::codex::{ProtectedFolders, sandbox_may_write};

/// Where a write lands, judged where its path resolves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    /// The thread's workspace or the temp folder, which every sandboxed mode writes.
    Inside,
    Outside,
    /// A folder the user must approve each change to (D20). It names the folder.
    Protected(String),
}

/// Where a write to `path` lands for a thread in `folder`, resolving links on the way.
///
/// # Errors
/// Returns an error when the path cannot be resolved, which no caller may read as safe.
pub(crate) fn destination(
    path: &Path,
    folder: &Path,
    protected: &ProtectedFolders,
) -> io::Result<Destination> {
    if let Some(protected_folder) = protected.containing(path)? {
        return Ok(Destination::Protected(
            protected_folder.display().to_string(),
        ));
    }
    Ok(if sandbox_may_write(path, folder)? {
        Destination::Inside
    } else {
        Destination::Outside
    })
}

/// Why a write asks the user, or `None` when it runs on its own. A protected folder always asks.
/// Ask me and plan mode ask before every write. Approve for me asks before a write outside the
/// workspace and the temp folder, and Full access does not.
pub(crate) fn asks(settings: &ThreadSettings, destination: Destination) -> Option<AskReason> {
    match (destination, settings.plan_mode, settings.permission) {
        (Destination::Protected(path), _, _) => Some(AskReason::ProtectedFolder { path }),
        (_, true, _) | (_, _, Permission::AskMe) => Some(AskReason::AskEveryAction),
        (Destination::Outside, false, Permission::ApproveForMe) => Some(AskReason::Escalation),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_permission_and_destination_has_one_verdict_for_both_providers() {
        // Catches Approve for me letting a write outside the workspace run unasked, which a Codex
        // patch did while a Claude write asked, and any permission skipping a protected folder.
        let protected = || Destination::Protected("credentials".into());
        let every = Some(AskReason::AskEveryAction);
        for (permission, plan_mode, destination, expected) in [
            (Permission::AskMe, false, Destination::Inside, every.clone()),
            (
                Permission::AskMe,
                false,
                Destination::Outside,
                every.clone(),
            ),
            (Permission::ApproveForMe, false, Destination::Inside, None),
            (
                Permission::ApproveForMe,
                false,
                Destination::Outside,
                Some(AskReason::Escalation),
            ),
            (Permission::FullAccess, false, Destination::Inside, None),
            (Permission::FullAccess, false, Destination::Outside, None),
            (
                Permission::FullAccess,
                true,
                Destination::Inside,
                every.clone(),
            ),
            (Permission::ApproveForMe, true, Destination::Outside, every),
        ] {
            let settings = ThreadSettings {
                permission,
                plan_mode,
                ..ThreadSettings::default()
            };
            assert_eq!(
                asks(&settings, destination.clone()),
                expected,
                "{permission:?}, plan {plan_mode}, {destination:?}"
            );
        }
        for permission in Permission::ALL {
            for plan_mode in [false, true] {
                let settings = ThreadSettings {
                    permission,
                    plan_mode,
                    ..ThreadSettings::default()
                };
                assert_eq!(
                    asks(&settings, protected()),
                    Some(AskReason::ProtectedFolder {
                        path: "credentials".into()
                    })
                );
            }
        }
    }
}
