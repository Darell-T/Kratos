use std::io;
use std::path::Path;

use kratos_protocol::{AskReason, Permission, ThreadSettings};

use super::codex::{ProtectedFolders, sandbox_may_write};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    Inside,
    Outside,
    Protected(String),
}

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
