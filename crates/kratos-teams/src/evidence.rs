//! The evidence rules. They decide which findings Kratos believes before the implementer sees them.
//!
//! A reviewer's word is not evidence. A bug counts only when its command ran in the reviewer's
//! worktree and exited non-zero. A requirement or constraint counts only when its quote appears in
//! a source the task owns. A location counts only when it names a line that exists. Everything here
//! is a pure function of what the engine observed, and the engine does the reading and the running.
//!
//! The order of those rules is itself a rule, because a command must never run for a finding whose
//! location is made up. So the order is written once, here, as a two-call protocol: `start_check`
//! answers from what is already known or asks for one command, and `finish_check` judges that
//! command's result.

use std::time::Duration;

use kratos_protocol::CommandResult;
use serde::{Deserialize, Serialize};

use crate::diff::Diff;
use crate::paths::{PathProblem, parse_repo_path, repo_path};
use crate::reports::{FindingReport, Kind, Location};

/// How long the engine lets an evidence command run. A timeout proves nothing.
pub const EVIDENCE_TIMEOUT: Duration = Duration::from_secs(120);

/// A quote shorter than this cannot be told apart from a coincidence.
pub const MIN_QUOTE_LEN: usize = 12;

/// The tail of a command's output that the record keeps.
const OUTPUT_TAIL_CHARS: usize = 4000;

// Location, quote, and command rules.

/// Why a finding's location is refused. The text is written for the reviewer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LocationProblem {
    #[error("{0} is not a path relative to the repository root.")]
    NotRelative(String),
    #[error("{0} leaves the repository.")]
    LeavesRepository(String),
    #[error("{0} does not exist in the reviewed commit.")]
    NoSuchFile(String),
    #[error("{path} has no line {line}. It has {lines}.")]
    NoSuchLine { path: String, line: u32, lines: u32 },
}

/// `None` when the location names a real line of a file in the reviewed commit. `file_text` is the
/// file at that commit, or `None` if it does not exist there.
pub fn location_problem(location: &Location, file_text: Option<&str>) -> Option<LocationProblem> {
    let path = match parse_repo_path(&location.path) {
        Ok(path) => path,
        Err(PathProblem::Rooted) => {
            return Some(LocationProblem::NotRelative(location.path.clone()));
        }
        Err(PathProblem::Climbs) => {
            return Some(LocationProblem::LeavesRepository(location.path.clone()));
        }
    };
    let Some(text) = file_text else {
        return Some(LocationProblem::NoSuchFile(path));
    };
    let lines = line_count(text);
    (location.line < 1 || location.line > lines).then_some(LocationProblem::NoSuchLine {
        path,
        line: location.line,
        lines,
    })
}

/// A final newline ends the last line. It does not start another.
fn line_count(text: &str) -> u32 {
    let breaks = text.matches('\n').count();
    let lines = if text.is_empty() || text.ends_with('\n') {
        breaks
    } else {
        breaks + 1
    };
    u32::try_from(lines).unwrap_or(u32::MAX)
}

/// Whether `quote` is long enough to mean something and appears in a source, ignoring how
/// whitespace wraps. Case matters.
pub fn quote_appears(quote: &str, sources: &[&str]) -> bool {
    let wanted = collapse_whitespace(quote);
    wanted.chars().count() >= MIN_QUOTE_LEN
        && sources
            .iter()
            .any(|source| collapse_whitespace(source).contains(&wanted))
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a finished evidence command shows about a bug.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandJudgment {
    Fails {
        output: String,
    },
    Passes {
        output: String,
    },
    /// A timeout or a refusal. Neither shows a bug, and neither shows its absence.
    Inconclusive {
        why: DropWhy,
        output: String,
    },
}

/// Only a command that ran and exited non-zero shows a bug. `output` is the tail of stdout then
/// stderr, cut so that a noisy command cannot flood the record.
pub fn judge_command(result: &CommandResult) -> CommandJudgment {
    match result {
        CommandResult::Exited {
            exit_code,
            stdout,
            stderr,
        } => {
            let both: Vec<&str> = [stdout.as_str(), stderr.as_str()]
                .into_iter()
                .filter(|text| !text.is_empty())
                .collect();
            let output = tail(&both.join("\n"));
            if *exit_code == 0 {
                CommandJudgment::Passes { output }
            } else {
                CommandJudgment::Fails { output }
            }
        }
        CommandResult::TimedOut { output } => CommandJudgment::Inconclusive {
            why: DropWhy::CommandTimedOut,
            output: tail(output),
        },
        CommandResult::Refused { error } => CommandJudgment::Inconclusive {
            why: DropWhy::CommandRefused,
            output: tail(error),
        },
    }
}

fn tail(text: &str) -> String {
    let skipped = text.chars().count().saturating_sub(OUTPUT_TAIL_CHARS);
    text.chars().skip(skipped).collect()
}

// What Kratos records about each finding.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropWhy {
    Location,
    Quote,
    CommandPassed,
    CommandTimedOut,
    CommandRefused,
}

impl DropWhy {
    /// Plain words for the record and for the reviewer.
    pub fn explanation(self) -> &'static str {
        match self {
            DropWhy::Location => "the location does not exist",
            DropWhy::Quote => "the quote is not in the task or the project rules",
            DropWhy::CommandPassed => "the command exited 0",
            DropWhy::CommandTimedOut => "the command timed out",
            DropWhy::CommandRefused => "the sandbox refused the command",
        }
    }
}

/// Why a finding never reached the implementer. It stays in the record, and the user can read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropReason {
    pub why: DropWhy,
    pub detail: String,
}

/// The result of checking one finding that could block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceCheck {
    /// `inside_new_diff` is false for a real problem in lines this round did not change. It was
    /// missed earlier, so the user reads it and no round starts for it. `output` is the failing
    /// command's tail, which becomes the probe's "output that showed the bug".
    Confirmed {
        inside_new_diff: bool,
        output: Option<String>,
    },
    Dropped {
        reason: DropReason,
    },
}

/// Kratos's own rerun of an open bug's command on a new commit. The exit code decides, not the
/// reviewer's word.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rerun {
    pub verdict: RerunVerdict,
    pub output: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RerunVerdict {
    Fails,
    Passes,
    /// The command could not run. Only then does the reviewer's `still_open` decide.
    Inconclusive,
}

pub fn rerun(result: &CommandResult) -> Rerun {
    let (verdict, output) = match judge_command(result) {
        CommandJudgment::Fails { output } => (RerunVerdict::Fails, output),
        CommandJudgment::Passes { output } => (RerunVerdict::Passes, output),
        CommandJudgment::Inconclusive { output, .. } => (RerunVerdict::Inconclusive, output),
    };
    Rerun { verdict, output }
}

/// A reviewer's untracked file, copied into the record before anything can delete it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeCapture {
    pub path: String,
    /// `None` for a binary file, or once the review's probe budget is spent.
    pub content: Option<String>,
    pub truncated: bool,
}

// The check protocol.

/// What the engine has already observed for one finding.
pub struct Seen<'a> {
    /// The cited file at the reviewed commit, or `None` if it does not exist there.
    pub file_text: Option<&'a str>,
    /// Every text a requirement quote may come from. `CheckStep::sources` supplies them. They are
    /// the task, the user's constraints and directions, the approved plan, and the three rules
    /// files.
    pub sources: &'a [&'a str],
    /// The diff since the last standard review. `None` in the first standard review and in the
    /// other passes, where every confirmed finding counts as inside the diff.
    pub diff: Option<&'a Diff>,
}

/// The first half of a check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    /// Decided without running anything, from a bad location or a quote.
    Done(EvidenceCheck),
    /// A bug whose location is real. The engine runs this command in the reviewer's reset worktree
    /// within `EVIDENCE_TIMEOUT` and calls `finish_check` with the result.
    RunCommand(String),
    /// A nit, a pre-existing problem, or a design opinion. It is listed and never checked.
    NothingToCheck,
}

fn dropped(why: DropWhy, detail: impl Into<String>) -> EvidenceCheck {
    EvidenceCheck::Dropped {
        reason: DropReason {
            why,
            detail: detail.into(),
        },
    }
}

fn inside_new_diff(location: &Location, diff: Option<&Diff>) -> bool {
    diff.is_none_or(|diff| diff.touches(&repo_path(&location.path), location.line))
}

/// Applies the location rule, then the quote rule for a requirement or constraint, or asks for the
/// command of a bug.
pub fn start_check(finding: &FindingReport, seen: &Seen<'_>) -> Start {
    if !finding.can_block() {
        return Start::NothingToCheck;
    }
    if let Some(problem) = location_problem(&finding.location, seen.file_text) {
        return Start::Done(dropped(DropWhy::Location, problem.to_string()));
    }
    if finding.kind == Kind::Bug {
        return Start::RunCommand(finding.evidence.clone());
    }
    Start::Done(if quote_appears(&finding.evidence, seen.sources) {
        EvidenceCheck::Confirmed {
            inside_new_diff: inside_new_diff(&finding.location, seen.diff),
            output: None,
        }
    } else {
        dropped(DropWhy::Quote, finding.evidence.clone())
    })
}

/// The second half. A command that exited non-zero confirms the bug. A pass, a timeout, or a
/// refusal drops the finding with that reason.
pub fn finish_check(
    finding: &FindingReport,
    result: &CommandResult,
    diff: Option<&Diff>,
) -> EvidenceCheck {
    match judge_command(result) {
        CommandJudgment::Fails { output } => EvidenceCheck::Confirmed {
            inside_new_diff: inside_new_diff(&finding.location, diff),
            output: Some(output),
        },
        CommandJudgment::Passes { .. } => dropped(DropWhy::CommandPassed, finding.evidence.clone()),
        CommandJudgment::Inconclusive { why, .. } => dropped(why, finding.evidence.clone()),
    }
}
