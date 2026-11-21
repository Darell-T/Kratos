//! Archiving a thread (R5, D16, D27), against real git in temporary repositories.

use std::path::Path;
use std::time::Instant;

use fake_agents::codex_control;
use kratos_api_client::Client;
use kratos_protocol::{
    ArchiveThread, ArchivedThread, Cleanup, LeftFiles, PlaceRequest, Thread, ThreadDetails, routes,
};
use kratos_store::{Checkout, Store};
use serde_json::json;
use tempfile::TempDir;

use super::running_engine::{RunningEngine, WAIT, fake_codex_at, post};
use super::thread_places::{
    create, existing, git, new_repository, resolved, workspace_at, worktree_count, worktree_of,
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
    assert_eq!(worktree_count(&repository), 1);
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
    let shared = resolved(&repository.join(".git"));
    let store = Store::open(&home).unwrap();
    let mut checkouts = Vec::new();
    // The sweep goes in reservation order, so once "clean" is gone it has passed "dirty".
    for name in ["dirty", "clean"] {
        let path = repository.join(".kratos-worktrees").join(name);
        let checkout = Checkout {
            path: path.to_str().unwrap().to_owned(),
            repository: shared.to_str().unwrap().to_owned(),
            branch: Some(name.to_owned()),
        };
        store.reserve_worktree(&checkout, "0000000").unwrap();
        git(
            &repository,
            &["worktree", "add", "--quiet", "-b", name, &checkout.path],
        );
        checkouts.push(path);
    }
    drop(store);
    std::fs::write(checkouts[0].join("notes.txt"), "mine\n").unwrap();

    let _engine = RunningEngine::start(&home);

    wait_until_gone(&checkouts[1]);
    assert!(branch_exists(&repository, "clean"));
    assert!(checkouts[0].join("notes.txt").exists());
    assert_eq!(worktree_count(&repository), 2);
}
