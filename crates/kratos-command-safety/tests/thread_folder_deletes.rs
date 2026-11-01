//! Forced deletes inside a thread's own folder still go to the user (D18). The check looks at the
//! command, never at where it points, so a delete the agent aims at its own work asks too.

use kratos_command_safety::{
    DangerousCommandMatch, DangerousCommandPlatform, dangerous_command, split_command_text,
};

/// What Codex puts in an approval request: the command's words joined with shell quoting.
fn approval_text(words: &[&str]) -> String {
    words
        .iter()
        .map(|word| {
            shlex::try_quote(word)
                .expect("no NUL in test words")
                .into_owned()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn check(words: &[&str], platform: DangerousCommandPlatform) -> Option<DangerousCommandMatch> {
    let command = split_command_text(&approval_text(words)).expect("the joined text splits");
    dangerous_command(&command, platform)
}

#[test]
fn a_forced_recursive_delete_in_the_thread_folder_is_flagged_on_posix() {
    for words in [
        &["bash", "-lc", "rm -rf ./build"][..],
        &["bash", "-lc", "cd /work/project && rm -rf build"],
        &["rm", "-rf", "/work/project/build"],
        &["/bin/rm", "-r", "-f", "build"],
    ] {
        assert_eq!(
            check(words, DangerousCommandPlatform::Posix),
            Some(DangerousCommandMatch::ForcedRm),
            "{words:?}"
        );
    }
}

#[test]
fn remove_item_with_force_in_the_thread_folder_is_flagged_on_windows() {
    for words in [
        &[
            "powershell.exe",
            "-NoProfile",
            "-Command",
            "Remove-Item -Recurse -Force .\\build",
        ][..],
        &[
            "powershell.exe",
            "-Command",
            "Remove-Item -Recurse -Force C:\\work\\project\\build",
        ],
        &[
            "pwsh",
            "-NoProfile",
            "-Command",
            "Get-ChildItem .\\out | Remove-Item -Force",
        ],
    ] {
        assert!(
            check(words, DangerousCommandPlatform::Windows).is_some(),
            "{words:?} was not flagged"
        );
    }
}

#[test]
fn del_with_force_in_the_thread_folder_is_flagged_on_windows() {
    for words in [
        &["cmd", "/c", "del /f build\\app.exe"][..],
        &[
            "cmd.exe",
            "/c",
            "del",
            "/f",
            "/q",
            "C:\\work\\project\\build\\app.exe",
        ],
    ] {
        assert!(
            check(words, DangerousCommandPlatform::Windows).is_some(),
            "{words:?} was not flagged"
        );
    }
}

#[test]
fn deletes_without_force_are_left_to_the_permission() {
    for (words, platform) in [
        (
            &["bash", "-lc", "rm -r ./build"][..],
            DangerousCommandPlatform::Posix,
        ),
        (&["rm", "build/app"], DangerousCommandPlatform::Posix),
        (
            &[
                "powershell.exe",
                "-Command",
                "Remove-Item .\\build\\app.exe",
            ],
            DangerousCommandPlatform::Windows,
        ),
        (
            &["cmd", "/c", "del build\\app.exe"],
            DangerousCommandPlatform::Windows,
        ),
    ] {
        assert_eq!(check(words, platform), None, "{words:?}");
    }
}

#[test]
fn text_that_is_not_shell_quoting_does_not_split() {
    assert_eq!(split_command_text("rm -rf 'unterminated"), None);
}
