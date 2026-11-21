//! Archiving a thread (R5, D16, D27), against real git in temporary repositories.

use std::path::Path;
use std::time::Instant;

use fake_agents::codex_control;
use kratos_api_client::Client;
use kratos_engine::worktrees::WORKTREE_ID_FILE;
use kratos_protocol::{
    ArchiveThread, ArchivedThread, Cleanup, CreatePullRequest, LeftFiles, Ownership, PlaceRequest,
    ProjectWorktree, RequiredText, Thread, ThreadDetails, Workspace, routes,
};
use kratos_store::{Checkout, Store};
use serde_json::json;
use tempfile::TempDir;

use super::running_engine::{Gate, RunningEngine, WAIT, fake_codex_at, post};
use super::thread_places::{
    create, existing, git, new_repository, refusal, request, resolved, take_place, workspace_at,
    worktree_count, worktree_of,
};

fn archive(client: &Client, thread: &Thread, files: LeftFiles) -> Cleanup {
    let reply = client
        .post(
            &routes::thread_archive(&thread.id),
            &ArchiveThread { files },
        )
        .unwrap();
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.parse::<ArchivedThread>().unwrap().cleanup
}

fn details(client: &Client, thread: &Thread) -> ThreadDetails {
    client
        .get(&routes::thread(&thread.id))
        .unwrap()
        .parse()
        .unwrap()
}

fn wait_for_turn(client: &Client, thread: &Thread, running: bool) {
    let deadline = Instant::now() + WAIT;
    while details(client, thread).running_turn.is_some() != running {
        assert!(Instant::now() < deadline, "running stayed {}", !running);
        std::thread::yield_now();
    }
}

fn branch_exists(repository: &Path, branch: &str) -> bool {
    !git(repository, &["branch", "--list", branch]).is_empty()
}

#[test]
fn archiving_waits_for_the_turn_then_ends_the_threads_process_and_refuses_new_messages() {
    // Catches an archive that strands a running turn, leaves the agent's process running in the
    // thread's folder, lets a message start the thread again, or touches the project folder.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    std::fs::write(repository.join("notes.txt"), "mine\n").unwrap();
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "chat", PlaceRequest::default());
    post(&client, &thread, "hello");
    engine.start_turn();
    wait_for_turn(&client, &thread, true);
    let port = codex_control::read_port(&engine.fake_codex_control_file()).unwrap();

    let refused = client
        .post(
            &routes::thread_archive(&thread.id),
            &ArchiveThread::default(),
        )
        .unwrap();
    assert_eq!(refused.status, 409, "{}", refused.body);
    let message = refused.body["error"].as_str().unwrap();
    assert!(message.contains("a turn runs"), "{message}");
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
    wait_for_turn(&client, &thread, false);
    assert!(fake_codex_at(port, "test/answers", json!({})).is_some());

    assert_eq!(
        archive(&client, &thread, LeftFiles::Discard),
        Cleanup::NotOwned
    );
    assert!(
        fake_codex_at(port, "test/answers", json!({})).is_none(),
        "the thread's Codex still runs"
    );
    assert!(details(&client, &thread).archived_at.is_some());
    let again = client
        .post(
            &routes::thread_messages(&thread.id),
            &json!({ "text": "again" }),
        )
        .unwrap();
    assert_eq!(again.status, 409, "{}", again.body);
    let message = again.body["error"].as_str().unwrap();
    assert!(message.contains("archived"), "{message}");
    assert_eq!(
        archive(&client, &thread, LeftFiles::Keep),
        Cleanup::NotOwned
    );
    assert_eq!(
        std::fs::read_to_string(repository.join("notes.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn a_thread_running_a_background_command_cannot_be_archived_until_it_ends() {
    // Catches an archive that kills a dev server or build the agent left running after its turn,
    // or removes the worktree it runs in.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "serve", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    post(&client, &thread, "start the dev server");
    engine.start_turn();
    wait_for_turn(&client, &thread, true);
    let terminal = json!({
        "command": "npm run dev",
        "cwd": path,
        "itemId": "item-1",
        "processId": "process-1",
    });
    engine.fake_codex(
        "test/backgroundTerminals",
        json!({ "terminals": [terminal] }),
    );
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
    wait_for_turn(&client, &thread, false);

    let refused = client
        .post(
            &routes::thread_archive(&thread.id),
            &ArchiveThread::default(),
        )
        .unwrap();

    assert_eq!(refused.status, 409, "{}", refused.body);
    let message = refused.body["error"].as_str().unwrap();
    assert!(message.contains("background command"), "{message}");
    assert!(details(&client, &thread).archived_at.is_none());
    assert!(Path::new(&path).exists());
    engine.fake_codex("test/backgroundTerminals", json!({ "terminals": [] }));
    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
}

#[test]
fn an_unused_clean_worktree_is_removed_and_its_branch_kept() {
    // Catches a worktree left behind once its only thread is archived, or a removal that deletes
    // the branch and the commits on it.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "Fix it", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    std::fs::write(folder.join("fix.txt"), "fixed\n").unwrap();
    git(folder, &["add", "fix.txt"]);
    git(folder, &["commit", "--quiet", "--message", "fix"]);

    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);

    assert!(!folder.exists());
    assert_eq!(worktree_count(&repository), 1);
    assert_eq!(
        git(&repository, &["show", "fix-it:fix.txt"]),
        "fixed",
        "the branch keeps the thread's commit"
    );
}

#[test]
fn a_worktree_holding_files_a_removal_would_delete_stays_until_the_user_discards_them() {
    // Catches a removal that deletes files git ignores, which `git worktree remove` does without
    // `--force`, or changed and untracked files, before the user chose to.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    std::fs::write(repository.join(".gitignore"), "*.log\n").unwrap();
    git(&repository, &["add", ".gitignore"]);
    git(
        &repository,
        &["commit", "--quiet", "--message", "ignore logs"],
    );
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "dirty", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    std::fs::write(folder.join("build.log"), "output\n").unwrap();

    assert_eq!(
        archive(&client, &thread, LeftFiles::Keep),
        Cleanup::Kept {
            files: vec!["!! build.log".into()]
        }
    );
    std::fs::write(folder.join("README.md"), "changed\n").unwrap();
    std::fs::write(folder.join("notes.txt"), "new\n").unwrap();
    let Cleanup::Kept { files } = archive(&client, &thread, LeftFiles::Keep) else {
        panic!("the changed worktree was not kept");
    };
    assert_eq!(files, [" M README.md", "?? notes.txt", "!! build.log"]);
    assert!(folder.join("build.log").exists());
    assert_eq!(worktree_count(&repository), 2);

    assert_eq!(
        archive(&client, &thread, LeftFiles::Discard),
        Cleanup::Removed
    );
    assert!(!folder.exists());
    assert!(branch_exists(&repository, "dirty"));
}

#[test]
fn an_edit_git_is_told_to_overlook_still_keeps_the_worktree() {
    // Catches a removal that trusts `git status`, which skips files marked assume-unchanged or
    // skip-worktree, and so deletes the only copy of an edit to them.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    std::fs::write(repository.join("config.txt"), "default\n").unwrap();
    git(&repository, &["add", "config.txt"]);
    git(&repository, &["commit", "--quiet", "--message", "config"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "hidden", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    std::fs::write(folder.join("README.md"), "only copy\n").unwrap();
    std::fs::write(folder.join("config.txt"), "local\n").unwrap();
    git(folder, &["update-index", "--assume-unchanged", "README.md"]);
    git(folder, &["update-index", "--skip-worktree", "config.txt"]);
    assert_eq!(git(folder, &["status", "--porcelain"]), "");

    assert_eq!(
        archive(&client, &thread, LeftFiles::Keep),
        Cleanup::Kept {
            files: vec![" M README.md".into(), " M config.txt".into()]
        }
    );
    assert_eq!(
        std::fs::read_to_string(folder.join("README.md")).unwrap(),
        "only copy\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn an_overlooked_edit_to_a_file_whose_name_is_not_utf8_keeps_the_worktree() {
    // Catches git's file names read as UTF-8 text: the name no longer matches the file, so its
    // edit goes unchecked and the removal deletes the only copy.
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let name = OsStr::from_bytes(b"notes-\xff.txt");
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    std::fs::write(repository.join(name), "original\n").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "--quiet", "--message", "notes"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "notes", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    let marked = std::process::Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(["update-index", "--assume-unchanged", "--"])
        .arg(name)
        .status()
        .unwrap();
    assert!(marked.success());
    std::fs::write(folder.join(name), "only copy\n").unwrap();
    assert_eq!(git(folder, &["status", "--porcelain"]), "");

    assert_eq!(
        archive(&client, &thread, LeftFiles::Keep),
        Cleanup::Kept {
            files: vec![" M notes-\u{FFFD}.txt".into()]
        }
    );
    assert_eq!(
        std::fs::read_to_string(folder.join(name)).unwrap(),
        "only copy\n"
    );
}

#[cfg(unix)]
#[test]
fn an_overlooked_change_of_mode_or_file_type_keeps_the_worktree() {
    // Catches overlooked files compared by content alone: a file made executable, or replaced by
    // a link to a file with the same content, hashes the same, so the removal drops the change.
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    for name in ["tool.sh", "link.txt", "peer.txt"] {
        std::fs::write(repository.join(name), "same\n").unwrap();
    }
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "--quiet", "--message", "files"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "modes", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    let tool = folder.join("tool.sh");
    let link = folder.join("link.txt");
    git(
        folder,
        &["update-index", "--assume-unchanged", "tool.sh", "link.txt"],
    );
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("peer.txt", &link).unwrap();
    assert_eq!(git(folder, &["status", "--porcelain"]), "");

    assert_eq!(
        archive(&client, &thread, LeftFiles::Keep),
        Cleanup::Kept {
            files: vec![" T link.txt".into(), " M tool.sh".into()]
        }
    );
    let mode = std::fs::metadata(&tool).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "{mode:o}");
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
}

#[test]
fn a_clean_worktree_with_many_files_git_is_told_to_overlook_is_removed() {
    // Catches every overlooked file named on git's command line, which outgrows the longest
    // command Windows runs, so a clean worktree could never be removed.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let names: Vec<String> = (0..550)
        .map(|number| format!("{}{number}.txt", "configuration-".repeat(5)))
        .collect();
    for name in &names {
        std::fs::write(repository.join(name), "tracked config\n").unwrap();
    }
    git(&repository, &["add", "."]);
    git(
        &repository,
        &["commit", "--quiet", "--message", "configuration"],
    );
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "large", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    for batch in names.chunks(100) {
        let mut marking = vec!["update-index", "--assume-unchanged", "--"];
        marking.extend(batch.iter().map(String::as_str));
        git(folder, &marking);
    }

    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
    assert!(!folder.exists());
}

#[test]
fn a_commit_no_branch_holds_keeps_the_worktree_even_when_files_are_discarded() {
    // Catches a removal that drops the worktree's HEAD when it is a commit on no branch, which
    // only that worktree's HEAD kept reachable.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "detached", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    git(folder, &["switch", "--quiet", "--detach"]);
    std::fs::write(folder.join("work.txt"), "finished\n").unwrap();
    let request = json!({ "message": "Finish the work" });
    let committed = client
        .post(&routes::thread_commit(&thread.id), &request)
        .unwrap();
    assert_eq!(committed.status, 201, "{}", committed.body);
    let commit = git(folder, &["rev-parse", "HEAD"]);

    for files in [LeftFiles::Keep, LeftFiles::Discard] {
        assert_eq!(
            archive(&client, &thread, files),
            Cleanup::UnsavedCommit {
                commit: commit.clone()
            }
        );
    }
    assert_eq!(git(folder, &["rev-parse", "HEAD"]), commit);
    git(folder, &["switch", "--quiet", "--create", "finished"]);
    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
    assert_eq!(git(&repository, &["rev-parse", "finished"]), commit);
}

#[test]
fn a_worktree_stays_while_a_project_opened_in_it_has_a_thread() {
    // Catches a removal that counts only threads attached to the worktree, and so deletes the
    // folder a thread of another project, opened at the worktree, works in.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let owner = create(&client, &workspace, "owner", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &owner).path;
    let opened = workspace_at(&engine, &Path::new(&path).join("."));
    let occupant = create(&client, &opened, "occupant", PlaceRequest::default());

    assert_eq!(archive(&client, &owner, LeftFiles::Discard), Cleanup::InUse);
    assert!(Path::new(&path).exists());
    assert_eq!(
        archive(&client, &occupant, LeftFiles::Keep),
        Cleanup::NotOwned
    );
    assert_eq!(archive(&client, &owner, LeftFiles::Keep), Cleanup::Removed);
}

#[test]
fn a_thread_cannot_start_in_a_worktree_an_archive_is_removing() {
    // Catches a removal that looks for threads in the worktree only before it marks it as being
    // removed: a project thread started there while the removal waits on git loses its folder.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let remote = root.path().join("remote.git");
    let remote_text = remote.to_str().unwrap();
    git(
        root.path(),
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            remote_text,
        ],
    );
    git(&repository, &["remote", "add", "origin", remote_text]);
    git(&repository, &["push", "--quiet", "origin", "main"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let owner = create(&client, &workspace, "owner", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &owner).path;
    let reviewed = create(&client, &workspace, "reviewed", PlaceRequest::NewWorktree);
    engine.hold(Gate::Gh);
    let pull_request = {
        let (client, reviewed) = (client.clone(), reviewed.clone());
        std::thread::spawn(move || {
            let request = CreatePullRequest {
                title: RequiredText::new("Review").unwrap(),
                body: String::new(),
            };
            let route = routes::thread_pull_request(&reviewed.id);
            client.post(&route, &request).unwrap().status
        })
    };
    engine.wait_until_held(Gate::Gh);
    let archiving = {
        let (client, owner) = (client.clone(), owner.clone());
        std::thread::spawn(move || archive(&client, &owner, LeftFiles::Keep))
    };
    wait_for_worktree_state(&engine, &path, "removing");
    let opened = workspace_at(&engine, Path::new(&path));
    let occupant = client.create_thread(&request(&opened, "occupant", PlaceRequest::default()));
    engine.release(Gate::Gh);

    let (status, message) = refusal(occupant.expect_err("the occupant started"));
    assert_eq!(status, 409, "{message}");
    assert_eq!(archiving.join().unwrap(), Cleanup::Removed);
    assert_eq!(pull_request.join().unwrap(), 201);
}

#[test]
fn a_worktree_stays_while_a_thread_works_in_a_worktree_made_inside_it() {
    // Catches a removal that counts only project threads inside the worktree, and so deletes a
    // worktree made from a project opened there along with it.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let owner = create(&client, &workspace, "owner", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &owner).path;
    let opened = workspace_at(&engine, Path::new(&path));
    let nested = create(&client, &opened, "nested", PlaceRequest::NewWorktree);
    let nested_path = worktree_of(&client, &nested).path;

    assert_eq!(archive(&client, &owner, LeftFiles::Discard), Cleanup::InUse);
    assert!(Path::new(&nested_path).exists());
}

/// Waits until the engine records the worktree at `path` in `state`.
fn wait_for_worktree_state(engine: &RunningEngine, path: &str, state: &str) {
    let database = rusqlite::Connection::open(engine.home().join("kratos.db")).unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let found: String = database
            .query_row(
                "SELECT state FROM worktrees WHERE path = ?1 AND state != 'removed'",
                [path],
                |row| row.get(0),
            )
            .unwrap();
        if found == state {
            return;
        }
        assert!(Instant::now() < deadline, "the worktree stayed {found}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn a_worktree_stays_while_another_thread_works_in_it() {
    // Catches a removal that pulls the folder out from under a thread still working in it.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let first = create(&client, &workspace, "shared", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &first).path;
    let second = create(&client, &workspace, "helper", existing(Path::new(&path)));

    assert_eq!(archive(&client, &first, LeftFiles::Discard), Cleanup::InUse);
    assert!(Path::new(&path).exists());
    assert_eq!(archive(&client, &second, LeftFiles::Keep), Cleanup::Removed);
    assert!(!Path::new(&path).exists());
}

#[test]
fn a_borrowed_worktree_is_never_removed() {
    // Catches Kratos deleting a worktree the user made, even when asked to discard files.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let theirs = root.path().join("feature");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature",
            theirs.to_str().unwrap(),
        ],
    );
    let theirs = resolved(&theirs);
    std::fs::write(theirs.join("wip.txt"), "mine\n").unwrap();
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "borrow", existing(&theirs));

    assert_eq!(
        archive(&client, &thread, LeftFiles::Discard),
        Cleanup::NotOwned
    );

    assert!(theirs.join("wip.txt").exists());
    assert_eq!(worktree_count(&repository), 2);
}

#[test]
fn a_folder_that_is_no_longer_a_worktree_is_removed_only_while_empty() {
    // Catches git run in a folder that lost its `.git` file, which reads the project's repository
    // instead, and a leftover folder deleted with the files still in it.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "orphan", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    std::fs::remove_file(folder.join(".git")).unwrap();

    let blocked = archive(&client, &thread, LeftFiles::Discard);

    assert!(matches!(blocked, Cleanup::Blocked { .. }), "{blocked:?}");
    assert!(folder.join("README.md").exists());
    std::fs::remove_file(folder.join("README.md")).unwrap();
    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
    assert!(!folder.exists());
    assert_eq!(worktree_count(&repository), 1);
}

#[test]
fn removing_a_leftover_forgets_only_that_worktree() {
    // Catches `git worktree prune`, which also deletes git's record of every other worktree whose
    // folder is out of reach for now, such as one on a drive that is not plugged in.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let users = root.path().join("users");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "users",
            users.to_str().unwrap(),
        ],
    );
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "orphan", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    for file in [".git", "README.md"] {
        std::fs::remove_file(folder.join(file)).unwrap();
    }
    let away = root.path().join("unplugged");
    std::fs::rename(&users, &away).unwrap();

    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
    assert!(!folder.exists());
    std::fs::rename(&away, &users).unwrap();
    assert_eq!(worktree_count(&repository), 2);
    assert_eq!(git(&users, &["branch", "--show-current"]), "users");
}

fn listed_ownership(client: &Client, workspace: &Workspace, path: &Path) -> Ownership {
    let listed: Vec<ProjectWorktree> = client
        .get(&routes::workspace_worktrees(&workspace.id))
        .unwrap()
        .parse()
        .unwrap();
    listed
        .into_iter()
        .find(|worktree| Path::new(&worktree.path) == path)
        .unwrap_or_else(|| panic!("{} is not listed", path.display()))
        .ownership
}

fn commit_status(client: &Client, thread: &Thread) -> u16 {
    let request = json!({ "message": "Commit through the old thread" });
    client
        .post(&routes::thread_commit(&thread.id), &request)
        .unwrap()
        .status
}

#[test]
fn a_checkout_that_took_a_moved_worktrees_place_is_never_taken_for_kratoss() {
    // Catches Kratos knowing its worktrees only by path: once the user moves one away and makes
    // another checkout where it stood, the listing calls the newcomer Kratos's, the old thread
    // commits into it, and archive deletes it.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "before", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    let moved = root.path().join("moved");
    take_place(&repository, &path, &moved);

    assert_eq!(
        listed_ownership(&client, &workspace, folder),
        Ownership::Borrowed
    );
    assert_eq!(commit_status(&client, &thread), 409);
    assert_eq!(
        archive(&client, &thread, LeftFiles::Discard),
        Cleanup::NotOwned
    );
    assert!(folder.join("users.txt").exists());
    assert!(moved.exists());
    let borrowed = create(&client, &workspace, "users", existing(folder));
    assert_eq!(
        worktree_of(&client, &borrowed).ownership,
        Ownership::Borrowed
    );
}

#[test]
fn a_checkout_git_made_again_where_kratoss_worktree_stood_is_never_taken_for_kratoss() {
    // Catches telling Kratos's worktree apart by the folder git keeps for it: once the user
    // removes the worktree and makes another at its path, git gives the newcomer a folder of the
    // same name, and archive deletes the user's checkout.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "before", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    git(&repository, &["worktree", "unlock", &path]);
    git(&repository, &["worktree", "remove", &path]);
    git(
        &repository,
        &["worktree", "add", "--quiet", "-b", "users-branch", &path],
    );
    std::fs::write(folder.join("users.txt"), "the user's\n").unwrap();

    assert_eq!(
        listed_ownership(&client, &workspace, folder),
        Ownership::Borrowed
    );
    assert_eq!(commit_status(&client, &thread), 409);
    assert_eq!(
        archive(&client, &thread, LeftFiles::Discard),
        Cleanup::NotOwned
    );
    assert!(folder.join("users.txt").exists());
}

#[test]
fn git_refuses_to_move_or_remove_a_worktree_kratos_made() {
    // Catches a worktree git lets the user move or remove while Kratos works in it, so another
    // checkout can take its path between Kratos's check of the path and its use.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "locked", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let moved = root.path().join("moved");

    for args in [
        ["worktree", "move", &path, moved.to_str().unwrap()].as_slice(),
        ["worktree", "remove", &path].as_slice(),
    ] {
        let refused = std::process::Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(args)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&refused.stderr);
        assert!(!refused.status.success(), "git {args:?} went ahead");
        assert!(said.contains("locked"), "{said}");
    }
    assert!(Path::new(&path).join(".git").exists());
    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
    assert!(!Path::new(&path).exists());
    assert_eq!(worktree_count(&repository), 1);
}

#[test]
fn a_worktree_that_holds_a_submodule_stays_unless_files_are_discarded() {
    // Catches a removal through Kratos's own lock with git's double force, which also skips git's
    // refusal to delete a submodule and the history it holds.
    let root = TempDir::new().unwrap();
    let library_root = TempDir::new_in(root.path()).unwrap();
    let library = new_repository(&library_root);
    let repository = new_repository(&root);
    let library_text = library.to_str().unwrap();
    git(
        &repository,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            library_text,
            "library",
        ],
    );
    git(&repository, &["commit", "--quiet", "--message", "library"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "nested", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    git(
        folder,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--quiet",
            "--init",
        ],
    );
    assert_eq!(git(folder, &["status", "--porcelain"]), "");

    let kept = archive(&client, &thread, LeftFiles::Keep);

    assert!(matches!(kept, Cleanup::Blocked { .. }), "{kept:?}");
    assert!(folder.join("library").join("README.md").exists());
    assert_eq!(
        archive(&client, &thread, LeftFiles::Discard),
        Cleanup::Removed
    );
    assert!(!folder.exists());
}

#[test]
fn a_checkout_that_takes_the_worktrees_place_during_an_archive_is_left_alone() {
    // Catches a worktree checked to be Kratos's before git reads it and removed by path after,
    // which deletes a checkout that took the path meanwhile.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start_holding_git(&root.path().join("kratos"), "symbolic-ref");
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "original", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    std::fs::write(Path::new(&path).join("original.txt"), "discard me\n").unwrap();
    engine.hold(Gate::Git);
    let archiving = {
        let (client, thread) = (client.clone(), thread.clone());
        std::thread::spawn(move || archive(&client, &thread, LeftFiles::Discard))
    };
    engine.wait_until_held(Gate::Git);
    let moved = root.path().join("moved");
    take_place(&repository, &path, &moved);
    engine.release(Gate::Git);

    assert_eq!(archiving.join().unwrap(), Cleanup::NotOwned);
    assert!(Path::new(&path).join("users.txt").exists());
    assert!(moved.join("original.txt").exists());
}

#[test]
fn an_archived_threads_finished_work_stops_once_its_worktree_is_gone() {
    // Catches a thread whose worktree Kratos removed committing into whatever checkout later
    // stands at the same path.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "reused", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
    git(
        &repository,
        &["worktree", "add", "--quiet", "-b", "later", &path],
    );
    std::fs::write(Path::new(&path).join("later.txt"), "later work\n").unwrap();

    assert_eq!(commit_status(&client, &thread), 409);
    let diff = client.get(&routes::thread_diff(&thread.id)).unwrap();
    assert_eq!(diff.status, 409, "{}", diff.body);
    assert_eq!(
        git(Path::new(&path), &["log", "-1", "--format=%s"]),
        "first"
    );
}

/// Windows refuses to remove a folder that a running program works in.
#[cfg(windows)]
#[test]
fn a_removal_a_held_folder_blocks_is_tried_again_by_the_next_archive() {
    // Catches a blocked removal reported as done, or one that stays blocked after the folder is
    // free again.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "held", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let mut holder = std::process::Command::new("ping")
        .args(["-n", "60", "127.0.0.1"])
        .current_dir(&path)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let blocked = archive(&client, &thread, LeftFiles::Keep);
    holder.kill().unwrap();
    holder.wait().unwrap();

    assert!(matches!(blocked, Cleanup::Blocked { .. }), "{blocked:?}");
    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
    assert!(!Path::new(&path).exists());
    assert_eq!(worktree_count(&repository), 1);
}

/// Waits for the engine's startup sweep to remove `folder`.
fn wait_until_gone(folder: &Path) {
    let deadline = Instant::now() + WAIT;
    while folder.exists() {
        assert!(
            Instant::now() < deadline,
            "{} is still there",
            folder.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn the_next_engine_finishes_a_removal_git_could_not() {
    // Catches a blocked removal that stays forever when no thread is archived again.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let home = root.path().join("kratos");
    let engine = RunningEngine::start(&home);
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "stuck", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let folder = Path::new(&path);
    std::fs::remove_file(folder.join(".git")).unwrap();
    let blocked = archive(&client, &thread, LeftFiles::Keep);
    assert!(matches!(blocked, Cleanup::Blocked { .. }), "{blocked:?}");
    engine.kill();
    std::fs::remove_file(folder.join("README.md")).unwrap();

    let _engine = RunningEngine::start(&home);

    wait_until_gone(folder);
    // The sweep deletes git's record of the worktree just after the folder.
    let deadline = Instant::now() + WAIT;
    while worktree_count(&repository) != 1 {
        assert!(Instant::now() < deadline, "git still records the worktree");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn the_next_engine_removes_a_worktree_whose_archive_stopped_before_removal_began() {
    // Catches a worktree left for good when an archive recorded the thread as archived and then
    // failed before marking the worktree for removal, which the next start never looked at.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let home = root.path().join("kratos");
    let engine = RunningEngine::start(&home);
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(
        &client,
        &workspace,
        "interrupted",
        PlaceRequest::NewWorktree,
    );
    let path = worktree_of(&client, &thread).path;
    engine.kill();
    let database = rusqlite::Connection::open(home.join("kratos.db")).unwrap();
    let archived = database
        .execute(
            "UPDATE threads SET archived_at = created_at WHERE id = ?1",
            [thread.id.as_str()],
        )
        .unwrap();
    assert_eq!(archived, 1);
    drop(database);

    let _engine = RunningEngine::start(&home);

    wait_until_gone(Path::new(&path));
    assert!(branch_exists(&repository, "interrupted"));
}

#[test]
fn the_next_engine_removes_a_clean_checkout_no_thread_took_and_keeps_one_with_files() {
    // Catches a worktree an engine made and then stopped before its thread was recorded, left
    // behind for good, or one holding files deleted without the user's choice.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let home = root.path().join("kratos");
    let store = Store::open(&home).unwrap();
    // The sweep goes in reservation order, so once "clean" is gone it has passed "dirty".
    let dirty = reserve_and_check_out(&store, &repository, "dirty", Marked::AsKratoss);
    let clean = reserve_and_check_out(&store, &repository, "clean", Marked::AsKratoss);
    drop(store);
    std::fs::write(dirty.join("notes.txt"), "mine\n").unwrap();

    let _engine = RunningEngine::start(&home);

    wait_until_gone(&clean);
    assert!(branch_exists(&repository, "clean"));
    assert!(dirty.join("notes.txt").exists());
    assert_eq!(worktree_count(&repository), 2);
}

#[test]
fn the_next_engine_never_removes_a_checkout_its_reservation_cannot_vouch_for() {
    // Catches a reservation trusted for its path alone. An engine that stopped before it marked
    // the checkout git made for it leaves a reservation that may name the user's own checkout by
    // the next start.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let home = root.path().join("kratos");
    let store = Store::open(&home).unwrap();
    let theirs = reserve_and_check_out(&store, &repository, "theirs", Marked::No);
    let ours = reserve_and_check_out(&store, &repository, "ours", Marked::AsKratoss);
    drop(store);

    let _engine = RunningEngine::start(&home);

    wait_until_gone(&ours);
    assert!(theirs.join("README.md").exists());
    assert_eq!(worktree_count(&repository), 2);
}

/// Whether a checkout carries the mark Kratos leaves on a worktree it made.
enum Marked {
    AsKratoss,
    No,
}

/// Reserves a worktree named `name`, as an engine does before it makes one, and has git make it.
fn reserve_and_check_out(
    store: &Store,
    repository: &Path,
    name: &str,
    marked: Marked,
) -> std::path::PathBuf {
    let path = repository.join(".kratos-worktrees").join(name);
    let checkout = Checkout {
        path: path.to_str().unwrap().to_owned(),
        repository: resolved(&repository.join(".git"))
            .to_str()
            .unwrap()
            .to_owned(),
        branch: Some(name.to_owned()),
    };
    let id = store.reserve_worktree(&checkout, "0000000").unwrap();
    git(
        repository,
        &["worktree", "add", "--quiet", "-b", name, &checkout.path],
    );
    if let Marked::AsKratoss = marked {
        let git_dir = git(&path, &["rev-parse", "--absolute-git-dir"]);
        std::fs::write(Path::new(&git_dir).join(WORKTREE_ID_FILE), id.as_str()).unwrap();
    }
    path
}
