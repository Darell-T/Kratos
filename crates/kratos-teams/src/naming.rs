//! The names Kratos writes from the task for git: the task title, the commit subject, and the branch
//! name. They come from the user's request and Kratos's own counts, never from agent text.

use kratos_protocol::Provider;

const TITLE_LIMIT: usize = 60;

/// The task's first line, with headings and extra whitespace stripped, at most 60 characters. Kratos
/// names the task's commits with it, so no agent text reaches a commit. `"Task"` for a blank request.
pub fn task_title(text: &str) -> String {
    let first = text
        .lines()
        .map(|line| {
            // A control character in a title would reach the commit subject and the terminal.
            let printable: String = line
                .chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect();
            printable
                .trim_start_matches('#')
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| "Task".to_owned());
    if first.chars().count() <= TITLE_LIMIT {
        return first;
    }
    let cut: String = first.chars().take(TITLE_LIMIT - 3).collect();
    format!("{}...", cut.trim_end())
}

pub fn commit_subject(title: &str, round: u32) -> String {
    format!("{title} (round {round})")
}

const SLUG_LIMIT: usize = 40;

/// `codex/add-login`: the implementer's provider, a slash, and a slug of the title (D36). `taken` is the
/// repository's existing branch names, and a clash gets `-2`, `-3`, and so on.
pub fn branch_name(provider: Provider, title: &str, taken: &[String]) -> String {
    let mut slug = String::new();
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.chars().take(SLUG_LIMIT).collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "task" } else { slug };
    let base = format!("{}/{slug}", provider.as_str());
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or(base)
}
