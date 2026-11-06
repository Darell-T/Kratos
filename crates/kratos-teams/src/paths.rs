//! Paths relative to the repository root, and the globs that match them. Reviewers, agents, and git
//! each spell a path their own way, so every rule that compares paths reads them through here.

use globset::{Glob, GlobBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// A checked destination relative to the repository root, without parent traversal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct RepoPath(String);

impl RepoPath {
    /// Normalizes separators and refuses rooted paths and parent traversal.
    ///
    /// # Errors
    /// Returns an error when the path is empty, rooted, or contains `..`.
    pub fn new(raw: &str) -> Result<Self, InvalidRepoPath> {
        parse_repo_path(raw)
            .map(Self)
            .map_err(|_| InvalidRepoPath(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A destination that cannot be used as a repository-relative path.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("probe destination `{0}` must be relative to the repository root without parent traversal")]
pub struct InvalidRepoPath(String);

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

/// Reviewers write `src\a.rs` or `./src/a.rs`. Git names the same file `src/a.rs`.
pub fn repo_path(raw: &str) -> String {
    let mut path = raw.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_owned();
    }
    path
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathProblem {
    /// It is empty, or it starts at the root or at a drive.
    Rooted,
    Climbs,
}

pub(crate) fn parse_repo_path(raw: &str) -> Result<String, PathProblem> {
    let path = repo_path(raw);
    let bytes = path.as_bytes();
    let has_drive =
        bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':');
    // A path that went through repo_path has no place to start when it is empty, begins at the
    // root, or begins at a drive.
    if path.is_empty() || path.starts_with('/') || has_drive {
        return Err(PathProblem::Rooted);
    }
    // A path that went through repo_path climbs out of its folder with a `..`.
    if path.split('/').any(|part| part == "..") {
        return Err(PathProblem::Climbs);
    }
    Ok(path)
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
