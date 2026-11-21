//! A thread's finished work (R5, D27, D55): its diff, a commit, and a pull request, against real
//! git in temporary repositories and a stand-in `gh`.

use std::path::Path;
use std::time::Instant;

use fake_agents::codex_control;
use kratos_api_client::{ApiReply, Client};
use kratos_protocol::{
    ArchiveThread, CommitThread, Committed, CreatePullRequest, LeftFiles, PlaceRequest,
    PullRequest, RequiredText, Thread, ThreadDetails, ThreadDiff, Workspace, routes,
};
use tempfile::TempDir;

use super::running_engine::{Gate, RunningEngine, WAIT, post};
use super::thread_places::{
    create, existing, git, new_repository, take_place, workspace_at, worktree_of,
};

fn diff(client: &Client, thread: &Thread) -> ThreadDiff {
    client
        .get(&routes::thread_diff(&thread.id))
        .unwrap()
        .parse()
        .unwrap()
}

fn commit(client: &Client, thread: &Thread, message: &str) -> ApiReply {
    let request = CommitThread {
        message: RequiredText::new(message).unwrap(),
    };
    client
        .post(&routes::thread_commit(&thread.id), &request)
        .unwrap()
}

#[test]
fn a_worktree_diff_shows_everything_since_its_base_and_leaves_the_users_staging_alone() {
    // Catches a diff that misses the agent's own commits or its new files, shows files git
    // ignores, or stages files in the user's index while it looks.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    std::fs::write(repository.join(".gitignore"), "*.log\n").unwrap();
    git(&repository, &["add", ".gitignore"]);
    git(
        &repository,
        &["commit", "--quiet", "--message", "ignore logs"],
    );
    let head = git(&repository, &["rev-parse", "HEAD"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "work", PlaceRequest::NewWorktree);
    let worktree = worktree_of(&client, &thread);
    let folder = Path::new(&worktree.path);
    std::fs::write(folder.join("committed.txt"), "by the agent\n").unwrap();
    git(folder, &["add", "committed.txt"]);
    git(folder, &["commit", "--quiet", "--message", "agent commit"]);
    std::fs::write(folder.join("README.md"), "changed\n").unwrap();
    std::fs::write(folder.join("new.txt"), "brand new\n").unwrap();
    std::fs::write(folder.join("build.log"), "noise\n").unwrap();
    let status = git(folder, &["status", "--porcelain"]);

    let diff = diff(&client, &thread);

    assert_eq!(worktree.base.as_deref(), Some(head.as_str()));
    assert_eq!(diff.base, head);
    for shown in ["+by the agent", "+changed", "+brand new"] {
        assert!(
            diff.patch.contains(shown),
            "{shown} missing:\n{}",
            diff.patch
        );
    }
    assert!(!diff.patch.contains("build.log"), "{}", diff.patch);
    assert!(!diff.truncated);
    assert_eq!(git(folder, &["status", "--porcelain"]), status);
}

#[test]
fn a_project_folder_diff_starts_at_its_head() {
    // Catches a project-folder thread whose diff is empty or starts somewhere other than the
    // commit the user has checked out.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "chat", PlaceRequest::default());
    std::fs::write(repository.join("README.md"), "edited in place\n").unwrap();

    let diff = diff(&client, &thread);

    assert_eq!(diff.base, git(&repository, &["rev-parse", "HEAD"]));
    assert!(diff.patch.contains("+edited in place"), "{}", diff.patch);
}

#[test]
fn a_diff_is_a_patch_git_can_apply() {
    // Catches a patch cut short of its last newline, which git rejects as corrupt.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "chat", PlaceRequest::default());
    std::fs::write(repository.join("README.md"), "edited in place\n").unwrap();

    let patch = root.path().join("change.patch");
    std::fs::write(&patch, diff(&client, &thread).patch).unwrap();

    git(
        &repository,
        &["apply", "--reverse", "--check", patch.to_str().unwrap()],
    );
}

#[test]
fn a_diff_past_the_limit_ends_at_a_whole_line_and_says_it_was_cut() {
    // Catches a cut in the middle of a line, or a long diff returned whole.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "chat", PlaceRequest::default());
    let line = format!("{}\n", "x".repeat(99));
    std::fs::write(repository.join("big.txt"), line.repeat(60_000)).unwrap();

    let diff = diff(&client, &thread);

    assert!(diff.truncated);
    assert!(diff.patch.len() <= 4 * 1024 * 1024, "{}", diff.patch.len());
    assert!(
        diff.patch.ends_with(&format!("+{line}")),
        "the patch ends mid-line"
    );
}

#[test]
fn a_diff_of_text_that_is_not_utf8_stays_within_the_limit_once_decoded() {
    // Catches the limit applied to git's bytes only: each byte that is not UTF-8 becomes a
    // three-byte replacement character, so 2 MiB of Latin-1 became a 6 MiB patch.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "chat", PlaceRequest::default());
    let line = [&[0xE9_u8; 63][..], b"\n"].concat();
    std::fs::write(repository.join("latin1.txt"), line.repeat(32_768)).unwrap();

    let diff = diff(&client, &thread);

    assert!(diff.patch.len() <= 4 * 1024 * 1024, "{}", diff.patch.len());
    assert!(diff.truncated);
    assert!(diff.patch.ends_with('\n'), "the patch ends mid-line");
}

#[test]
fn a_project_in_a_subfolder_diffs_and_commits_only_that_folder() {
    // Catches a diff or commit that takes in the rest of the repository, including a change the
    // user staged outside the project.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let component = repository.join("component");
    std::fs::create_dir(&component).unwrap();
    std::fs::write(component.join("inside.txt"), "one\n").unwrap();
    std::fs::write(repository.join("staged.txt"), "one\n").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "--quiet", "--message", "parts"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &component);
    let client = engine.client();
    let thread = create(&client, &workspace, "chat", PlaceRequest::default());
    std::fs::write(component.join("inside.txt"), "two\n").unwrap();
    std::fs::write(repository.join("outside.txt"), "not the project's\n").unwrap();
    std::fs::write(repository.join("staged.txt"), "the user's\n").unwrap();
    git(&repository, &["add", "staged.txt"]);

    let patch = diff(&client, &thread).patch;
    let reply = commit(&client, &thread, "Change the component");

    assert!(patch.contains("component/inside.txt"), "{patch}");
    assert!(
        !patch.contains("outside.txt") && !patch.contains("staged.txt"),
        "{patch}"
    );
    assert_eq!(reply.status, 201, "{}", reply.body);
    let committed = git(&repository, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(committed, "component/inside.txt");
    assert_eq!(
        git(&repository, &["status", "--porcelain"]),
        "M  staged.txt\n?? outside.txt"
    );
}

#[test]
fn a_commit_at_the_top_of_the_repository_concludes_a_merge() {
    // Catches a commit that names its folder to git even when the folder is the whole checkout,
    // which git refuses during a merge as a partial commit.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    git(&repository, &["switch", "--quiet", "--create", "feature"]);
    std::fs::write(repository.join("feature.txt"), "feature\n").unwrap();
    git(&repository, &["add", "feature.txt"]);
    git(&repository, &["commit", "--quiet", "--message", "feature"]);
    git(&repository, &["switch", "--quiet", "main"]);
    git(
        &repository,
        &["merge", "--quiet", "--no-ff", "--no-commit", "feature"],
    );
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "merge", PlaceRequest::default());

    let reply = commit(&client, &thread, "Merge the feature");

    assert_eq!(reply.status, 201, "{}", reply.body);
    let parents = git(
        &repository,
        &["rev-list", "--parents", "--max-count=1", "HEAD"],
    );
    assert_eq!(parents.split(' ').count(), 3, "{parents}");
    assert!(!repository.join(".git").join("MERGE_HEAD").exists());
}

#[test]
fn a_commit_at_the_top_concludes_a_merge_that_changes_no_file() {
    // Catches the check for staged changes refusing a merge whose result equals HEAD, as when
    // both branches made the same change, which git still records as a commit with two parents.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    git(&repository, &["switch", "--quiet", "--create", "feature"]);
    std::fs::write(repository.join("same.txt"), "same\n").unwrap();
    git(&repository, &["add", "same.txt"]);
    git(&repository, &["commit", "--quiet", "--message", "feature"]);
    git(&repository, &["switch", "--quiet", "main"]);
    std::fs::write(repository.join("same.txt"), "same\n").unwrap();
    git(&repository, &["add", "same.txt"]);
    git(&repository, &["commit", "--quiet", "--message", "main"]);
    git(
        &repository,
        &["merge", "--quiet", "--no-ff", "--no-commit", "feature"],
    );
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "merge", PlaceRequest::default());

    let reply = commit(&client, &thread, "Merge the feature");

    assert_eq!(reply.status, 201, "{}", reply.body);
    let parents = git(
        &repository,
        &["rev-list", "--parents", "--max-count=1", "HEAD"],
    );
    assert_eq!(parents.split(' ').count(), 3, "{parents}");
    assert!(!repository.join(".git").join("MERGE_HEAD").exists());
}

#[test]
fn commits_sent_together_make_one_commit_and_the_rest_find_nothing_to_commit() {
    // Catches two git commands racing for the repository's index lock, which git answers with an
    // error that tells the user to delete the lock file.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "chat", PlaceRequest::default());
    std::fs::write(repository.join("README.md"), "changed\n").unwrap();

    let senders: Vec<_> = (0..5)
        .map(|_| {
            let (client, thread) = (client.clone(), thread.clone());
            std::thread::spawn(move || commit(&client, &thread, "Change the readme"))
        })
        .collect();
    let mut statuses: Vec<u16> = senders
        .into_iter()
        .map(|sender| {
            let reply = sender.join().unwrap();
            assert!(reply.status != 500, "{}", reply.body);
            reply.status
        })
        .collect();
    statuses.sort_unstable();

    assert_eq!(statuses, [201, 400, 400, 400, 400]);
}

#[test]
fn a_commit_is_the_users_own_with_the_message_as_given_and_runs_no_hook() {
    // Catches a commit under another identity, a trailer or signature line added to the message,
    // a hook an agent planted running outside every sandbox, or an empty commit.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let marker = root.path().join("hook-ran");
    let hook = repository.join(".git").join("hooks").join("pre-commit");
    let marker_text = marker.to_str().unwrap().replace('\\', "/");
    std::fs::write(&hook, format!("#!/bin/sh\necho ran > '{marker_text}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "fix", PlaceRequest::NewWorktree);
    let folder = worktree_of(&client, &thread).path;
    let folder = Path::new(&folder);
    std::fs::write(folder.join("README.md"), "fixed\n").unwrap();
    std::fs::write(folder.join("new.txt"), "new\n").unwrap();

    let reply = commit(&client, &thread, "Fix the login bug");

    assert_eq!(reply.status, 201, "{}", reply.body);
    let Committed { commit: made } = reply.parse().unwrap();
    let shown = git(folder, &["log", "-1", "--format=%H%n%an <%ae>%n%B"]);
    assert_eq!(
        shown,
        format!("{made}\nTest <test@example.com>\nFix the login bug")
    );
    assert_eq!(git(folder, &["status", "--porcelain"]), "");
    assert!(
        !marker.exists(),
        "Kratos's commit ran the repository's hook"
    );
    let again = commit(&client, &thread, "Nothing new");
    assert_eq!(again.status, 400, "{}", again.body);
    let message = again.body["error"].as_str().unwrap();
    assert!(message.contains("nothing to commit"), "{message}");
}

#[test]
fn a_commit_waits_for_the_running_turn() {
    // Catches a commit taken while the agent may be halfway through an edit.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "busy", PlaceRequest::default());
    std::fs::write(repository.join("README.md"), "half done\n").unwrap();
    post(&client, &thread, "keep going");
    engine.start_turn();
    wait_for_turn(&client, &thread);

    let refused = commit(&client, &thread, "Too early");

    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(git(&repository, &["rev-list", "--count", "HEAD"]), "1");
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
}

/// A repository with a committed `component` folder, and an engine.
fn repository_with_component(root: &TempDir) -> (std::path::PathBuf, RunningEngine) {
    let repository = repository_with_component_only(root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    (repository, engine)
}

/// A repository with a committed `component` folder that holds `part.txt`.
fn repository_with_component_only(root: &TempDir) -> std::path::PathBuf {
    let repository = new_repository(root);
    let component = repository.join("component");
    std::fs::create_dir(&component).unwrap();
    std::fs::write(component.join("part.txt"), "one\n").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "--quiet", "--message", "part"]);
    repository
}

#[test]
fn a_commit_waits_for_a_turn_in_a_worktree_it_shares_but_not_in_another_checkout() {
    // Catches an idle thread committing another thread's half-done edit in a worktree they
    // share, and a turn in a separate checkout holding a commit up.
    let root = TempDir::new().unwrap();
    let (repository, engine) = repository_with_component(&root);
    let workspace = workspace_at(&engine, &repository);
    let inner = workspace_at(&engine, &repository.join("component"));
    let client = engine.client();
    let busy = create(&client, &workspace, "busy", PlaceRequest::NewWorktree);
    let shared = worktree_of(&client, &busy).path;
    let sharer = create(&client, &workspace, "sharer", existing(Path::new(&shared)));
    let in_component = create(&client, &inner, "component", PlaceRequest::default());
    std::fs::write(
        Path::new(&shared).join("README.md"),
        "first half
",
    )
    .unwrap();
    std::fs::write(
        repository.join("component/part.txt"),
        "two
",
    )
    .unwrap();
    post(&client, &busy, "keep going");
    engine.start_turn();
    wait_for_turn(&client, &busy);

    assert_eq!(commit(&client, &sharer, "Too early").status, 409);
    assert_eq!(
        git(Path::new(&shared), &["log", "-1", "--format=%s"]),
        "part"
    );
    assert_eq!(commit(&client, &in_component, "Separate").status, 201);
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
}

#[test]
fn a_commit_waits_for_a_turn_in_a_project_folder_that_holds_it() {
    // Catches a thread in a subfolder committing the half-done edit of a thread that works in
    // the whole repository.
    let root = TempDir::new().unwrap();
    let (repository, engine) = repository_with_component(&root);
    let workspace = workspace_at(&engine, &repository);
    let inner = workspace_at(&engine, &repository.join("component"));
    let client = engine.client();
    let busy = create(&client, &workspace, "top", PlaceRequest::default());
    let in_component = create(&client, &inner, "component", PlaceRequest::default());
    std::fs::write(
        repository.join("component/part.txt"),
        "first half
",
    )
    .unwrap();
    post(&client, &busy, "keep going");
    engine.start_turn();
    wait_for_turn(&client, &busy);

    assert_eq!(commit(&client, &in_component, "Too early").status, 409);
    assert_eq!(git(&repository, &["log", "-1", "--format=%s"]), "part");
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
}

#[test]
fn a_commit_through_another_path_to_the_folder_still_waits_for_its_turn() {
    // Catches folders compared as the user typed them: a project added through `..` names the
    // same folder, but the turn running there went unseen and the commit took its edit.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let sibling = root.path().join("sibling");
    std::fs::create_dir(&sibling).unwrap();
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let aliased = workspace_at(&engine, &sibling.join("..").join("repo"));
    let client = engine.client();
    let busy = create(&client, &workspace, "busy", PlaceRequest::default());
    let idle = create(&client, &aliased, "idle", PlaceRequest::default());
    std::fs::write(repository.join("half.txt"), "unfinished\n").unwrap();
    post(&client, &busy, "keep going");
    engine.start_turn();
    wait_for_turn(&client, &busy);

    let refused = commit(&client, &idle, "Through the alias");

    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(git(&repository, &["rev-list", "--count", "HEAD"]), "1");
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
}

#[test]
fn a_commit_that_waited_for_the_repository_checks_for_turns_once_it_has_it() {
    // Catches a check made before the commit waits behind another git command of the repository,
    // here a pull request's push: a turn that starts during the wait goes unseen.
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
    let busy = create(&client, &workspace, "busy", PlaceRequest::default());
    let idle = create(&client, &workspace, "idle", PlaceRequest::default());
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
    let committing = {
        let (client, idle) = (client.clone(), idle.clone());
        std::thread::spawn(move || commit(&client, &idle, "After the wait"))
    };
    // Long enough for the commit to reach the repository's lock and wait there.
    std::thread::sleep(std::time::Duration::from_secs(1));
    post(&client, &busy, "keep going");
    engine.start_turn();
    wait_for_turn(&client, &busy);
    std::fs::write(repository.join("half.txt"), "unfinished\n").unwrap();
    engine.release(Gate::Gh);

    let refused = committing.join().unwrap();
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(git(&repository, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(pull_request.join().unwrap(), 201);
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
}

/// A bare `origin` for `repository` with its `main` pushed, as a real origin has.
fn with_origin(root: &TempDir, repository: &Path) -> std::path::PathBuf {
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
    git(repository, &["remote", "add", "origin", remote_text]);
    git(repository, &["push", "--quiet", "origin", "main"]);
    remote
}

#[test]
fn a_commit_that_waited_while_another_checkout_took_its_worktrees_place_is_refused() {
    // Catches a commit that checks its worktree only before it waits for the repository, so a
    // checkout that takes the path meanwhile gets the old thread's commit.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    with_origin(&root, &repository);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let reviewed = create(&client, &workspace, "reviewed", PlaceRequest::NewWorktree);
    let old = create(&client, &workspace, "old", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &old).path;
    std::fs::write(Path::new(&path).join("old.txt"), "the old thread's\n").unwrap();
    engine.hold(Gate::Gh);
    let pull_request = {
        let (client, reviewed) = (client.clone(), reviewed.clone());
        std::thread::spawn(move || open_pull_request(&client, &reviewed).status)
    };
    engine.wait_until_held(Gate::Gh);
    let committing = {
        let (client, old) = (client.clone(), old.clone());
        std::thread::spawn(move || commit(&client, &old, "Old thread commit"))
    };
    // Long enough for the commit to reach the repository's lock and wait there.
    std::thread::sleep(std::time::Duration::from_secs(1));
    take_place(&repository, &path, &root.path().join("moved"));
    engine.release(Gate::Gh);

    let refused = committing.join().unwrap();
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(
        git(Path::new(&path), &["log", "-1", "--format=%s"]),
        "first"
    );
    assert_eq!(pull_request.join().unwrap(), 201);
}

#[test]
fn a_pull_request_whose_worktree_another_checkout_took_while_it_ran_is_refused() {
    // Catches a pull request that reads the branch to push from whatever checkout stands at the
    // worktree's path once its check passed, and so publishes another checkout's branch.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let remote = with_origin(&root, &repository);
    let engine = RunningEngine::start_holding_git(&root.path().join("kratos"), "symbolic-ref");
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "proposal", PlaceRequest::NewWorktree);
    let path = worktree_of(&client, &thread).path;
    engine.hold(Gate::Git);
    let pull_request = {
        let (client, thread) = (client.clone(), thread.clone());
        std::thread::spawn(move || open_pull_request(&client, &thread))
    };
    engine.wait_until_held(Gate::Git);
    take_place(&repository, &path, &root.path().join("moved"));
    engine.release(Gate::Git);

    let refused = pull_request.join().unwrap();
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(git(&remote, &["branch", "--list", "users-branch"]), "");
    assert!(engine.fake_gh_runs().is_empty());
}

fn open_pull_request(client: &Client, thread: &Thread) -> ApiReply {
    let request = CreatePullRequest {
        title: RequiredText::new("Review").unwrap(),
        body: String::new(),
    };
    client
        .post(&routes::thread_pull_request(&thread.id), &request)
        .unwrap()
}

#[test]
fn a_turn_that_starts_while_a_commit_stages_files_stops_it_and_leaves_the_index_alone() {
    // Catches a commit that checks for turns only before it stages: a turn that starts while
    // `git add` reads the files may have half-written one of them.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let gate = root.path().join("filter-gate");
    std::fs::create_dir(&gate).unwrap();
    let gate_text = gate.to_str().unwrap().replace('\\', "/");
    // Git runs the clean filter on each file it stages, so this holds `git add` until the test
    // has started a turn.
    git(
        &repository,
        &[
            "config",
            "filter.held.clean",
            &format!(
                "touch '{gate_text}/staging' && \
                 while [ ! -f '{gate_text}/go' ]; do sleep 0.05; done && cat"
            ),
        ],
    );
    std::fs::write(
        repository.join(".git").join("info").join("attributes"),
        "half.txt filter=held\n",
    )
    .unwrap();
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let busy = create(&client, &workspace, "busy", PlaceRequest::default());
    let idle = create(&client, &workspace, "idle", PlaceRequest::default());
    std::fs::write(repository.join("half.txt"), "unfinished\n").unwrap();
    let committing = {
        let (client, idle) = (client.clone(), idle.clone());
        std::thread::spawn(move || commit(&client, &idle, "While staging"))
    };
    let deadline = Instant::now() + WAIT;
    while !gate.join("staging").exists() {
        assert!(Instant::now() < deadline, "git never staged half.txt");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    post(&client, &busy, "keep going");
    engine.start_turn();
    wait_for_turn(&client, &busy);
    std::fs::write(gate.join("go"), "").unwrap();

    let refused = committing.join().unwrap();
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(git(&repository, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(git(&repository, &["status", "--porcelain"]), "?? half.txt");
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
}

#[test]
fn a_folder_commit_takes_its_files_as_staged_and_not_as_a_later_turn_rewrites_them() {
    // Catches a commit of a folder inside the repository that git reads from disk again once the
    // check for turns has passed, so a turn that starts then has its half-written file committed.
    let root = TempDir::new().unwrap();
    let repository = repository_with_component_only(&root);
    let engine = RunningEngine::start_holding_git(&root.path().join("kratos"), "commit");
    let inner = workspace_at(&engine, &repository.join("component"));
    let client = engine.client();
    let idle = create(&client, &inner, "idle", PlaceRequest::default());
    let editor = create(&client, &inner, "editor", PlaceRequest::default());
    let part = repository.join("component").join("part.txt");
    std::fs::write(&part, "complete edit\n").unwrap();
    engine.hold(Gate::Git);
    let committing = {
        let (client, idle) = (client.clone(), idle.clone());
        std::thread::spawn(move || commit(&client, &idle, "Finished part"))
    };
    engine.wait_until_held(Gate::Git);
    post(&client, &editor, "keep going");
    engine.start_turn();
    wait_for_turn(&client, &editor);
    std::fs::write(&part, "half written\n").unwrap();
    engine.release(Gate::Git);

    let reply = committing.join().unwrap();
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(
        git(&repository, &["show", "HEAD:component/part.txt"]),
        "complete edit"
    );
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
}

#[test]
fn a_refused_commit_leaves_what_the_user_staged_meanwhile() {
    // Catches a refused commit that puts back a copy of the whole index taken before it staged,
    // which drops a version the user staged meanwhile and then edited further on disk.
    let root = TempDir::new().unwrap();
    let repository = repository_with_component_only(&root);
    let engine = RunningEngine::start_holding_git(&root.path().join("kratos"), "add");
    let inner = workspace_at(&engine, &repository.join("component"));
    let client = engine.client();
    let idle = create(&client, &inner, "idle", PlaceRequest::default());
    let editor = create(&client, &inner, "editor", PlaceRequest::default());
    std::fs::write(repository.join("component").join("part.txt"), "two\n").unwrap();
    engine.hold(Gate::Git);
    let committing = {
        let (client, idle) = (client.clone(), idle.clone());
        std::thread::spawn(move || commit(&client, &idle, "Two"))
    };
    engine.wait_until_held(Gate::Git);
    let readme = repository.join("README.md");
    std::fs::write(&readme, "staged by the user\n").unwrap();
    git(&repository, &["add", "README.md"]);
    std::fs::write(&readme, "edited after staging\n").unwrap();
    post(&client, &editor, "keep going");
    engine.start_turn();
    wait_for_turn(&client, &editor);
    engine.release(Gate::Git);

    let refused = committing.join().unwrap();
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(
        git(&repository, &["show", ":README.md"]),
        "staged by the user"
    );
    assert_eq!(
        std::fs::read_to_string(&readme).unwrap(),
        "edited after staging\n"
    );
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
}

#[test]
fn a_commit_the_user_makes_while_a_folder_commit_stages_files_is_not_undone() {
    // Catches a folder commit whose files outside the folder come from HEAD as it was before it
    // staged, put on top of the commit the user made meanwhile, which undoes that commit.
    let root = TempDir::new().unwrap();
    let repository = repository_with_component_only(&root);
    let engine = RunningEngine::start_holding_git(&root.path().join("kratos"), "add");
    let inner = workspace_at(&engine, &repository.join("component"));
    let client = engine.client();
    let thread = create(&client, &inner, "chat", PlaceRequest::default());
    std::fs::write(repository.join("component").join("part.txt"), "two\n").unwrap();
    engine.hold(Gate::Git);
    let committing = {
        let (client, thread) = (client.clone(), thread.clone());
        std::thread::spawn(move || commit(&client, &thread, "Two"))
    };
    engine.wait_until_held(Gate::Git);
    std::fs::write(repository.join("README.md"), "the user's\n").unwrap();
    git(&repository, &["commit", "--quiet", "-am", "The user's"]);
    let users = git(&repository, &["rev-parse", "HEAD"]);
    engine.release(Gate::Git);

    let refused = committing.join().unwrap();
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), users);
    assert!(!repository.join(".git").join("index.lock").exists());
}

#[test]
fn a_merge_the_user_starts_while_a_folder_commit_stages_files_is_left_to_them() {
    // Catches a commit that looks for a merge in progress only before it stages, so it concludes
    // one the user starts meanwhile without the merged files outside the folder.
    let root = TempDir::new().unwrap();
    let repository = repository_with_component_only(&root);
    git(&repository, &["switch", "--quiet", "--create", "feature"]);
    std::fs::write(repository.join("feature.txt"), "feature\n").unwrap();
    git(&repository, &["add", "feature.txt"]);
    git(&repository, &["commit", "--quiet", "--message", "feature"]);
    git(&repository, &["switch", "--quiet", "main"]);
    let head = git(&repository, &["rev-parse", "HEAD"]);
    let engine = RunningEngine::start_holding_git(&root.path().join("kratos"), "add");
    let inner = workspace_at(&engine, &repository.join("component"));
    let client = engine.client();
    let thread = create(&client, &inner, "chat", PlaceRequest::default());
    std::fs::write(repository.join("component").join("part.txt"), "two\n").unwrap();
    engine.hold(Gate::Git);
    let committing = {
        let (client, thread) = (client.clone(), thread.clone());
        std::thread::spawn(move || commit(&client, &thread, "Two"))
    };
    engine.wait_until_held(Gate::Git);
    git(
        &repository,
        &["merge", "--quiet", "--no-ff", "--no-commit", "feature"],
    );
    engine.release(Gate::Git);

    let refused = committing.join().unwrap();
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), head);
    assert!(repository.join(".git").join("MERGE_HEAD").exists());
}

#[test]
fn a_commit_keeps_what_the_user_staged_in_the_folder_while_it_staged_files() {
    // Catches a commit that sets the user's index to the committed files afterwards, which drops a
    // version the user staged meanwhile.
    let root = TempDir::new().unwrap();
    let repository = repository_with_component_only(&root);
    let engine = RunningEngine::start_holding_git(&root.path().join("kratos"), "add");
    let inner = workspace_at(&engine, &repository.join("component"));
    let client = engine.client();
    let thread = create(&client, &inner, "chat", PlaceRequest::default());
    let part = repository.join("component").join("part.txt");
    std::fs::write(&part, "two\n").unwrap();
    engine.hold(Gate::Git);
    let committing = {
        let (client, thread) = (client.clone(), thread.clone());
        std::thread::spawn(move || commit(&client, &thread, "Three"))
    };
    engine.wait_until_held(Gate::Git);
    git(&repository, &["add", "component/part.txt"]);
    std::fs::write(&part, "three\n").unwrap();
    engine.release(Gate::Git);

    let reply = committing.join().unwrap();
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(
        git(&repository, &["show", "HEAD:component/part.txt"]),
        "three"
    );
    assert_eq!(git(&repository, &["show", ":component/part.txt"]), "two");
    assert!(!repository.join(".git").join("index.lock").exists());
}

#[test]
fn a_commit_while_another_git_command_changes_the_index_is_refused_before_it_commits() {
    // Catches a commit that moves HEAD, then fails to update the user's index, which another git
    // command holds, and reports the commit it made as failed.
    let root = TempDir::new().unwrap();
    let (repository, engine) = repository_with_component(&root);
    let inner = workspace_at(&engine, &repository.join("component"));
    let client = engine.client();
    let thread = create(&client, &inner, "chat", PlaceRequest::default());
    std::fs::write(repository.join("component").join("part.txt"), "two\n").unwrap();
    let head = git(&repository, &["rev-parse", "HEAD"]);
    let index_lock = repository.join(".git").join("index.lock");
    std::fs::write(&index_lock, "").unwrap();

    let refused = commit(&client, &thread, "Two");

    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), head);
    std::fs::remove_file(&index_lock).unwrap();
    assert_eq!(git(&repository, &["show", ":component/part.txt"]), "one");
}

#[test]
fn a_turn_that_runs_and_ends_while_a_commit_stages_files_stops_it() {
    // Catches a commit that only asks whether a turn runs before and after git stages: a whole
    // turn can run between, so git took one file from before it and another from after it.
    let root = TempDir::new().unwrap();
    let (refused, repository) = commit_while_git_stages(&root, |engine, client, workspace| {
        run_a_turn_that_edits_both(engine, client, workspace);
    });

    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(git(&repository, &["rev-list", "--count", "HEAD"]), "1");
}

#[test]
fn a_turn_whose_thread_is_archived_while_a_commit_stages_files_still_stops_it() {
    // Catches a commit that looks for turns only in threads that are not archived, so a thread
    // archived right after its turn hides that turn.
    let root = TempDir::new().unwrap();
    let (refused, repository) = commit_while_git_stages(&root, |engine, client, workspace| {
        let busy = run_a_turn_that_edits_both(engine, client, workspace);
        let archive = ArchiveThread {
            files: LeftFiles::Keep,
        };
        let archived = client
            .post(&routes::thread_archive(&busy.id), &archive)
            .unwrap();
        assert_eq!(archived.status, 200, "{}", archived.body);
    });

    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(git(&repository, &["rev-list", "--count", "HEAD"]), "1");
}

#[test]
fn a_thread_made_while_a_commit_stages_files_does_not_stop_it() {
    // Catches a commit that counts any new thread in its folder as one that ran a turn.
    let root = TempDir::new().unwrap();
    let (reply, _) = commit_while_git_stages(&root, |_, client, workspace| {
        create(client, workspace, "new", PlaceRequest::default());
    });

    assert_eq!(reply.status, 201, "{}", reply.body);
}

/// Commits a change to `a.txt` and `b.txt` from an idle thread at the top of a new repository, and
/// runs `meanwhile` once git has read `a.txt` and before it reads `b.txt`. Returns the commit's
/// reply and the repository.
fn commit_while_git_stages(
    root: &TempDir,
    meanwhile: impl FnOnce(&RunningEngine, &Client, &Workspace),
) -> (ApiReply, std::path::PathBuf) {
    let repository = new_repository(root);
    let gate = root.path().join("filter-gate");
    std::fs::create_dir(&gate).unwrap();
    let gate_text = gate.to_str().unwrap().replace('\\', "/");
    // Git runs the clean filter on `a.txt` once it has read it and before it reads `b.txt`.
    git(
        &repository,
        &[
            "config",
            "filter.held.clean",
            &format!(
                "touch '{gate_text}/staging' && \
                 while [ ! -f '{gate_text}/go' ]; do sleep 0.05; done && cat"
            ),
        ],
    );
    std::fs::write(
        repository.join(".git").join("info").join("attributes"),
        "a.txt filter=held\n",
    )
    .unwrap();
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let idle = create(&client, &workspace, "idle", PlaceRequest::default());
    std::fs::write(repository.join("a.txt"), "old\n").unwrap();
    std::fs::write(repository.join("b.txt"), "old\n").unwrap();
    let committing = {
        let (client, idle) = (client.clone(), idle.clone());
        std::thread::spawn(move || commit(&client, &idle, "While git stages"))
    };
    let deadline = Instant::now() + WAIT;
    while !gate.join("staging").exists() {
        assert!(Instant::now() < deadline, "git never staged a.txt");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    meanwhile(&engine, &client, &workspace);
    std::fs::write(gate.join("go"), "").unwrap();
    (committing.join().unwrap(), repository)
}

/// Starts a thread in `workspace` whose turn rewrites `a.txt` and `b.txt`, and returns it once the
/// turn ended.
fn run_a_turn_that_edits_both(
    engine: &RunningEngine,
    client: &Client,
    workspace: &Workspace,
) -> Thread {
    let busy = create(client, workspace, "busy", PlaceRequest::default());
    let folder = std::path::PathBuf::from(workspace.path.as_str());
    post(client, &busy, "edit both");
    engine.start_turn();
    wait_for_turn(client, &busy);
    std::fs::write(folder.join("a.txt"), "new\n").unwrap();
    std::fs::write(folder.join("b.txt"), "new\n").unwrap();
    engine.fake_codex_request(codex_control::complete("thread-A", "turn-1", "completed"));
    wait_for_no_turn(client, &busy);
    busy
}

#[test]
fn a_folder_commit_is_refused_while_a_merge_or_a_revert_waits_to_be_committed() {
    // Catches a commit of only a folder that ends a merge or a revert with none of its changes
    // from outside the folder.
    refuses_a_folder_commit_while(start_merge, "MERGE_HEAD");
    refuses_a_folder_commit_while(start_revert, "REVERT_HEAD");
}

/// Checks that a commit of the `component` folder is refused, and leaves git's `pending` file,
/// once `start` leaves git in the middle of a commit.
fn refuses_a_folder_commit_while(start: fn(&Path), pending: &str) {
    let root = TempDir::new().unwrap();
    let repository = repository_with_component_only(&root);
    start(&repository);
    let head = git(&repository, &["rev-parse", "HEAD"]);
    std::fs::write(repository.join("component").join("part.txt"), "two\n").unwrap();
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let inner = workspace_at(&engine, &repository.join("component"));
    let client = engine.client();
    let thread = create(&client, &inner, "component", PlaceRequest::default());

    let refused = commit(&client, &thread, "Only the component");

    assert_eq!(refused.status, 409, "{pending}: {}", refused.body);
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), head);
    assert!(repository.join(".git").join(pending).exists(), "{pending}");
}

fn start_merge(repository: &Path) {
    git(repository, &["switch", "--quiet", "--create", "feature"]);
    std::fs::write(repository.join("feature.txt"), "feature\n").unwrap();
    git(repository, &["add", "feature.txt"]);
    git(repository, &["commit", "--quiet", "--message", "feature"]);
    git(repository, &["switch", "--quiet", "main"]);
    git(
        repository,
        &["merge", "--quiet", "--no-ff", "--no-commit", "feature"],
    );
}

/// Reverts a commit whose file changed since, which stops on the conflict, and stages the user's
/// resolution.
fn start_revert(repository: &Path) {
    let readme = repository.join("README.md");
    std::fs::write(&readme, "to revert\n").unwrap();
    git(
        repository,
        &["commit", "--quiet", "--all", "--message", "To revert"],
    );
    let target = git(repository, &["rev-parse", "HEAD"]);
    std::fs::write(&readme, "later\n").unwrap();
    git(
        repository,
        &["commit", "--quiet", "--all", "--message", "Later"],
    );
    let reverted = std::process::Command::new("git")
        .args(["revert", "--no-edit", &target])
        .current_dir(repository)
        .output()
        .unwrap();
    assert!(!reverted.status.success(), "the revert met no conflict");
    std::fs::write(&readme, "resolved\n").unwrap();
    git(repository, &["add", "README.md"]);
}

fn wait_for_turn(client: &Client, thread: &Thread) {
    wait_for_running(client, thread, true);
}

fn wait_for_no_turn(client: &Client, thread: &Thread) {
    wait_for_running(client, thread, false);
}

fn wait_for_running(client: &Client, thread: &Thread, running: bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        let details: ThreadDetails = client
            .get(&routes::thread(&thread.id))
            .unwrap()
            .parse()
            .unwrap();
        if details.running_turn.is_some() == running {
            return;
        }
        assert!(Instant::now() < deadline, "running stayed {}", !running);
        std::thread::yield_now();
    }
}

#[test]
fn a_pull_request_from_the_default_branch_is_refused_before_anything_is_pushed() {
    // Catches a pull request that pushes the thread's work straight onto the branch it was meant
    // to be reviewed against.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let remote = root.path().join("remote.git");
    git(
        root.path(),
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            remote.to_str().unwrap(),
        ],
    );
    git(
        &repository,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repository, &["push", "--quiet", "origin", "main"]);
    let published = git(&remote, &["rev-parse", "main"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "chat", PlaceRequest::default());
    std::fs::write(repository.join("README.md"), "reviewed later\n").unwrap();
    assert_eq!(commit(&client, &thread, "Change the readme").status, 201);

    let reply = client
        .post(
            &routes::thread_pull_request(&thread.id),
            &CreatePullRequest {
                title: RequiredText::new("Change the readme").unwrap(),
                body: String::new(),
            },
        )
        .unwrap();

    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(
        reply.body["error"]
            .as_str()
            .unwrap()
            .contains("default branch"),
        "{}",
        reply.body
    );
    assert_eq!(git(&remote, &["rev-parse", "main"]), published);
    assert!(engine.fake_gh_runs().is_empty());
}

#[test]
fn a_pull_request_to_an_origin_with_no_branches_is_refused_before_anything_is_pushed() {
    // Catches a push to an empty origin, which names no default branch to compare with: the push
    // publishes the thread's branch as origin's first branch, which GitHub makes the default.
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
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "chat", PlaceRequest::default());

    let reply = client
        .post(
            &routes::thread_pull_request(&thread.id),
            &CreatePullRequest {
                title: RequiredText::new("Proposed change").unwrap(),
                body: String::new(),
            },
        )
        .unwrap();

    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(git(&remote, &["for-each-ref"]), "");
    assert!(engine.fake_gh_runs().is_empty());
}

#[test]
fn a_pull_request_pushes_the_branch_and_gives_gh_each_value_as_its_own_argument() {
    // Catches a branch never pushed, or a title or body that a shell or `gh` itself would read as
    // code or a flag.
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
    let thread = create(&client, &workspace, "Add login", PlaceRequest::NewWorktree);
    let folder = worktree_of(&client, &thread).path;
    std::fs::write(Path::new(&folder).join("login.txt"), "login\n").unwrap();
    assert_eq!(commit(&client, &thread, "Add login").status, 201);
    let title = "--draft Add login";
    let body = "Uses \"quotes\", $HOME, and `ticks`; rm -rf /";

    let reply = client
        .post(
            &routes::thread_pull_request(&thread.id),
            &CreatePullRequest {
                title: RequiredText::new(title).unwrap(),
                body: body.to_owned(),
            },
        )
        .unwrap();

    assert_eq!(reply.status, 201, "{}", reply.body);
    let PullRequest { url } = reply.parse().unwrap();
    assert_eq!(url, "https://github.com/example/repo/pull/1");
    assert_eq!(
        git(&remote, &["rev-parse", "add-login"]),
        git(Path::new(&folder), &["rev-parse", "HEAD"])
    );
    assert_eq!(
        engine.fake_gh_runs(),
        [[
            "pr".to_owned(),
            "create".to_owned(),
            format!("--title={title}"),
            format!("--body={body}"),
            "--head=add-login".to_owned(),
        ]]
    );
}
