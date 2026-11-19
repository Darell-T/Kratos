//! The file a Claude path names. The CLI expands `~` and, on Windows, `/c/...`, `/mnt/c/...`, and
//! `/cygdrive/c/...` into drive paths, and it trims the text first, before it touches the disk. A
//! rule that judged the text as written would judge a different file than the one the CLI writes.

use std::path::{Path, PathBuf};

/// The file `raw` names for a tool running in `base`, always an absolute path, or `None` when the
/// CLI may resolve it to a place Kratos cannot name: `~other`, `~` without a known home folder, or
/// a Windows drive-relative path such as `C:..\x`, which names a file relative to that drive's
/// current folder and not to `base`.
pub(super) fn expand(raw: &str, base: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let text = raw.trim_matches(trimmed);
    let named = if let Some(rest) = text.strip_prefix('~') {
        if !rest.is_empty() && !rest.starts_with(['/', '\\']) {
            return None;
        }
        home?.join(rest.trim_start_matches(['/', '\\']))
    } else {
        match windows_drive(text) {
            Some(drive) => PathBuf::from(drive),
            None => base.join(text),
        }
    };
    named.is_absolute().then_some(named)
}

/// The characters trimmed from a path's ends: Rust's whitespace and JavaScript's, which also counts
/// U+FEFF. Trimming both judges a name at least as near the home folder as the one the CLI opens.
fn trimmed(character: char) -> bool {
    character.is_whitespace() || character == '\u{feff}'
}

/// `/c/dir`, `/mnt/c/dir`, and `/cygdrive/c/dir` as the `C:\dir` the CLI opens, on Windows only.
fn windows_drive(text: &str) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let unified = text.replace('\\', "/");
    let letter_and_rest = ["/proc/cygdrive/", "/cygdrive/", "/mnt/", "/"]
        .iter()
        .find_map(|mount| unified.strip_prefix(mount))?;
    let mut characters = letter_and_rest.chars();
    let drive = characters.next().filter(char::is_ascii_alphabetic)?;
    let rest = characters.as_str();
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    Some(format!(
        "{}:{}",
        drive.to_ascii_uppercase(),
        if rest.is_empty() { "/" } else { rest }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            r"C:\work\proj"
        } else {
            "/work/proj"
        })
    }

    #[test]
    fn tilde_names_the_home_folder_and_a_relative_name_stays_in_the_workspace() {
        // Catches `~` joined to the workspace as a folder name, and a plain name leaving the workspace.
        let home = base().join("h");
        for raw in ["~/a/b", "  ~/a/b  ", "\u{feff}~/a/b"] {
            assert_eq!(
                expand(raw, &base(), Some(&home)),
                Some(home.join("a").join("b")),
                "{raw:?}"
            );
        }
        let backslashed = if cfg!(windows) {
            home.join("a").join("b")
        } else {
            home.join(r"a\b")
        };
        assert_eq!(expand(r"~\a\b", &base(), Some(&home)), Some(backslashed));
        assert_eq!(expand("~", &base(), Some(&home)), Some(home.join("")));
        assert_eq!(
            expand("a/b", &base(), Some(&home)),
            Some(base().join("a/b"))
        );
    }

    #[test]
    fn a_tilde_the_cli_may_resolve_elsewhere_names_no_file() {
        // Catches `~other` or `~` without a home folder being judged as a workspace file.
        assert_eq!(expand("~other/x", &base(), Some(&base())), None);
        assert_eq!(expand("~/x", &base(), None), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_relative_path_names_no_file() {
        let home = base().join("h");
        for raw in [r"C:..\..\.kratos\client-token", "C:x", "~/C:x"] {
            assert_eq!(expand(raw, &base(), Some(&home)), None, "{raw:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn unix_style_drive_paths_become_drive_paths() {
        // Catches `/c/Users` being joined to the workspace's drive as `C:\c\Users`.
        let expected = Some(PathBuf::from("C:/Users/x"));
        for raw in [
            "/c/Users/x",
            "/C/Users/x",
            "/mnt/c/Users/x",
            "/cygdrive/c/Users/x",
            r"\c\Users\x",
        ] {
            assert_eq!(expand(raw, &base(), None), expected, "{raw:?}");
        }
        assert_eq!(expand("/c", &base(), None), Some(PathBuf::from("C:/")));
        assert_eq!(
            expand("/cache/x", &base(), None),
            Some(PathBuf::from(r"C:\cache\x"))
        );
    }
}
