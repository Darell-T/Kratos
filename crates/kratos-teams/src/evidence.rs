//! The evidence rules. They decide which findings Kratos believes before the implementer sees them.
//!
//! A reviewer's word is not evidence. A bug counts only when its command ran in the reviewer's worktree
//! and exited non-zero. A requirement or constraint counts only when its quote appears in a source the
//! task owns. A location counts only when it names a line that exists. Everything here is a pure
//! function of what the engine observed, and the engine does the reading and the running.
//!
//! The order of those rules is itself a rule, because a command must never run for a finding whose
//! location is made up. So the order is written once, here, as a two-call protocol: `start_check`
//! answers from what is already known or asks for one command, and `finish_check` judges that command's
//! result.

use std::time::Duration;

use kratos_protocol::CommandResult;
use serde::{Deserialize, Serialize};

use crate::reports::{FindingReport, Kind, Location};

/// How long the engine lets an evidence command run. A timeout proves nothing.
pub const EVIDENCE_TIMEOUT: Duration = Duration::from_secs(120);

/// A quote shorter than this cannot be told apart from a coincidence.
pub const MIN_QUOTE_LEN: usize = 12;

/// The tail of a command's output that the record keeps.
const OUTPUT_TAIL_CHARS: usize = 4000;

/// The git arguments every diff the rules parse must come from, followed by the revision range.
///
/// - `core.quotePath=false` keeps non-ASCII file names as written. Without it git writes each byte of
///   such a name as an octal escape.
/// - The two prefixes make the new side `b/`, whatever the user's `diff.noprefix` says.
/// - `--no-ext-diff` and `--no-textconv` keep a diff driver in a tree an agent wrote from running.
/// - `-U0` keeps the output small. The parser reads any amount of context.
pub const GIT_DIFF_ARGS: [&str; 9] = [
    "-c",
    "core.quotePath=false",
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "-U0",
];

/// Reviewers write `src\a.rs` or `./src/a.rs`. Git names the same file `src/a.rs`.
pub fn repo_path(raw: &str) -> String {
    let mut path = raw.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_owned();
    }
    path
}

/// Whether a path that went through `repo_path` has no place to start from. That means it is empty,
/// begins at the root, or begins at a drive.
pub(crate) fn is_rooted(path: &str) -> bool {
    let bytes = path.as_bytes();
    let has_drive =
        bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':');
    path.is_empty() || path.starts_with('/') || has_drive
}

/// Whether a path that went through `repo_path` climbs out of its folder with a `..`.
pub(crate) fn climbs(path: &str) -> bool {
    path.split('/').any(|part| part == "..")
}

// ---------------------------------------------------------------------------------------------
// Diffs.
// ---------------------------------------------------------------------------------------------

/// First and last line of a run of changed lines, counted in the new file. Both are included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub first: u32,
    pub last: u32,
}

/// One file's side of a parsed diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFile {
    /// The path as git names it, with forward slashes and relative to the repository root.
    pub path: String,
    /// New-side ranges of the changed lines. A pure deletion covers the lines on either side of it.
    pub ranges: Vec<LineRange>,
    /// The text of each added line, without its leading `+`.
    pub added: Vec<String>,
}

/// A unified diff, parsed. A deleted file does not appear, and a renamed or new file appears under its
/// new name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub files: Vec<DiffFile>,
}

impl Diff {
    /// Reads `git diff` output made with `GIT_DIFF_ARGS`.
    ///
    /// The parser counts every line of a hunk against the counts in its header, context lines
    /// included. So an added line that itself starts with `+++ ` is never taken for a file header, and
    /// a diff made with context parses to the same ranges as one made with `-U0`. It never fails:
    /// lines it does not know are skipped, and a hunk the text cuts short ends with the text.
    pub fn parse(text: &str) -> Diff {
        let mut walker = Walker::default();
        for line in text.lines() {
            walker.feed(line);
        }
        walker.finish()
    }

    /// Whether the line a finding cites was added or changed in this diff.
    pub fn touches(&self, location: &Location) -> bool {
        let path = repo_path(&location.path);
        self.files
            .iter()
            .filter(|file| file.path == path)
            .flat_map(|file| &file.ranges)
            .any(|range| (range.first..=range.last).contains(&location.line))
    }
}

#[derive(Default)]
struct Walker {
    files: Vec<DiffFile>,
    /// Whether the last `+++` header named a file, which is the file new hunks belong to.
    in_file: bool,
    hunk: Option<Hunk>,
}

/// A run of changed lines inside a hunk. A deletion has no new-side lines, so it remembers only that
/// it happened.
enum Block {
    None,
    Deleted,
    Added { first: u32 },
}

struct Hunk {
    old_left: u32,
    new_left: u32,
    next_new: u32,
    block: Block,
}

impl Walker {
    fn feed(&mut self, line: &str) {
        if self.hunk.is_some() {
            self.feed_hunk_line(line);
        } else if line.starts_with("diff ") {
            self.in_file = false;
        } else if let Some(header) = line.strip_prefix("+++ ") {
            self.in_file = self.start_file(header);
        } else if self.in_file {
            self.hunk = Hunk::open(line);
        }
    }

    fn start_file(&mut self, header: &str) -> bool {
        let Some(path) = diff_path(header) else {
            return false;
        };
        self.files.push(DiffFile {
            path,
            ranges: Vec::new(),
            added: Vec::new(),
        });
        true
    }

    fn feed_hunk_line(&mut self, line: &str) {
        let (Some(hunk), Some(file)) = (self.hunk.as_mut(), self.files.last_mut()) else {
            return;
        };
        match line.as_bytes().first() {
            Some(b'+') => {
                hunk.add();
                file.added.push(line[1..].to_owned());
            }
            Some(b'-') => hunk.remove(),
            // A blank line is a context line whose trailing space an editor stripped.
            Some(b' ') | None => hunk.context(&mut file.ranges),
            // "\ No newline at end of file" belongs to the line before it and counts for nothing.
            Some(_) => return,
        }
        if hunk.is_finished() {
            hunk.close_block(&mut file.ranges);
            self.hunk = None;
        }
    }

    fn finish(mut self) -> Diff {
        if let (Some(hunk), Some(file)) = (self.hunk.as_mut(), self.files.last_mut()) {
            hunk.close_block(&mut file.ranges);
        }
        Diff { files: self.files }
    }
}

impl Hunk {
    /// Reads `@@ -old[,count] +new[,count] @@`.
    fn open(line: &str) -> Option<Hunk> {
        let spec = line.strip_prefix("@@ -")?;
        let (old, rest) = spec.split_once(" +")?;
        let (new, _) = rest.split_once(" @@")?;
        let (_, old_count) = hunk_span(old)?;
        let (new_first, new_count) = hunk_span(new)?;
        // `+3,0` means "after line 3", so the first line this hunk can reach is the next one.
        let next_new = if new_count == 0 {
            new_first.saturating_add(1)
        } else {
            new_first
        };
        Some(Hunk {
            old_left: old_count,
            new_left: new_count,
            next_new,
            block: Block::None,
        })
    }

    fn is_finished(&self) -> bool {
        self.old_left == 0 && self.new_left == 0
    }

    fn add(&mut self) {
        if !matches!(self.block, Block::Added { .. }) {
            self.block = Block::Added {
                first: self.next_new,
            };
        }
        self.next_new = self.next_new.saturating_add(1);
        self.new_left = self.new_left.saturating_sub(1);
    }

    fn remove(&mut self) {
        if matches!(self.block, Block::None) {
            self.block = Block::Deleted;
        }
        self.old_left = self.old_left.saturating_sub(1);
    }

    fn context(&mut self, ranges: &mut Vec<LineRange>) {
        self.close_block(ranges);
        self.next_new = self.next_new.saturating_add(1);
        self.old_left = self.old_left.saturating_sub(1);
        self.new_left = self.new_left.saturating_sub(1);
    }

    fn close_block(&mut self, ranges: &mut Vec<LineRange>) {
        let range = match std::mem::replace(&mut self.block, Block::None) {
            Block::None => return,
            Block::Added { first } => LineRange {
                first,
                last: self.next_new.saturating_sub(1),
            },
            // A pure deletion covers the line before the gap and the line after it.
            Block::Deleted => LineRange {
                first: self.next_new.saturating_sub(1).max(1),
                last: self.next_new,
            },
        };
        ranges.push(range);
    }
}

/// `3,2` is two lines from line 3, and `3` is one line.
fn hunk_span(spec: &str) -> Option<(u32, u32)> {
    match spec.split_once(',') {
        Some((first, count)) => Some((first.parse().ok()?, count.parse().ok()?)),
        None => Some((spec.parse().ok()?, 1)),
    }
}

/// The new-side path of a `+++` header, or `None` for `/dev/null`. Git quotes a path that holds a
/// quote, a backslash, or a control character, and ends one that holds a space with a tab.
fn diff_path(header: &str) -> Option<String> {
    let raw = header.strip_suffix('\t').unwrap_or(header);
    if raw == "/dev/null" {
        return None;
    }
    let path = match raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        Some(quoted) => unquote(quoted),
        None => raw.to_owned(),
    };
    let without_prefix = path.strip_prefix("b/").map(str::to_owned);
    Some(without_prefix.unwrap_or(path))
}

/// Undoes git's C-style quoting. Octal escapes are bytes of a UTF-8 name, so they decode together.
fn unquote(quoted: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(quoted.len());
    let mut rest = quoted.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        rest = tail;
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let Some((&escape, tail)) = rest.split_first() else {
            bytes.push(b'\\');
            break;
        };
        rest = tail;
        match escape {
            b'0'..=b'3' if rest.len() >= 2 => {
                let digits = [escape, rest[0], rest[1]];
                match std::str::from_utf8(&digits)
                    .ok()
                    .and_then(|text| u8::from_str_radix(text, 8).ok())
                {
                    Some(value) => {
                        bytes.push(value);
                        rest = &rest[2..];
                    }
                    None => bytes.extend([b'\\', escape]),
                }
            }
            b'a' => bytes.push(0x07),
            b'b' => bytes.push(0x08),
            b't' => bytes.push(b'\t'),
            b'n' => bytes.push(b'\n'),
            b'v' => bytes.push(0x0b),
            b'f' => bytes.push(0x0c),
            b'r' => bytes.push(b'\r'),
            other => bytes.push(other),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

// ---------------------------------------------------------------------------------------------
// Location, quote, and command rules.
// ---------------------------------------------------------------------------------------------

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

/// `None` when the location names a real line of a file in the reviewed commit. `file_text` is the file
/// at that commit, or `None` if it does not exist there.
pub fn location_problem(location: &Location, file_text: Option<&str>) -> Option<LocationProblem> {
    let path = repo_path(&location.path);
    if is_rooted(&path) {
        return Some(LocationProblem::NotRelative(location.path.clone()));
    }
    if climbs(&path) {
        return Some(LocationProblem::LeavesRepository(location.path.clone()));
    }
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

/// Whether `quote` is long enough to mean something and appears in a source, ignoring how whitespace
/// wraps. Case matters.
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

// ---------------------------------------------------------------------------------------------
// What Kratos records about each finding.
// ---------------------------------------------------------------------------------------------

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
    /// `inside_new_diff` is false for a real problem in lines this round did not change. It was missed
    /// earlier, so the user reads it and no round starts for it. `output` is the failing command's
    /// tail, which becomes the probe's "output that showed the bug".
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

// ---------------------------------------------------------------------------------------------
// The check protocol.
// ---------------------------------------------------------------------------------------------

/// What the engine has already observed for one finding.
pub struct Seen<'a> {
    /// The cited file at the reviewed commit, or `None` if it does not exist there.
    pub file_text: Option<&'a str>,
    /// Every text a requirement quote may come from. `CheckStep::sources` supplies them. They are the
    /// task, the user's constraints and directions, the approved plan, and the three rules files.
    pub sources: &'a [&'a str],
    /// The diff since the last standard review. `None` in the first standard review and in the other
    /// passes, where every confirmed finding counts as inside the diff.
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
    diff.is_none_or(|diff| diff.touches(location))
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

/// The second half. A command that exited non-zero confirms the bug. A pass, a timeout, or a refusal
/// drops the finding with that reason.
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
