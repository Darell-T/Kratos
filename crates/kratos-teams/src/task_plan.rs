//! The task plan is the one document a planner writes (or the implementer, in a two-member Team with
//! planning on), the user approves, and the reviewer judges the work against.
//!
//! "Plan" means only this document. The function that picks what happens next is `next_step`.
//!
//! The plan fixes the agreed scope of the work. Scope is advice to the reviewer, not a wall: Kratos
//! lists the changed files outside it in the review package, because lockfiles and generated files
//! change as side effects, and the reviewer judges them. An implementer who needs more scope asks the
//! user with a plan-change request (D30, D32).

use std::fmt;

use globset::GlobMatcher;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::paths::{PatternError, compile, parse_repo_path, repo_path};

/// A glob relative to the repository root that names files a task may change: `src/auth/**`,
/// `docs/*.md`, `Cargo.toml`. A folder needs an explicit `/**`, because Kratos cannot tell a file from a
/// folder without reading the disk. `*` stays inside one folder and `**` crosses folders.
///
/// Checked when parsed, so every `ScopePattern` in a fact or a view is a valid glob that stays inside
/// the repository.
#[derive(Clone)]
pub struct ScopePattern {
    text: String,
    matcher: GlobMatcher,
}

impl ScopePattern {
    pub fn new(glob: &str) -> Result<ScopePattern, PatternError> {
        let text = parse_repo_path(glob.trim()).map_err(|_| PatternError::OutsideRepository {
            pattern: glob.to_owned(),
        })?;
        let matcher = compile(&text)?.compile_matcher();
        Ok(ScopePattern { text, matcher })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether `path`, written as a reviewer or git writes it, is covered.
    pub fn covers(&self, path: &str) -> bool {
        self.matcher.is_match(repo_path(path))
    }
}

impl fmt::Debug for ScopePattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ScopePattern({:?})", self.text)
    }
}

impl PartialEq for ScopePattern {
    fn eq(&self, other: &ScopePattern) -> bool {
        self.text == other.text
    }
}

impl Eq for ScopePattern {}

impl Serialize for ScopePattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for ScopePattern {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<ScopePattern, D::Error> {
        let text = String::deserialize(deserializer)?;
        ScopePattern::new(&text).map_err(serde::de::Error::custom)
    }
}

/// What to build, what done means, and which files are in scope. One shape, whoever writes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPlan {
    pub build: String,
    /// Checkable statements. A quote from here counts as requirement evidence (D30).
    pub done_means: Vec<String>,
    /// Never empty. A plan that covers every file says `**`, on purpose.
    pub scope: Vec<ScopePattern>,
}

/// Why a plan is sent back to its author. The text is written for the author.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanProblem {
    #[error("build: must say what to build")]
    NothingToBuild,
    #[error("done_means: must hold at least one checkable statement")]
    NoDoneMeans,
    #[error("done_means: a statement is blank")]
    BlankDoneMeans,
    #[error("files_in_scope: must name the files in scope, or `**` when every file is")]
    NoScope,
}

impl TaskPlan {
    /// The only way an agent's words become a plan.
    pub fn new(
        build: String,
        done_means: Vec<String>,
        scope: Vec<ScopePattern>,
    ) -> Result<TaskPlan, PlanProblem> {
        if crate::is_blank(&build) {
            return Err(PlanProblem::NothingToBuild);
        }
        if done_means.is_empty() {
            return Err(PlanProblem::NoDoneMeans);
        }
        if done_means
            .iter()
            .any(|statement| crate::is_blank(statement))
        {
            return Err(PlanProblem::BlankDoneMeans);
        }
        if scope.is_empty() {
            return Err(PlanProblem::NoScope);
        }
        Ok(TaskPlan {
            build,
            done_means,
            scope,
        })
    }

    /// Every text a requirement quote may come from.
    pub fn quotable(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.build.as_str()).chain(self.done_means.iter().map(String::as_str))
    }
}

/// A request to change the agreed plan, most often to touch more files. It always goes to the user,
/// even in a Team with a planner (D32).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanChangeRequest {
    pub reason: String,
    /// Scope the requester wants added. Empty when the change is not about files.
    pub extra_scope: Vec<ScopePattern>,
}

/// A plan change the user allowed. The plan stays as drafted, and amendments layer on it, so the
/// record shows what was agreed and what was added later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Amendment {
    pub reason: String,
    pub extra_scope: Vec<ScopePattern>,
    pub instructions: Option<String>,
}

/// What the user allowed when they handed probe retention to the implementer (D78). It is recorded
/// when the task starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeConstraints {
    /// Where a kept probe may live, as repository-relative globs. Nothing outside them is kept.
    pub test_paths: Vec<ScopePattern>,
    /// The user's own words, shown to the implementer unchanged.
    pub note: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeSource {
    ApprovedPlan,
    /// There is no plan, so this is the scope the user gave when starting the task.
    TaskRequest,
}

/// The scope in force right now.
///
/// With an approved plan it is the plan's scope plus every amendment. Without one it is the scope the
/// user gave at the start. If the user gave none, the whole worktree is in scope, so nothing is
/// reported as outside it. A missing plan is neither unlimited authority nor a reason to reject
/// every change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Scope {
    pub source: ScopeSource,
    pub patterns: Vec<ScopePattern>,
}

impl Scope {
    /// Whether `path` is in scope. With no patterns, every path is.
    pub fn covers(&self, path: &str) -> bool {
        self.patterns.is_empty() || self.patterns.iter().any(|pattern| pattern.covers(path))
    }

    /// The changed files no pattern covers, in the order given and without repeats.
    pub fn outside<'a>(&self, files: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut outside: Vec<String> = Vec::new();
        for file in files {
            let path = repo_path(file);
            if !self.covers(&path) && !outside.contains(&path) {
                outside.push(path);
            }
        }
        outside
    }
}
