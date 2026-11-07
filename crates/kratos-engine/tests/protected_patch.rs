use kratos_engine::providers::codex::policy::{Verdict, decide_patch};
use kratos_engine::providers::codex::profiles::ProtectedFolders;
use kratos_protocol::{
    AskReason, CappedText, FileChange, FileChangeKind, Permission, ThreadSettings,
};

fn credential_alias_verdict(home: &std::path::Path, path: &str) -> Verdict {
    let settings = ThreadSettings {
        permission: Permission::FullAccess,
        ..ThreadSettings::default()
    };
    let folders = ProtectedFolders::new(home.join(".kratos"), None, Some(home.into()));
    let change = FileChange {
        path: path.into(),
        kind: FileChangeKind::Add,
        diff: CappedText::new("test", 100),
    };
    decide_patch(&settings, Some(&[change]), false, home, &folders)
}

#[test]
fn a_missing_credential_folder_with_a_trailing_dot_asks() {
    // Catches a missing .aws. folder being created as the protected .aws directory on Windows.
    let home = tempfile::tempdir().unwrap();
    assert_eq!(
        credential_alias_verdict(home.path(), ".aws./config"),
        Verdict::Ask(AskReason::Unchecked)
    );
}

#[test]
fn a_missing_credential_folder_with_a_stream_suffix_asks() {
    // Catches .ssh::$INDEX_ALLOCATION creating files in the protected .ssh directory on NTFS.
    let home = tempfile::tempdir().unwrap();
    assert_eq!(
        credential_alias_verdict(home.path(), ".ssh::$INDEX_ALLOCATION/config"),
        Verdict::Ask(AskReason::Unchecked)
    );
}

#[test]
fn a_missing_credential_folder_with_a_unicode_case_alias_asks() {
    // Catches a new .aw\u{17f} folder being read as protected .aws on a case-insensitive disk.
    let home = tempfile::tempdir().unwrap();
    assert_eq!(
        credential_alias_verdict(home.path(), ".aw\u{17f}/config"),
        Verdict::Ask(AskReason::Unchecked)
    );
}

#[test]
fn a_new_unicode_file_in_an_existing_folder_is_accepted() {
    // Catches the missing-folder rule refusing a non-ASCII file name in an existing directory.
    let home = tempfile::tempdir().unwrap();
    assert_eq!(
        credential_alias_verdict(home.path(), "r\u{e9}sum\u{e9}.txt"),
        Verdict::Accept
    );
}

#[test]
fn a_case_alias_of_a_missing_credential_folder_asks() {
    // Catches a new .AWS folder bypassing the protected .aws path before it exists.
    let home = tempfile::tempdir().unwrap();
    assert_eq!(
        credential_alias_verdict(home.path(), ".AWS/config"),
        Verdict::Ask(AskReason::ProtectedFolder {
            path: home.path().join(".aws").display().to_string()
        })
    );
}

#[test]
fn a_case_alias_below_an_existing_credential_folder_asks() {
    // Catches .SSH writing through an existing .ssh directory on a case-insensitive disk.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".ssh")).unwrap();
    assert_eq!(
        credential_alias_verdict(home.path(), ".SSH/new-key"),
        Verdict::Ask(AskReason::ProtectedFolder {
            path: home.path().join(".ssh").display().to_string()
        })
    );
}

#[test]
fn a_missing_parent_climb_into_a_case_alias_asks() {
    // Catches an uncanonicalized .SSH suffix surviving a missing/.. pair.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".ssh")).unwrap();
    assert_eq!(
        credential_alias_verdict(home.path(), "missing/../.SSH/config"),
        Verdict::Ask(AskReason::ProtectedFolder {
            path: home.path().join(".ssh").display().to_string()
        })
    );
}

#[test]
fn a_patch_through_a_directory_link_still_asks_before_changing_protected_files() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let protected = root.path().join("credentials");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&protected).unwrap();
    let alias = project.join("alias");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&protected, &alias).unwrap();
    #[cfg(windows)]
    {
        let made = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "New-Item -ItemType Junction -Path $env:KRATOS_TEST_LINK -Target $env:KRATOS_TEST_TARGET | Out-Null"])
            .env("KRATOS_TEST_LINK", &alias)
            .env("KRATOS_TEST_TARGET", &protected)
            .status().unwrap();
        assert!(made.success());
    }
    let settings = ThreadSettings {
        permission: Permission::FullAccess,
        ..ThreadSettings::default()
    };
    let folders = ProtectedFolders::new(protected.clone(), None, None);
    #[cfg(unix)]
    {
        // Catches missing/.. bypassing resolution of the real alias into protected files.
        let change = FileChange {
            path: "missing/../alias/new.txt".into(),
            kind: FileChangeKind::Add,
            diff: CappedText::new("test", 100),
        };
        assert_eq!(
            decide_patch(&settings, Some(&[change]), false, &project, &folders),
            Verdict::Ask(AskReason::Unchecked)
        );
    }
    // Catches the missing-parent guard refusing an ordinary new file without traversal.
    let change = FileChange {
        path: "missing/plain/new.txt".into(),
        kind: FileChangeKind::Add,
        diff: CappedText::new("test", 100),
    };
    assert_eq!(
        decide_patch(&settings, Some(&[change]), false, &project, &folders),
        Verdict::Accept
    );
    for (path, kind) in [
        ("alias/new.txt", FileChangeKind::Add),
        (
            "normal.txt",
            FileChangeKind::Update {
                move_path: Some("alias/moved.txt".into()),
            },
        ),
    ] {
        let change = FileChange {
            path: path.into(),
            kind,
            diff: CappedText::new("test", 100),
        };
        let verdict = decide_patch(&settings, Some(&[change]), false, &project, &folders);
        assert_eq!(
            verdict,
            Verdict::Ask(AskReason::ProtectedFolder {
                path: protected.display().to_string()
            })
        );
    }
}
