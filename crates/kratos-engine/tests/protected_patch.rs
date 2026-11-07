use kratos_engine::providers::codex::policy::{Verdict, decide_patch};
use kratos_engine::providers::codex::profiles::ProtectedFolders;
use kratos_protocol::{
    AskReason, CappedText, FileChange, FileChangeKind, Permission, ThreadSettings,
};

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
