//! Paths relative to the repository root, and the globs that match them. Reviewers, agents, and git
//! each spell a path their own way, so every rule that compares paths reads them through here.

use globset::{Glob, GlobBuilder};

/// Reviewers write `src\a.rs` or `./src/a.rs`. Git names the same file `src/a.rs`.
pub fn repo_path(raw: &str) -> String {
    let mut path = raw.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_owned();
    }
    path
}

/// Why a path cannot name a file in the repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathProblem {
    /// It is empty, or it starts at the root or at a drive.
    Rooted,
    /// It climbs out of its folder with a `..`.
    Climbs,
}

/// `raw` spelled the way git spells it, when it is relative to the repository root and stays inside it.
pub(crate) fn parse_repo_path(raw: &str) -> Result<String, PathProblem> {
    let path = repo_path(raw);
    if is_rooted(&path) {
        return Err(PathProblem::Rooted);
    }
    if climbs(&path) {
        return Err(PathProblem::Climbs);
    }
    Ok(path)
}

/// Whether a path that went through `repo_path` has no place to start from. That means it is empty,
/// begins at the root, or begins at a drive.
fn is_rooted(path: &str) -> bool {
    let bytes = path.as_bytes();
    let has_drive =
        bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':');
    path.is_empty() || path.starts_with('/') || has_drive
}

/// Whether a path that went through `repo_path` climbs out of its folder with a `..`.
fn climbs(path: &str) -> bool {
    path.split('/').any(|part| part == "..")
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    #[error("`{pattern}` is not a valid glob: {reason}")]
    NotAGlob { pattern: String, reason: String },
    #[error("`{pattern}` must be relative to the repository root and stay inside it")]
    OutsideRepository { pattern: String },
}

/// Compiles a glob with `*` kept inside one path component, as in `.gitignore`.
pub(crate) fn compile(glob: &str) -> Result<Glob, PatternError> {
    GlobBuilder::new(glob)
        .literal_separator(true)
        .build()
        .map_err(|error| PatternError::NotAGlob {
            pattern: glob.to_owned(),
            reason: error.kind().to_string(),
        })
}
