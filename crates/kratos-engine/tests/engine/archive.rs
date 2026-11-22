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
fn a_worktree_stays_locked_and_leaves_any_replacement_alone_during_removal() {
    // Catches unlocking a worktree during removal: another git command could move the checkout
    // without unlocking it and place an unlocked replacement at the path.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start_holding_git(&root.path().join("kratos"), "symbolic-ref");
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "guarded", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let relocated = root.path().join("relocated");
    engine.hold(Gate::Git);
    let archiving = {
        let (client, thread) = (client.clone(), thread.clone());
        std::thread::spawn(move || archive(&client, &thread, LeftFiles::Discard))
    };
    engine.wait_until_held(Gate::Git);
    let moved = std::process::Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args(["worktree", "move", &path, relocated.to_str().unwrap()])
        .output()
        .unwrap();
    let mut created_replacement = false;
    if !Path::new(&path).exists() {
        git(
            &repository,
            &["worktree", "add", "--quiet", "-b", "replacement", &path],
        );
        std::fs::write(Path::new(&path).join("users.txt"), "unlocked\n").unwrap();
        created_replacement = true;
    }
    engine.release(Gate::Git);

    assert!(
        !moved.status.success(),
        "move succeeded without user unlock"
    );
    assert!(!relocated.exists());
    assert_eq!(archiving.join().unwrap(), Cleanup::Removed);
    if created_replacement {
        assert_eq!(
            std::fs::read_to_string(Path::new(&path).join("users.txt")).unwrap(),
            "unlocked\n"
        );
        assert_eq!(
            git(Path::new(&path), &["branch", "--show-current"]),
            "replacement"
        );
    }
}

#[test]
fn a_failed_removal_never_overwrites_a_users_lock_reason() {
    // Catches unconditional re-locking after a refused removal, which overwrote a lock reason the
    // user installed before git ran.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start_holding_git(&root.path().join("kratos"), "ls-files");
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "relock", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    let admin = git(Path::new(&path), &["rev-parse", "--absolute-git-dir"]);
    engine.hold(Gate::Git);
    let archiving = {
        let (client, thread) = (client.clone(), thread.clone());
        std::thread::spawn(move || archive(&client, &thread, LeftFiles::Keep))
    };
    engine.wait_until_held(Gate::Git);
    git(&repository, &["worktree", "unlock", &path]);
    git(
        &repository,
        &[
            "worktree",
            "lock",
            "--reason",
            "user: preserve offline",
            &path,
        ],
    );
    engine.release(Gate::Git);

    let cleanup = archiving.join().unwrap();
    assert!(matches!(cleanup, Cleanup::Blocked { .. }), "{cleanup:?}");
    assert!(Path::new(&path).join("README.md").exists());
    assert_eq!(
        std::fs::read_to_string(Path::new(&admin).join("locked"))
            .unwrap()
            .trim(),
        "user: preserve offline"
    );
}

#[test]
fn the_next_engine_leaves_an_offline_worktree_the_user_locked() {
    // Catches offline leftover cleanup deleting git's admin record and lock when the record still
    // carries Kratos's worktree id even though the user replaced the lock reason.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let home = root.path().join("kratos");
    let engine = RunningEngine::start(&home);
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let worktrees = ["offline", "later"].map(|title| {
        let thread = create(&client, &workspace, title, PlaceRequest::NewWorktree);
        let path = worktree_of(&client, &thread).path;
        std::fs::write(Path::new(&path).join("hold.txt"), "keep\n").unwrap();
        let kept = archive(&client, &thread, LeftFiles::Keep);
        assert!(matches!(kept, Cleanup::Kept { .. }), "{kept:?}");
        if title == "later" {
            std::fs::remove_file(Path::new(&path).join("hold.txt")).unwrap();
        }
        path
    });
    drop(engine);
    let [offline, later] = &worktrees;
    let admin = git(Path::new(offline), &["rev-parse", "--absolute-git-dir"]);
    git(&repository, &["worktree", "unlock", offline]);
    git(
        &repository,
        &[
            "worktree",
            "lock",
            "--reason",
            "user: preserve while offline",
            offline,
        ],
    );
    let parked = root.path().join("parked-offline");
    std::fs::rename(offline, &parked).unwrap();

    let _engine = RunningEngine::start(&home);

    wait_until_gone(Path::new(later));
    std::fs::rename(&parked, offline).unwrap();
    assert!(Path::new(&admin).exists(), "git admin record was deleted");
    assert_eq!(
        std::fs::read_to_string(Path::new(&admin).join("locked"))
            .unwrap()
            .trim(),
        "user: preserve while offline"
    );
    assert!(Path::new(offline).join("hold.txt").is_file());
    assert_eq!(
        git(Path::new(offline), &["rev-parse", "--is-inside-work-tree"]),
        "true"
    );
}

#[test]
fn the_next_engine_leaves_a_kept_worktree_the_user_locked() {
    // Catches a removal that passes the user's own git lock, which only Kratos's lock may.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let home = root.path().join("kratos");
    let engine = RunningEngine::start(&home);
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    // The sweep goes in reservation order, so once "later" is gone it has passed "locked".
    let worktrees = ["locked", "later"].map(|title| {
        let thread = create(&client, &workspace, title, PlaceRequest::NewWorktree);
        let path = worktree_of(&client, &thread).path;
        std::fs::write(Path::new(&path).join("notes.txt"), "mine\n").unwrap();
        let kept = archive(&client, &thread, LeftFiles::Keep);
        assert!(matches!(kept, Cleanup::Kept { .. }), "{kept:?}");
        std::fs::remove_file(Path::new(&path).join("notes.txt")).unwrap();
        path
    });
    drop(engine);
    let [locked, later] = &worktrees;
    git(&repository, &["worktree", "unlock", locked]);
    git(
        &repository,
        &["worktree", "lock", "--reason", "mine to keep", locked],
    );

    let _engine = RunningEngine::start(&home);

    wait_until_gone(Path::new(later));
    assert!(Path::new(locked).join("README.md").exists());
    let listed = git(&repository, &["worktree", "list", "--porcelain"]);
    assert!(listed.contains("locked mine to keep"), "{listed}");
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

/// Waits for the engine's startup sweep to leave `expected` worktrees registered in `repository`.
fn wait_for_worktree_count(repository: &Path, expected: usize) {
    let deadline = Instant::now() + WAIT;
    while worktree_count(repository) != expected {
        assert!(
            Instant::now() < deadline,
            "worktree count stayed {} instead of {expected}",
            worktree_count(repository)
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
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
    wait_for_worktree_count(&repository, 2);
    assert!(branch_exists(&repository, "clean"));
    assert!(dirty.join("notes.txt").exists());
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
    wait_for_worktree_count(&repository, 2);
    assert!(theirs.join("README.md").exists());
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

fn trash_entries(repository: &Path) -> Vec<(std::path::PathBuf, serde_json::Value)> {
    let trash = repository.join(".kratos-trash");
    let Ok(entries) = std::fs::read_dir(&trash) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            let meta: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let dir = path.with_extension("");
            found.push((dir, meta));
        }
    }
    found
}

#[test]
fn a_clean_keep_archive_moves_the_worktree_to_trash_and_keeps_repo_status_clean() {
    // Catches Keep deleting a clean worktree outright instead of moving it to `.kratos-trash`,
    // lengthening its path past the original worktree path, or dirtying the project's `git status`.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let keep_thread = create(&client, &workspace, "w", PlaceRequest::NewWorktree);
    let keep_wt = worktree_of(&client, &keep_thread);
    let keep_folder = Path::new(&keep_wt.path);
    std::fs::write(keep_folder.join("committed.txt"), "saved\n").unwrap();
    git(keep_folder, &["add", "committed.txt"]);
    git(keep_folder, &["commit", "--quiet", "--message", "saved"]);

    assert_eq!(
        archive(&client, &keep_thread, LeftFiles::Keep),
        Cleanup::Removed
    );
    assert!(!keep_folder.exists());
    assert_eq!(worktree_count(&repository), 1);
    assert_eq!(git(&repository, &["status", "--porcelain"]), "");
    assert_eq!(git(&repository, &["show", "w:committed.txt"]), "saved");

    let trashed = trash_entries(&repository);
    assert_eq!(trashed.len(), 1, "{trashed:?}");
    let (trash_dir, meta) = &trashed[0];
    assert!(
        trash_dir.to_string_lossy().len() <= keep_wt.path.len(),
        "trash path {} is longer than worktree path {}",
        trash_dir.display(),
        keep_wt.path
    );
    assert_eq!(meta["worktreeId"].as_str(), Some(keep_wt.id.as_str()));
    assert_eq!(meta["originalPath"].as_str(), Some(keep_wt.path.as_str()));
    assert_eq!(meta["branch"].as_str(), Some("w"));
    assert!(meta["trashedAt"].as_str().is_some());
    assert_eq!(
        std::fs::read_to_string(trash_dir.join("committed.txt")).unwrap(),
        "saved\n"
    );
    assert_eq!(
        std::fs::read_to_string(trash_dir.join(".git")).unwrap(),
        "gitdir: .kratos-archived-worktree\n"
    );

    // Discard deletes immediately and leaves no new trash entry.
    let discard_thread = create(&client, &workspace, "drop", PlaceRequest::NewWorktree);
    let discard_wt = worktree_of(&client, &discard_thread);
    std::fs::write(Path::new(&discard_wt.path).join("scratch.txt"), "bye\n").unwrap();
    assert_eq!(
        archive(&client, &discard_thread, LeftFiles::Discard),
        Cleanup::Removed
    );
    assert!(!Path::new(&discard_wt.path).exists());
    assert_eq!(trash_entries(&repository).len(), 1);
}

#[cfg(unix)]
#[test]
fn an_open_directory_save_after_keep_check_survives_in_trash() {
    // Catches Keep deleting saves written through an already-open directory handle (a shell or
    // editor inside the worktree) after Kratos's last clean scan.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    std::fs::write(repository.join(".gitignore"), "*.local\n").unwrap();
    git(&repository, &["add", ".gitignore"]);
    git(
        &repository,
        &["commit", "--quiet", "--message", "ignore local"],
    );
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "work", PlaceRequest::NewWorktree);
    let wt = worktree_of(&client, &thread);
    let ready = root.path().join("writer-ready");
    let trigger = root.path().join("writer-go");
    let done = root.path().join("writer-done");
    let mut writer = std::process::Command::new("sh")
        .current_dir(&wt.path)
        .args([
            "-c",
            "touch \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.01; done; \
             printf 'late tracked save\\n' > README.md; \
             printf 'late ignored save\\n' > saved.local; \
             touch \"$3\"",
            "open-dir-writer",
            ready.to_str().unwrap(),
            trigger.to_str().unwrap(),
            done.to_str().unwrap(),
        ])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + WAIT;
    while !ready.exists() {
        assert!(Instant::now() < deadline, "open-dir writer never opened fd");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
    std::fs::write(&trigger, "").unwrap();
    assert!(writer.wait().unwrap().success());
    assert!(done.exists());

    let trashed = trash_entries(&repository);
    assert_eq!(trashed.len(), 1, "{trashed:?}");
    let (trash_dir, _) = &trashed[0];
    assert_eq!(
        std::fs::read_to_string(trash_dir.join("README.md")).unwrap(),
        "late tracked save\n"
    );
    assert_eq!(
        std::fs::read_to_string(trash_dir.join("saved.local")).unwrap(),
        "late ignored save\n"
    );
}

#[cfg(windows)]
#[test]
fn a_windows_long_path_worktree_archives_cleanly_under_keep_and_discard() {
    // Catches quarantine names or `git worktree remove` pushing valid <=240-char Windows paths
    // past MAX_PATH with `core.longpaths=false` and leaving a broken checkout.
    for files in [LeftFiles::Keep, LeftFiles::Discard] {
        let root = TempDir::new().unwrap();
        let repository = new_repository(&root);
        git(&repository, &["config", "core.longpaths", "false"]);
        git(&repository, &["config", "core.autocrlf", "false"]);
        let prefix = repository.join(".kratos-worktrees").join("w");
        let leaf_len = 240usize.saturating_sub(prefix.to_string_lossy().len() + 1);
        assert!(leaf_len >= 16, "unexpected temp path: {}", prefix.display());
        let leaf = "p".repeat(leaf_len);
        std::fs::write(repository.join(&leaf), "tracked contents\n").unwrap();
        git(&repository, &["add", &leaf]);
        git(
            &repository,
            &["commit", "--quiet", "--message", "long path"],
        );
        let engine = RunningEngine::start(&root.path().join("kratos"));
        let workspace = workspace_at(&engine, &repository);
        let client = engine.client();
        let thread = create(&client, &workspace, "w", PlaceRequest::NewWorktree);
        let path = worktree_of(&client, &thread).path;
        let folder = Path::new(&path);
        assert_eq!(
            std::fs::read_to_string(folder.join(&leaf)).unwrap().trim(),
            "tracked contents"
        );

        assert_eq!(archive(&client, &thread, files), Cleanup::Removed);
        assert!(!folder.exists());
        assert_eq!(worktree_count(&repository), 1);
        assert_eq!(archive(&client, &thread, files), Cleanup::Removed);
        if files == LeftFiles::Keep {
            let trashed = trash_entries(&repository);
            assert_eq!(trashed.len(), 1);
            assert_eq!(
                std::fs::read_to_string(trashed[0].0.join(&leaf))
                    .unwrap()
                    .trim(),
                "tracked contents"
            );
        } else {
            assert!(trash_entries(&repository).is_empty());
        }
    }
}

#[test]
fn a_crash_after_moving_to_trash_finishes_cleanly_with_or_without_a_replacement_checkout() {
    // Catches restart recovery failing to remove the old admin record or disarm `<slot>/.git`, or
    // confusing a replacement checkout at the vacated path with the trashed worktree.
    for with_replacement in [false, true] {
        let root = TempDir::new().unwrap();
        let repository = new_repository(&root);
        let home = root.path().join("kratos");
        let engine = RunningEngine::start(&home);
        let workspace = workspace_at(&engine, &repository);
        let client = engine.client();
        let thread = create(&client, &workspace, "work", PlaceRequest::NewWorktree);
        let wt = worktree_of(&client, &thread);
        let folder = Path::new(&wt.path);
        let admin = std::path::PathBuf::from(git(folder, &["rev-parse", "--absolute-git-dir"]));
        engine.kill();

        // Simulate a crash right after `begin_removal`, write-ahead `<slot>.json`, and atomic
        // rename into `.kratos-trash/<slot>`, before removing `admin` or disarming `<slot>/.git`.
        let database = rusqlite::Connection::open(home.join("kratos.db")).unwrap();
        database
            .execute(
                "UPDATE threads SET archived_at = created_at WHERE id = ?1",
                [thread.id.as_str()],
            )
            .unwrap();
        database
            .execute(
                "UPDATE worktrees SET state = 'removing' WHERE id = ?1",
                [wt.id.as_str()],
            )
            .unwrap();
        drop(database);
        let slot: String = wt
            .id
            .as_str()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(5)
            .collect::<String>()
            .to_ascii_lowercase();
        let trash_dir = repository.join(".kratos-trash");
        std::fs::create_dir_all(&trash_dir).unwrap();
        std::fs::write(trash_dir.join(".gitignore"), "*\n").unwrap();
        std::fs::write(
            trash_dir.join(format!("{slot}.json")),
            json!({
                "worktreeId": wt.id.as_str(),
                "originalPath": wt.path,
                "branch": "work",
                "trashedAt": "2026-11-21T00:00:00.000Z"
            })
            .to_string(),
        )
        .unwrap();
        let moved = trash_dir.join(&slot);
        std::fs::rename(folder, &moved).unwrap();
        std::fs::write(moved.join("saved.local"), "late save after rename\n").unwrap();
        if with_replacement {
            git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "--force",
                    "--force",
                    "--quiet",
                    "-b",
                    "replacement",
                    &wt.path,
                ],
            );
            std::fs::write(folder.join("replacement.txt"), "user replacement\n").unwrap();
        }

        let restarted = RunningEngine::start(&home);
        let client = restarted.client();
        let expected_worktrees = if with_replacement { 2 } else { 1 };
        wait_for_worktree_count(&repository, expected_worktrees);

        for _ in 0..2 {
            assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
        }
        let our_admin_still_present = std::fs::read_dir(repository.join(".git").join("worktrees"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                std::fs::read_to_string(entry.path().join("kratos-worktree-id"))
                    .ok()
                    .is_some_and(|mark| mark.trim() == wt.id.as_str())
            });
        assert!(!our_admin_still_present, "old admin dir still registered");
        assert_eq!(
            std::fs::read_to_string(moved.join("saved.local")).unwrap(),
            "late save after rename\n"
        );
        assert!(trash_dir.join(format!("{slot}.json")).is_file());
        assert_eq!(
            std::fs::read_to_string(moved.join(".git")).unwrap(),
            "gitdir: .kratos-archived-worktree\n"
        );
        if with_replacement {
            assert_eq!(
                std::fs::read_to_string(folder.join("replacement.txt")).unwrap(),
                "user replacement\n"
            );
            assert_eq!(git(folder, &["branch", "--show-current"]), "replacement");
        } else {
            assert!(!admin.exists(), "old admin dir still registered");
            assert!(!folder.exists());
        }
    }
}

#[test]
fn expired_trash_is_emptied_on_startup_without_deleting_recent_saves() {
    // Catches trash accumulating forever, or startup emptying trash that is still within the
    // 7-day retention window based on `trashedAt`.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let home = root.path().join("kratos");
    let engine = RunningEngine::start(&home);
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    for title in ["expired", "fresh"] {
        let thread = create(&client, &workspace, title, PlaceRequest::NewWorktree);
        assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
    }
    drop(engine);

    let mut expired_dir = None;
    let mut fresh_dir = None;
    for (dir, mut meta) in trash_entries(&repository) {
        if meta["branch"].as_str() == Some("expired") {
            meta["trashedAt"] = json!("2020-01-01T00:00:00.000Z");
            std::fs::write(
                dir.with_extension("json"),
                serde_json::to_vec(&meta).unwrap(),
            )
            .unwrap();
            expired_dir = Some(dir);
        } else {
            fresh_dir = Some(dir);
        }
    }
    let expired_dir = expired_dir.unwrap();
    let fresh_dir = fresh_dir.unwrap();

    let _restarted = RunningEngine::start(&home);

    wait_until_gone(&expired_dir);
    assert!(!expired_dir.with_extension("json").exists());
    assert!(fresh_dir.exists());
}

#[test]
fn git_in_a_trashed_worktree_fails_and_never_touches_the_project() {
    // Catches Keep removing `<slot>/.git` so a terminal left open inside the trashed worktree (or
    // a subdirectory of it) walks up to the project's repository and commits project edits.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    std::fs::create_dir_all(repository.join("nested")).unwrap();
    std::fs::write(repository.join("nested").join("file.txt"), "tracked\n").unwrap();
    git(&repository, &["add", "nested/file.txt"]);
    git(
        &repository,
        &["commit", "--quiet", "--message", "add nested"],
    );
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "work", PlaceRequest::NewWorktree);
    std::fs::write(
        repository.join("README.md"),
        "unfinished edit in the project\n",
    )
    .unwrap();
    let project_head = git(&repository, &["rev-parse", "HEAD"]);

    assert_eq!(archive(&client, &thread, LeftFiles::Keep), Cleanup::Removed);
    let trashed = trash_entries(&repository);
    assert_eq!(trashed.len(), 1, "{trashed:?}");
    let slot = &trashed[0].0;

    for dir in [slot.clone(), slot.join("nested")] {
        for args in [
            &["rev-parse", "--show-toplevel"][..],
            &["commit", "--quiet", "-am", "typed in old worktree terminal"],
        ] {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "git {args:?} in {} unexpectedly succeeded: {}",
                dir.display(),
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(
                stderr.to_ascii_lowercase().contains("not a git repository"),
                "expected 'not a git repository' from git {args:?} in {}, got: {stderr}",
                dir.display()
            );
        }
    }
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), project_head);
    assert_eq!(
        std::fs::read_to_string(repository.join("README.md")).unwrap(),
        "unfinished edit in the project\n"
    );
    assert_eq!(git(&repository, &["status", "--porcelain"]), "M README.md");
}
