//! Native CLI discovery with explicit override and PATH search diagnostics.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Finds the native executable from `CLAUDE_EXE`, otherwise the first matching PATH entry.
///
/// # Errors
/// Returns the override or searched directories when no runnable native CLI is found.
pub fn locate_claude(
    claude_exe: Option<&OsStr>,
    path: Option<&OsStr>,
) -> Result<PathBuf, LocateError> {
    if let Some(value) = claude_exe.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if cfg!(windows)
            && path.extension().is_some_and(|extension| {
                ["cmd", "bat", "ps1"]
                    .iter()
                    .any(|script| extension.eq_ignore_ascii_case(script))
            })
        {
            return Err(LocateError::Launcher(path));
        }
        return if runnable(&path) {
            Ok(path)
        } else {
            Err(LocateError::InvalidOverride(path))
        };
    }
    let searched: Vec<PathBuf> = path.into_iter().flat_map(std::env::split_paths).collect();
    for folder in &searched {
        let candidate = folder.join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        });
        if runnable(&candidate) {
            return Ok(candidate);
        }
        if cfg!(windows) {
            let launcher = folder.join("claude.cmd");
            if launcher.is_file() {
                return Err(LocateError::Launcher(launcher));
            }
        }
    }
    Err(LocateError::NotOnPath { searched })
}

fn runnable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Search evidence for a missing native installation.
#[derive(Debug, thiserror::Error)]
pub enum LocateError {
    #[error("CLAUDE_EXE names {}, which is not a runnable file", .0.display())]
    InvalidOverride(PathBuf),
    #[error("found npm launcher {}; install the native Claude CLI or set CLAUDE_EXE to its executable", .0.display())]
    Launcher(PathBuf),
    #[error("cannot find claude on PATH; searched {}; install the native Claude CLI or set CLAUDE_EXE", searched.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join("; "))]
    NotOnPath { searched: Vec<PathBuf> },
}
