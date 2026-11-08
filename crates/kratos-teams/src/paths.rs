//! Paths relative to the repository root, and the globs that match them. Reviewers, agents, and git
//! each spell a path their own way, so every rule that compares paths reads them through here.

use globset::{Glob, GlobBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A checked path relative to the repository root, without parent traversal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct RepoPath(String);

impl RepoPath {
    /// Normalizes separators and refuses rooted paths and parent traversal.
    ///
    /// # Errors
    /// Returns an error for an empty, rooted, climbing, or unusable path.
    pub fn new(raw: &str) -> Result<Self, RepoPathError> {
        let path = repo_path(raw);
        let bytes = path.as_bytes();
        let has_drive =
            bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':');
        if path.is_empty() || path.starts_with('/') || has_drive {
            return Err(RepoPathError::Rooted(raw.to_owned()));
        }
        if path.split('/').any(|part| part == "..") {
            return Err(RepoPathError::Climbs(raw.to_owned()));
        }
        if path.bytes().any(|byte| byte.is_ascii_control())
            || path
                .split('/')
                .any(|part| matches!(part, "" | ".") || part.ends_with(['.', ' ']))
        {
            return Err(RepoPathError::InvalidComponent(raw.to_owned()));
        }
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why text cannot name a path inside the repository.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RepoPathError {
    #[error("{0:?} is not relative to the repository root.")]
    Rooted(String),
    #[error("{0:?} climbs out of the repository.")]
    Climbs(String),
    #[error("{0:?} has an invalid repository path component.")]
    InvalidComponent(String),
}

impl TryFrom<String> for RepoPath {
    type Error = RepoPathError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(&raw)
    }
}

impl From<RepoPath> for String {
    fn from(path: RepoPath) -> Self {
        path.0
    }
}

/// Reviewers write `src\a.rs` or `./src/a.rs`. Git names the same file `src/a.rs`.
pub(crate) fn repo_path(raw: &str) -> String {
    let mut path = raw.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_owned();
    }
    path
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    #[error("`{pattern}` is not a valid glob: {reason}")]
    NotAGlob { pattern: String, reason: String },
    #[error(transparent)]
    InvalidPath(#[from] RepoPathError),
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
