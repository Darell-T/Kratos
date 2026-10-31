//! `git diff` output, parsed into the files a change touched, the lines it changed, and the lines it
//! added. The evidence rules read the changed lines, and the security triggers read the files and the
//! added lines.

use serde::{Deserialize, Serialize};

/// The git arguments for every diff the engine makes for the rules, followed by the revision range.
/// That covers each diff a check or a scan parses and the one `Committed::size` measures.
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

    /// Whether `line` of `path`, spelled the way git spells it, was added or changed in this diff.
    pub fn touches(&self, path: &str, line: u32) -> bool {
        self.files
            .iter()
            .filter(|file| file.path == path)
            .flat_map(|file| &file.ranges)
            .any(|range| (range.first..=range.last).contains(&line))
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
