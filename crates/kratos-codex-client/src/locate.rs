//! Finds the `codex` executable the way the user's shell would, then looks through the npm
//! launcher to the native binary it starts (D2, D5, L8).
//!
//! `npm install -g @openai/codex` puts a Node script on PATH (`codex.cmd` on Windows). Kratos never
//! starts it: it starts the platform binary that script would start, from the platform package's
//! `vendor/<target>/bin` folder.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The npm package that holds the native binary for this machine, and its target folder.
const PLATFORM: Option<(&str, &str)> = if cfg!(all(windows, target_arch = "x86_64")) {
    Some(("codex-win32-x64", "x86_64-pc-windows-msvc"))
} else if cfg!(all(windows, target_arch = "aarch64")) {
    Some(("codex-win32-arm64", "aarch64-pc-windows-msvc"))
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some(("codex-darwin-x64", "x86_64-apple-darwin"))
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some(("codex-darwin-arm64", "aarch64-apple-darwin"))
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some(("codex-linux-x64", "x86_64-unknown-linux-musl"))
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some(("codex-linux-arm64", "aarch64-unknown-linux-musl"))
} else {
    None
};

const BINARY: &str = if cfg!(windows) { "codex.exe" } else { "codex" };

#[derive(Debug, thiserror::Error)]
pub enum LocateError {
    #[error(
        "Could not find codex on PATH. Install Codex with \"npm install -g @openai/codex\" or set CODEX_EXE."
    )]
    NotOnPath,
    #[error(
        "Found the npm launcher {} but not the Codex binary it starts. Reinstall Codex with \"npm install -g @openai/codex\" or set CODEX_EXE.",
        .0.display()
    )]
    NoBinaryBehindLauncher(PathBuf),
    #[error("CODEX_EXE names {}, which is not a file", .0.display())]
    NotAFile(PathBuf),
}

/// The binary to start: `codex_exe` (the `CODEX_EXE` variable) when set, else the first `codex` on
/// `path` (the `PATH` variable), seen through the npm launcher when it is one.
pub fn locate_codex(
    codex_exe: Option<&OsStr>,
    path: Option<&OsStr>,
) -> Result<PathBuf, LocateError> {
    if let Some(codex_exe) = codex_exe.filter(|value| !value.is_empty()) {
        let codex_exe = PathBuf::from(codex_exe);
        return if codex_exe.is_file() {
            Ok(codex_exe)
        } else {
            Err(LocateError::NotAFile(codex_exe))
        };
    }
    let found = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .find_map(|folder| on_path(&folder))
        .ok_or(LocateError::NotOnPath)?;
    match found {
        Found::Binary(binary) => Ok(binary),
        Found::Launcher(launcher) => {
            behind_launcher(&launcher).ok_or(LocateError::NoBinaryBehindLauncher(launcher))
        }
    }
}

enum Found {
    Binary(PathBuf),
    /// An npm launcher, after following any symlink to `bin/codex.js` in the package.
    Launcher(PathBuf),
}

/// What `folder` holds under the name `codex`, in the order a shell tries names.
fn on_path(folder: &Path) -> Option<Found> {
    let binary = folder.join(BINARY);
    if cfg!(windows) {
        if binary.is_file() {
            return Some(Found::Binary(binary));
        }
        // npm writes codex.cmd, codex.ps1, and a shell script named codex next to each other.
        let launcher = folder.join("codex.cmd");
        return launcher.is_file().then_some(Found::Launcher(launcher));
    }
    if !binary.is_file() {
        return None;
    }
    let target = std::fs::canonicalize(&binary).unwrap_or_else(|_| binary.clone());
    if target.extension() == Some(OsStr::new("js")) {
        Some(Found::Launcher(target))
    } else {
        Some(Found::Binary(binary))
    }
}

/// The binary an npm launcher starts. On Windows the launcher sits in npm's global folder, next to
/// `node_modules`; elsewhere it is `bin/codex.js` inside the package.
fn behind_launcher(launcher: &Path) -> Option<PathBuf> {
    let (platform_package, target) = PLATFORM?;
    let folder = launcher.parent()?;
    let package = if cfg!(windows) {
        folder.join("node_modules").join("@openai").join("codex")
    } else {
        folder.parent()?.to_path_buf()
    };
    let candidates = [
        package
            .join("node_modules")
            .join("@openai")
            .join(platform_package),
        package.parent()?.join(platform_package),
        package.clone(),
    ];
    candidates
        .iter()
        .map(|root| root.join("vendor").join(target).join("bin").join(BINARY))
        .find(|binary| binary.is_file())
}
