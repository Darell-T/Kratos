//! Security triggers are path globs and code patterns that send a change to a security review. Matching is
//! plain text work on the diff, with no model call.
//!
//! A match selects review, and no match is not proof of safety. The patterns are a cheap filter that
//! keeps the security review off changes that obviously need none. The reviewer's standing rules still
//! apply to everything.
//!
//! Matching leans toward review. A pattern that exceeds the regex engine's backtracking budget on a
//! line counts as a hit, because a line built to defeat the filter is the line a person should read.

use fancy_regex::RegexBuilder;
use globset::GlobMatcher;
use serde::{Deserialize, Serialize};

use crate::evidence::DiffFile;
use crate::task_plan::compile;

/// One trigger, as parsed from `REVIEW.md` or taken from the defaults. It is stored as text in the
/// task's first fact and compiled each time it is matched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// A glob matched against the paths the task changed.
    Path { glob: String },
    /// A regular expression in fancy-regex syntax, so look-behind works, matched against added lines.
    Code { pattern: String },
}

/// Why a trigger fired, with enough for the security reviewer to see what to read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerHit {
    Path {
        glob: String,
        path: String,
    },
    /// `line` is the first added line that matched, trimmed and cut to `MAX_HIT_LINE` characters.
    Code {
        pattern: String,
        path: String,
        line: String,
    },
}

/// The most hits one scan reports. More would only make the security prompt longer.
pub const MAX_HITS: usize = 50;
pub const MAX_HIT_LINE: usize = 200;
/// Only the first characters of a very long added line are matched, such as a minified file.
pub const MAX_SCANNED_LINE: usize = 2_000;
/// The regex engine's backtracking budget for one line.
pub const BACKTRACK_LIMIT: usize = 100_000;

/// The path triggers used when `REVIEW.md` has no `kratos-security` block. Build scripts and cargo
/// configuration run code at build time, so they count.
pub const DEFAULT_PATHS: [&str; 6] = [
    "**/*{sandbox,Sandbox,permission,Permission,approval,Approval,credential,Credential}*",
    "**/.env",
    "**/.env.*",
    "**/build.rs",
    "**/.cargo/config",
    "**/.cargo/config.toml",
];

/// The code triggers used without a block. They cover processes, network, filesystem changes,
/// credentials, and permissions in JavaScript, TypeScript, Python, and Rust projects.
/// `tests/teams/security_triggers.rs` holds a fixture for each pattern and benign lines beside it.
pub const DEFAULT_PATTERNS: [&str; 7] = [
    // Processes in JavaScript and Python, then in Rust. `(?<!::)` keeps `tokio::spawn(`, which starts a
    // task, out of the first pattern.
    r"child_process|\bexeca\b|(?<!::)\bspawn(?:Sync)?\(|\bexecFile(?:Sync)?\(|\bexecSync\(|(?<!\.)\bexec\(|\bsubprocess\.|\bos\.system\(",
    r"\bCommand::new\(|\b(?:std|tokio)::process::(?:Command|Child|Stdio)\b|\btokio::process\b",
    // Network.
    r"\bfetch\(|\bhttps?\.(?:request|get)\(|\bnew WebSocket\(|\bnet\.(?:connect|createConnection)\(|\bcreateServer\(|\baxios\b|XMLHttpRequest",
    r"\b(?:reqwest|hyper|ureq|isahc|surf|tungstenite)::|\bTcpStream::connect\b|\bTcpListener::bind\b|\bUdpSocket::bind\b|\btokio::net::|\bstd::net::(?:Tcp|Udp)|\bconnect_async\(|\baxum::serve\b",
    // Filesystem changes in Rust. Reads are not triggers.
    r"\bfs::(?:remove_dir_all|remove_file|remove_dir|write|rename|copy|create_dir_all|hard_link)\b|\btokio::fs::(?:remove|write|rename|copy|create)|\bOpenOptions::new\(|\bFile::create\(|\bsymlink(?:_file|_dir)?\(",
    // Credentials.
    r"(?i:password|passwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token|private[_-]?key|credential)",
    // Permissions and sandboxes.
    r"(?i:permission|sandbox|canUseTool|approval|chmod|setuid|\bsudo\b)",
];

pub fn default_triggers() -> Vec<Trigger> {
    let paths = DEFAULT_PATHS.iter().map(|glob| Trigger::Path {
        glob: (*glob).to_owned(),
    });
    let patterns = DEFAULT_PATTERNS.iter().map(|pattern| Trigger::Code {
        pattern: (*pattern).to_owned(),
    });
    paths.chain(patterns).collect()
}

/// Why a trigger line or a stored trigger is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TriggerError {
    /// A line of the `kratos-security` block. The task refuses to start and names the line.
    #[error("REVIEW.md line {number}: {problem}: {text}")]
    BadLine {
        number: usize,
        problem: LineProblem,
        text: String,
    },
    /// A stored trigger that no longer compiles. Possible only if the regex engine changed under an
    /// old task.
    #[error("trigger `{trigger}` no longer compiles: {reason}")]
    Stale { trigger: String, reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LineProblem {
    #[error(r#"expected "path: <glob>" or "code: <regular expression>""#)]
    Unrecognized,
    #[error("the trigger is empty")]
    Empty,
    #[error("not a valid glob")]
    NotAGlob,
    #[error("not a valid regular expression")]
    NotARegex,
}

/// Reads the fenced `kratos-security` block of `REVIEW.md`: one `path: <glob>` or
/// `code: <regular expression>` per line, `#` comments, and blank lines ignored.
///
/// No block, or no `REVIEW.md`, gives the defaults. An empty block gives no triggers, which turns the
/// security pass off for the project. Other fenced blocks are ignored.
pub fn parse_triggers(review_md: Option<&str>) -> Result<Vec<Trigger>, TriggerError> {
    let lines: Vec<&str> = review_md.unwrap_or_default().lines().collect();
    let Some(open) = lines.iter().position(|line| fence_opening(line).is_some()) else {
        return Ok(default_triggers());
    };
    let fence = fence_opening(lines[open]).unwrap_or_default();
    let body = lines[open + 1..]
        .iter()
        .take_while(|line| !closes_fence(line, fence));
    let mut triggers = Vec::new();
    for (index, line) in body.enumerate() {
        let text = line.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let number = open + 2 + index;
        let trigger = parse_line(text).map_err(|problem| TriggerError::BadLine {
            number,
            problem,
            text: text.to_owned(),
        })?;
        triggers.push(trigger);
    }
    Ok(triggers)
}

/// The fence marker (three or more backticks or tildes) of a line that opens a `kratos-security` block.
fn fence_opening(line: &str) -> Option<&str> {
    let unindented = line.trim_start_matches(' ');
    if line.len() - unindented.len() > 3 {
        return None;
    }
    let marker_char = unindented
        .chars()
        .next()
        .filter(|c| matches!(c, '`' | '~'))?;
    let marker_len = unindented.chars().take_while(|c| *c == marker_char).count();
    let (marker, info) = unindented.split_at(marker_len);
    (marker_len >= 3 && info.trim() == "kratos-security").then_some(marker)
}

fn closes_fence(line: &str, fence: &str) -> bool {
    let unindented = line.trim_start_matches(' ');
    let Some(marker_char) = fence.chars().next() else {
        return false;
    };
    let run = unindented.chars().take_while(|c| *c == marker_char).count();
    line.len() - unindented.len() <= 3 && run >= fence.len() && unindented[run..].trim().is_empty()
}

fn parse_line(line: &str) -> Result<Trigger, LineProblem> {
    let (kind, value) = line.split_once(':').ok_or(LineProblem::Unrecognized)?;
    let value = value.trim();
    if kind != "path" && kind != "code" {
        return Err(LineProblem::Unrecognized);
    }
    if value.is_empty() {
        return Err(LineProblem::Empty);
    }
    if kind == "path" {
        compile(value).map_err(|_| LineProblem::NotAGlob)?;
        return Ok(Trigger::Path {
            glob: value.to_owned(),
        });
    }
    RegexBuilder::new(value)
        .build()
        .map_err(|_| LineProblem::NotARegex)?;
    Ok(Trigger::Code {
        pattern: value.to_owned(),
    })
}

enum Compiled {
    Path {
        glob: String,
        matcher: GlobMatcher,
    },
    Code {
        pattern: String,
        regex: fancy_regex::Regex,
    },
}

fn compile_trigger(trigger: &Trigger) -> Result<Compiled, TriggerError> {
    let stale = |reason: String| TriggerError::Stale {
        trigger: match trigger {
            Trigger::Path { glob } => glob.clone(),
            Trigger::Code { pattern } => pattern.clone(),
        },
        reason,
    };
    match trigger {
        Trigger::Path { glob } => Ok(Compiled::Path {
            glob: glob.clone(),
            matcher: compile(glob)
                .map_err(|error| stale(error.to_string()))?
                .compile_matcher(),
        }),
        Trigger::Code { pattern } => Ok(Compiled::Code {
            pattern: pattern.clone(),
            regex: RegexBuilder::new(pattern)
                .backtrack_limit(BACKTRACK_LIMIT)
                .build()
                .map_err(|error| stale(error.to_string()))?,
        }),
    }
}

/// Path globs match changed files, and code patterns match added lines. A scan reports at most
/// `MAX_HITS` hits, in file order and then trigger order.
pub fn match_triggers(
    triggers: &[Trigger],
    files: &[DiffFile],
) -> Result<Vec<TriggerHit>, TriggerError> {
    let compiled = triggers
        .iter()
        .map(compile_trigger)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(files
        .iter()
        .flat_map(|file| {
            compiled
                .iter()
                .filter_map(move |trigger| hit(trigger, file))
        })
        .take(MAX_HITS)
        .collect())
}

fn hit(trigger: &Compiled, file: &DiffFile) -> Option<TriggerHit> {
    match trigger {
        Compiled::Path { glob, matcher } => {
            matcher.is_match(&file.path).then(|| TriggerHit::Path {
                glob: glob.clone(),
                path: file.path.clone(),
            })
        }
        Compiled::Code { pattern, regex } => {
            let line = file.added.iter().find(|added| matches_line(regex, added))?;
            Some(TriggerHit::Code {
                pattern: pattern.clone(),
                path: file.path.clone(),
                line: line.trim().chars().take(MAX_HIT_LINE).collect(),
            })
        }
    }
}

/// An error here is the backtracking budget running out, which counts as a match.
fn matches_line(regex: &fancy_regex::Regex, line: &str) -> bool {
    let scanned_end = line
        .char_indices()
        .nth(MAX_SCANNED_LINE)
        .map_or(line.len(), |(index, _)| index);
    regex.is_match(&line[..scanned_end]).unwrap_or(true)
}
