//! The sandbox every Codex turn runs in. Plan, Ask me, and Approve for me select named permission
//! profiles Kratos defines when it starts the app-server. Full access selects Codex's built-in
//! `:danger-full-access`, which runs commands without a sandbox, as Zeron does. These profiles write
//! only the thread's workspaces and the temp folder: on Windows, Codex sets permissions on every
//! file below a write root before the thread starts, and for a home folder that never finished.
//!
//! Measured on Codex 0.159.1: a profile's `deny` entry hides a folder from reads and writes, a
//! `read` entry makes it read-only, and Codex keeps a command inside the sandbox even after the
//! user approves running it outside whenever the profile denies any read. Every Kratos profile
//! denies Kratos's data folder, so no sandboxed command can read the client token (D19). A Full
//! access command runs as the user and can. An approved patch writes wherever it points, so
//! [`super::policy`] checks patch paths against [`ProtectedFolders`] before accepting one.
//! A Claude command runs in exactly the profile a Codex thread at the same level uses. Claude's own
//! file tools run outside every sandbox, so [`sandbox_may_write`] gives them the same scope.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

use kratos_protocol::{Permission, ThreadSettings};

/// The profile a turn selects, from the thread's settings at the turn's start (D87).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Profile {
    /// Reads everything but Kratos's data and writes nothing.
    Plan,
    /// Writes the thread's workspaces and the temp folder.
    AskMe,
    /// As [`Profile::AskMe`], with the network open.
    ApproveForMe,
    /// No sandbox. [`super::policy`] still asks before dangerous commands.
    FullAccess,
}

impl Profile {
    pub(super) fn for_settings(settings: &ThreadSettings) -> Profile {
        match (settings.plan_mode, settings.permission) {
            (true, _) => Profile::Plan,
            (false, Permission::AskMe) => Profile::AskMe,
            (false, Permission::ApproveForMe) => Profile::ApproveForMe,
            (false, Permission::FullAccess) => Profile::FullAccess,
        }
    }

    /// The id the app-server knows the profile by.
    pub(super) fn id(self) -> &'static str {
        match self {
            Profile::Plan => "kratos-plan",
            Profile::AskMe => "kratos-ask",
            Profile::ApproveForMe => "kratos-approve",
            Profile::FullAccess => ":danger-full-access",
        }
    }
}

/// Folders an agent may not change without the user (D20), and Kratos's own data folder, which an
/// agent may not even read (D19).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedFolders {
    kratos_home: PathBuf,
    read_only: Vec<PathBuf>,
}

/// Credential folders under the user's home folder, besides Codex's and Claude's own.
const CREDENTIAL_FOLDERS: [&str; 4] = [".ssh", ".gnupg", ".aws", ".config/gh"];

impl ProtectedFolders {
    /// `codex_home` is Codex's data folder, `CODEX_HOME` or `~/.codex`. Every path is absolute.
    pub fn new(kratos_home: PathBuf, codex_home: Option<PathBuf>, home: Option<PathBuf>) -> Self {
        let mut read_only: Vec<PathBuf> = codex_home.into_iter().collect();
        if let Some(home) = &home {
            read_only.push(home.join(".claude"));
            read_only.extend(CREDENTIAL_FOLDERS.iter().map(|folder| home.join(folder)));
        }
        ProtectedFolders {
            kratos_home,
            read_only,
        }
    }

    pub fn kratos_home(&self) -> &Path {
        &self.kratos_home
    }

    /// Resolves existing links before checking a patch's destination, including new files below
    /// a linked directory. An unreadable or dangling link cannot establish a safe destination.
    /// Neither can a missing-parent climb or a new name the disk may interpret differently.
    pub(crate) fn containing(&self, path: &Path) -> io::Result<Option<&Path>> {
        let path = resolve_destination(path)?;
        for folder in std::iter::once(&self.kratos_home).chain(&self.read_only) {
            if is_within(&path, &resolve_destination(folder)?) {
                return Ok(Some(folder));
            }
        }
        Ok(None)
    }

    /// Recursive searches must not start above a protected folder either.
    pub(crate) fn overlaps(&self, path: &Path) -> io::Result<bool> {
        let path = resolve_destination(path)?;
        for folder in std::iter::once(&self.kratos_home).chain(&self.read_only) {
            let folder = resolve_destination(folder)?;
            if is_within(&path, &folder) || is_within(&folder, &path) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The `-c` arguments that define Kratos's three profiles, for the app-server's command line.
    /// `temp` is the folder Ask me and Approve for me let commands write besides the workspaces.
    pub(super) fn launch_arguments(&self, temp: &Path) -> Vec<OsString> {
        let deny = format!("{}=\"deny\"", toml_key(&self.kratos_home));
        let workspaces = vec![
            r#"":root"="read""#.to_owned(),
            r#"":workspace_roots"="write""#.to_owned(),
            format!("{}=\"write\"", toml_key(temp)),
            deny.clone(),
        ];
        let tables = [
            (Profile::Plan, vec![r#"":root"="read""#.to_owned(), deny]),
            (Profile::AskMe, workspaces.clone()),
            (Profile::ApproveForMe, workspaces),
        ];
        let mut arguments = vec![
            "-c".into(),
            format!("default_permissions=\"{}\"", Profile::Plan.id()).into(),
        ];
        for (profile, entries) in tables {
            arguments.push("-c".into());
            arguments.push(
                format!(
                    "permissions.{}.filesystem={{{}}}",
                    profile.id(),
                    entries.join(", ")
                )
                .into(),
            );
        }
        arguments.push("-c".into());
        arguments.push(
            format!(
                "permissions.{}.network.enabled=true",
                Profile::ApproveForMe.id()
            )
            .into(),
        );
        arguments
    }
}

/// The temp folder Ask me and Approve for me let commands write besides the thread's workspace.
pub(crate) fn temp_folder() -> PathBuf {
    std::env::temp_dir()
}

/// A TOML basic string, quoted and escaped, for a path used as a table key.
fn toml_key(path: &Path) -> String {
    let mut key = String::from("\"");
    for character in path.to_string_lossy().chars() {
        match character {
            '"' => key.push_str("\\\""),
            '\\' => key.push_str("\\\\"),
            character if character.is_control() => {
                key.push_str(&format!("\\u{:04X}", u32::from(character)));
            }
            character => key.push(character),
        }
    }
    key.push('"');
    key
}

fn resolve_destination(path: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    if let Some(Component::Prefix(prefix)) = path.components().next()
        && matches!(
            prefix.kind(),
            std::path::Prefix::UNC(..)
                | std::path::Prefix::VerbatimUNC(..)
                | std::path::Prefix::DeviceNS(..)
                | std::path::Prefix::Verbatim(..)
        )
    {
        // Even a metadata lookup on a network path can disclose ambient Windows credentials.
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a remote or device patch path needs user review",
        ));
    }
    let mut ancestor = path.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(&ancestor) {
            Ok(mut resolved) => {
                resolved.extend(suffix.iter().rev());
                return Ok(resolved);
            }
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound
                    || std::fs::symlink_metadata(&ancestor).is_ok()
                {
                    return Err(error);
                }
                let Some(Component::Normal(part)) = ancestor.components().next_back() else {
                    return Err(error);
                };
                let name = part.as_encoded_bytes();
                let invalid_folder = !suffix.is_empty()
                    && (!name.is_ascii() || matches!(name.last(), Some(b'.' | b' ')));
                if name.contains(&b':') || invalid_folder {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "an unresolved patch name may have a filesystem alias",
                    ));
                }
                suffix.push(part.to_os_string());
                ancestor.pop();
            }
        }
    }
}

fn is_within(path: &Path, folder: &Path) -> bool {
    let lower = |path: &Path| path.to_string_lossy().to_lowercase().replace('/', "\\");
    let (path, folder) = (lower(path), lower(folder));
    let folder = folder.trim_end_matches('\\');
    path == folder || path.starts_with(&format!("{folder}\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn network_patch_paths_are_refused_before_filesystem_resolution() {
        let error = resolve_destination(Path::new(r"\\untrusted.invalid\share\file")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    fn folders() -> ProtectedFolders {
        let root = if cfg!(windows) {
            "C:\\Users\\u"
        } else {
            "/home/u"
        };
        let home = PathBuf::from(root);
        ProtectedFolders::new(home.join(".kratos"), Some(home.join(".codex")), Some(home))
    }

    #[test]
    fn a_path_that_climbs_back_into_a_protected_folder_is_still_inside_it() {
        // Catches a text-only check missing work/../.ssh/config through an existing folder.
        let root = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(root.path()).unwrap();
        std::fs::create_dir(home.join("work")).unwrap();
        let folders = ProtectedFolders::new(home.join(".kratos"), None, Some(home.clone()));

        let climbing = home.join("work").join("..").join(".ssh").join("config");
        let lookalike = home.join(".sshkeys").join("config");

        assert_eq!(
            folders.containing(&climbing).unwrap(),
            Some(home.join(".ssh").as_path())
        );
        assert_eq!(folders.containing(&lookalike).unwrap(), None);
        assert_eq!(
            folders
                .containing(&home.join(".kratos").join("client-token"))
                .unwrap(),
            Some(home.join(".kratos").as_path())
        );
    }

    #[test]
    fn sandboxed_profiles_write_only_workspaces_and_temp_and_full_access_has_no_sandbox() {
        // Catches a profile that writes the home folder, which on Windows makes Codex change the
        // permissions of every file in it before a thread starts, a sandboxed profile that forgets
        // the deny entry, which would let an agent command read the client token, and a network
        // switch on the wrong profile.
        let folders = folders();
        let arguments: Vec<String> = folders
            .launch_arguments(Path::new("/tmp"))
            .into_iter()
            .map(|argument| argument.into_string().unwrap())
            .collect();
        let deny = format!("{}=\"deny\"", toml_key(folders.kratos_home()));
        let workspaces =
            format!(r#"":root"="read", ":workspace_roots"="write", "/tmp"="write", {deny}"#);
        let table = |id: &str| {
            arguments
                .iter()
                .find(|argument| argument.starts_with(&format!("permissions.{id}.filesystem=")))
                .cloned()
        };

        assert_eq!(
            table("kratos-plan"),
            Some(format!(
                r#"permissions.kratos-plan.filesystem={{":root"="read", {deny}}}"#
            ))
        );
        for id in ["kratos-ask", "kratos-approve"] {
            assert_eq!(
                table(id),
                Some(format!("permissions.{id}.filesystem={{{workspaces}}}"))
            );
        }
        assert_eq!(Profile::FullAccess.id(), ":danger-full-access");
        let network: Vec<&String> = arguments
            .iter()
            .filter(|argument| argument.contains("network"))
            .collect();
        assert_eq!(network, ["permissions.kratos-approve.network.enabled=true"]);
    }

    #[test]
    fn a_windows_path_becomes_an_escaped_toml_key() {
        // Catches unescaped backslashes, which TOML reads as escapes, so the profile would protect
        // a different folder or fail to parse.
        assert_eq!(
            toml_key(Path::new("C:\\Users\\a \"b\"")),
            "\"C:\\\\Users\\\\a \\\"b\\\"\""
        );
    }
}
