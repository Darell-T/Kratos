//! What Kratos does with each request Codex sends for approval: accept it, or ask the user and say
//! why. Every turn runs under Codex's `untrusted` approval policy, which measurement showed sends
//! every command and every patch here first, so this is where the thread's permission takes
//! effect (D84, D93, D18, D20).

use std::path::Path;

use kratos_command_safety::{
    DangerousCommandPlatform, dangerous_command, reads_commands_from_input, split_command_text,
};
use kratos_protocol::{AskReason, FileChange, FileChangeKind, Permission, ThreadSettings};

use super::profiles::ProtectedFolders;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    Ask(AskReason),
}

/// The parts of a command approval request the decision reads.
#[derive(Clone, Copy, Debug)]
pub struct CommandRequest<'a> {
    /// The command as Codex shows it: its words joined with shell quoting.
    pub command: Option<&'a str>,
    /// The agent's reason for asking, which Codex sends with a request to leave the sandbox.
    pub reason: Option<&'a str>,
    pub wants_more_permissions: bool,
    pub wants_network: bool,
}

/// Ask me asks about everything (D84). Under the other permissions a command runs without asking
/// unless it asks to leave the sandbox, the dangerous-command check flags it (D18), or it starts a
/// shell or interpreter whose later input Kratos would never see. Plan mode changes the sandbox,
/// not what asks: its profile writes nothing.
pub fn decide_command(
    settings: &ThreadSettings,
    request: &CommandRequest<'_>,
    platform: DangerousCommandPlatform,
) -> Verdict {
    if settings.permission == Permission::AskMe {
        return Verdict::Ask(AskReason::AskEveryAction);
    }
    if request.reason.is_some() || request.wants_more_permissions || request.wants_network {
        return Verdict::Ask(AskReason::Escalation);
    }
    let Some(words) = request.command.and_then(split_command_text) else {
        return Verdict::Ask(AskReason::Unchecked);
    };
    if dangerous_command(&words, platform).is_some() {
        return Verdict::Ask(AskReason::Dangerous);
    }
    if reads_commands_from_input(&words) {
        return Verdict::Ask(AskReason::Unchecked);
    }
    Verdict::Accept
}

/// An approved patch writes wherever its paths point, outside the sandbox, so its paths decide.
/// `changes` is `None` when Codex has not said which files the patch touches. Relative paths are
/// read against the thread's `folder`.
pub fn decide_patch(
    settings: &ThreadSettings,
    changes: Option<&[FileChange]>,
    asks_for_root: bool,
    folder: &Path,
    protected: &ProtectedFolders,
) -> Verdict {
    let Some(changes) = changes else {
        return Verdict::Ask(AskReason::Unchecked);
    };
    for path in changes.iter().flat_map(changed_paths) {
        match protected.containing(&folder.join(path)) {
            Ok(Some(protected_folder)) => {
                return Verdict::Ask(AskReason::ProtectedFolder {
                    path: protected_folder.display().to_string(),
                });
            }
            Err(_) => return Verdict::Ask(AskReason::Unchecked),
            Ok(None) => {}
        }
    }
    if settings.permission == Permission::AskMe || settings.plan_mode {
        return Verdict::Ask(AskReason::AskEveryAction);
    }
    if asks_for_root {
        return Verdict::Ask(AskReason::Escalation);
    }
    Verdict::Accept
}

/// The file a change writes, and the file a move writes to.
fn changed_paths(change: &FileChange) -> impl Iterator<Item = &str> {
    let moved_to = match &change.kind {
        FileChangeKind::Update {
            move_path: Some(path),
        } => Some(path.as_str()),
        _ => None,
    };
    std::iter::once(change.path.as_str()).chain(moved_to)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kratos_protocol::CappedText;

    use super::*;

    const POSIX: DangerousCommandPlatform = DangerousCommandPlatform::Posix;

    fn settings(permission: Permission, plan_mode: bool) -> ThreadSettings {
        ThreadSettings {
            permission,
            plan_mode,
            ..ThreadSettings::default()
        }
    }

    fn command(text: &str) -> CommandRequest<'_> {
        CommandRequest {
            command: Some(text),
            reason: None,
            wants_more_permissions: false,
            wants_network: false,
        }
    }

    fn protected() -> ProtectedFolders {
        let home = PathBuf::from("/home/u");
        ProtectedFolders::new(home.join(".kratos"), Some(home.join(".codex")), Some(home))
    }

    fn change(path: &str, kind: FileChangeKind) -> FileChange {
        FileChange {
            path: path.to_owned(),
            kind,
            diff: CappedText::new("", 0),
        }
    }

    #[test]
    fn each_permission_asks_about_what_d84_and_d18_name() {
        // Catches a permission that lets a forced delete through, an Ask me thread that runs
        // anything unasked, and a check that misses a bare shell started inside Codex's login
        // shell wrapper.
        let cases = [
            (
                "/bin/bash -lc 'cargo test'",
                Permission::AskMe,
                Verdict::Ask(AskReason::AskEveryAction),
            ),
            (
                "/bin/bash -lc 'cargo test'",
                Permission::ApproveForMe,
                Verdict::Accept,
            ),
            (
                "/bin/bash -lc 'cargo test'",
                Permission::FullAccess,
                Verdict::Accept,
            ),
            (
                "/bin/bash -lc 'rm -rf build'",
                Permission::ApproveForMe,
                Verdict::Ask(AskReason::Dangerous),
            ),
            (
                "/bin/bash -lc 'rm -rf build'",
                Permission::FullAccess,
                Verdict::Ask(AskReason::Dangerous),
            ),
            (
                "/bin/bash -lc bash",
                Permission::FullAccess,
                Verdict::Ask(AskReason::Unchecked),
            ),
            (
                "/bin/bash -lc 'unclosed",
                Permission::FullAccess,
                Verdict::Ask(AskReason::Unchecked),
            ),
        ];
        for (text, permission, expected) in cases {
            for plan_mode in [false, true] {
                let verdict =
                    decide_command(&settings(permission, plan_mode), &command(text), POSIX);
                assert_eq!(
                    verdict, expected,
                    "{text} under {permission:?}, plan {plan_mode}"
                );
            }
        }
    }

    #[test]
    fn a_request_to_leave_the_sandbox_asks_under_every_permission() {
        // Catches an escalation accepted under Full access, which would hand the agent extra
        // permissions such as a write root in a protected folder.
        let escalations = [
            CommandRequest {
                reason: Some("needs ~/.ssh"),
                ..command("/bin/bash -lc ls")
            },
            CommandRequest {
                wants_more_permissions: true,
                ..command("/bin/bash -lc ls")
            },
            CommandRequest {
                wants_network: true,
                ..command("/bin/bash -lc 'curl example.com'")
            },
        ];
        for request in escalations {
            let verdict = decide_command(&settings(Permission::FullAccess, false), &request, POSIX);
            assert_eq!(verdict, Verdict::Ask(AskReason::Escalation), "{request:?}");
        }
    }

    #[test]
    fn a_patch_into_a_protected_folder_asks_whatever_the_permission() {
        // Catches the measured gap: an approved patch writes into a read-only protected folder,
        // and a move counts as a write to its destination.
        let folder = Path::new("/work/repo");
        let into_ssh = [change("/home/u/.ssh/config", FileChangeKind::Add)];
        let moved_into_kratos = [change(
            "notes.md",
            FileChangeKind::Update {
                move_path: Some("../../home/u/.kratos/notes.md".into()),
            },
        )];
        for changes in [&into_ssh[..], &moved_into_kratos] {
            let verdict = decide_patch(
                &settings(Permission::FullAccess, false),
                Some(changes),
                false,
                folder,
                &protected(),
            );
            assert!(
                matches!(verdict, Verdict::Ask(AskReason::ProtectedFolder { .. })),
                "{changes:?}"
            );
        }
    }

    #[test]
    fn a_patch_asks_under_ask_me_and_plan_mode_and_when_its_files_are_unknown() {
        // Catches a planning thread whose patch is accepted, which would write despite its
        // read-only sandbox, and a patch accepted before Kratos saw which files it touches.
        let folder = Path::new("/work/repo");
        let inside = [change(
            "src/main.rs",
            FileChangeKind::Update { move_path: None },
        )];
        let decide = |permission, plan_mode, changes| {
            decide_patch(
                &settings(permission, plan_mode),
                changes,
                false,
                folder,
                &protected(),
            )
        };

        assert_eq!(
            decide(Permission::AskMe, false, Some(&inside[..])),
            Verdict::Ask(AskReason::AskEveryAction)
        );
        assert_eq!(
            decide(Permission::FullAccess, true, Some(&inside[..])),
            Verdict::Ask(AskReason::AskEveryAction)
        );
        assert_eq!(
            decide(Permission::ApproveForMe, false, None),
            Verdict::Ask(AskReason::Unchecked)
        );
        assert_eq!(
            decide(Permission::ApproveForMe, false, Some(&inside[..])),
            Verdict::Accept
        );
    }
}
