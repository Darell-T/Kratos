//! The sandbox every Codex turn runs in, as named permission profiles Kratos defines when it starts
//! the app-server and selects per turn.
//!
//! Measured on Codex 0.159.1: a profile's `deny` entry hides a folder from reads and writes, a
//! `read` entry makes it read-only, and Codex keeps a command inside the sandbox even after the
//! user approves running it outside whenever the profile denies any read. Every profile denies
//! Kratos's data folder, so no agent command can read the client token (D19). An approved patch is
//! the exception: Codex writes it wherever it points, so [`super::policy`] checks patch paths
//! against [`ProtectedFolders`] before accepting one.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

use kratos_protocol::{Permission, ThreadSettings};

/// The profile a turn selects, from the thread's settings at the turn's start (D87).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Reads everything but Kratos's data and writes nothing.
    Plan,
    /// Writes the thread's folder and the temp folder.
    AskMe,
    /// Writes user folders and registered workspaces on Windows, everywhere on Unix, except
    /// protected folders. See docs/r3-verification.md for the Windows limitation.
    ApproveForMe,
    /// As [`Profile::ApproveForMe`], with the network open.
    FullAccess,
}

const PROFILES: [(Profile, &str); 4] = [
    (Profile::Plan, "kratos-plan"),
    (Profile::AskMe, "kratos-ask"),
    (Profile::ApproveForMe, "kratos-approve"),
    (Profile::FullAccess, "kratos-full"),
];

impl Profile {
    pub fn for_settings(settings: &ThreadSettings) -> Profile {
        match (settings.plan_mode, settings.permission) {
            (true, _) => Profile::Plan,
            (false, Permission::AskMe) => Profile::AskMe,
            (false, Permission::ApproveForMe) => Profile::ApproveForMe,
            (false, Permission::FullAccess) => Profile::FullAccess,
        }
    }

    /// The id the app-server knows the profile by.
    pub fn id(self) -> &'static str {
        PROFILES
            .iter()
            .find(|(profile, _)| *profile == self)
            .map(|(_, id)| *id)
            .expect("every profile has an id")
    }
}

/// Folders an agent may not change without the user (D20), and Kratos's own data folder, which an
/// agent may not even read (D19).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedFolders {
    kratos_home: PathBuf,
    read_only: Vec<PathBuf>,
    user_home: Option<PathBuf>,
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
            user_home: home,
        }
    }

    pub fn kratos_home(&self) -> &Path {
        &self.kratos_home
    }

    /// Resolves existing links before checking a patch's destination, including new files below
    /// a linked directory. An unreadable or dangling link cannot establish a safe destination.
    pub fn containing(&self, path: &Path) -> io::Result<Option<&Path>> {
        let path = resolve_destination(path)?;
        for folder in std::iter::once(&self.kratos_home).chain(&self.read_only) {
            if is_within(&path, &resolve_destination(folder)?) {
                return Ok(Some(folder));
            }
        }
        Ok(None)
    }

    /// The `-c` arguments that define the four profiles, for the app-server's command line. `temp`
    /// is the folder Ask me lets commands write besides the thread's own.
    pub fn launch_arguments(&self, temp: &Path) -> Vec<OsString> {
        let deny = format!("{}=\"deny\"", toml_key(&self.kratos_home));
        let read_only: Vec<String> = self
            .read_only
            .iter()
            .map(|folder| format!("{}=\"read\"", toml_key(folder)))
            .collect();
        let write_everywhere = [self.write_roots(temp), read_only, vec![deny.clone()]].concat();
        let tables = [
            (
                Profile::Plan,
                vec![r#"":root"="read""#.to_owned(), deny.clone()],
            ),
            (
                Profile::AskMe,
                vec![
                    r#"":root"="read""#.to_owned(),
                    r#"":workspace_roots"="write""#.to_owned(),
                    format!("{}=\"write\"", toml_key(temp)),
                    deny,
                ],
            ),
            (Profile::ApproveForMe, write_everywhere.clone()),
            (Profile::FullAccess, write_everywhere),
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
                Profile::FullAccess.id()
            )
            .into(),
        );
        arguments
    }

    fn write_roots(&self, temp: &Path) -> Vec<String> {
        if !cfg!(windows) {
            return vec![r#"":root"="write""#.to_owned()];
        }
        // shortcut: Windows root-write needs system-drive ACL changes; restore it when Codex can enforce carveouts without that grant.
        let mut roots = vec![
            r#"":root"="read""#.to_owned(),
            r#"":workspace_roots"="write""#.to_owned(),
            format!("{}=\"write\"", toml_key(temp)),
        ];
        if let Some(home) = &self.user_home {
            roots.push(format!("{}=\"write\"", toml_key(home)));
        }
        roots
    }
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
                for part in suffix.iter().rev() {
                    resolved.push(part);
                }
                return Ok(normalize(&resolved));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(&ancestor).is_ok() {
                    return Err(error);
                }
                let Some(part) = ancestor.components().next_back() else {
                    return Err(error);
                };
                suffix.push(part.as_os_str().to_os_string());
                if !ancestor.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// Only the not-yet-created suffix needs lexical normalization; existing links are resolved first.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn is_within(path: &Path, folder: &Path) -> bool {
    if cfg!(windows) {
        let lower = |path: &Path| path.to_string_lossy().to_lowercase().replace('/', "\\");
        let (path, folder) = (lower(path), lower(folder));
        let folder = folder.trim_end_matches('\\');
        path == folder || path.starts_with(&format!("{folder}\\"))
    } else {
        path.starts_with(folder)
    }
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
        // Catches a check that compares text, which `project/../../.ssh/config` would pass.
        let folders = folders();
        let home = folders.kratos_home().parent().unwrap().to_path_buf();

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
