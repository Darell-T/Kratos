//! A thread's finished work (R5, D27, D55): its diff, a commit, and a pull request, against real
//! git in temporary repositories and a stand-in `gh`.

use std::path::Path;
use std::time::Instant;

use fake_agents::codex_control;
use kratos_api_client::{ApiReply, Client};
use kratos_protocol::{
    CommitThread, Committed, CreatePullRequest, PlaceRequest, PullRequest, RequiredText, Thread,
    ThreadDetails, ThreadDiff, routes,
};
use tempfile::TempDir;

use super::running_engine::{Gate, RunningEngine, WAIT, post};
use super::thread_places::{create, existing, git, new_repository, workspace_at, worktree_of};

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
fn a_commit_at_the_top_refuses_unresolved_merge_conflicts_until_resolved() {
    // Catches a top-level commit staging raw conflict markers in a private index and wiping
    // unmerged stages from `.git/index` instead of refusing until the conflict is resolved.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    git(&repository, &["switch", "--quiet", "--create", "feature"]);
    std::fs::write(repository.join("README.md"), "feature version\n").unwrap();
    git(&repository, &["add", "README.md"]);
    git(&repository, &["commit", "--quiet", "--message", "feature"]);
    git(&repository, &["switch", "--quiet", "main"]);
    std::fs::write(repository.join("README.md"), "main version\n").unwrap();
    git(&repository, &["add", "README.md"]);
    git(&repository, &["commit", "--quiet", "--message", "main"]);
    let head = git(&repository, &["rev-parse", "HEAD"]);
    let merge = std::process::Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args(["merge", "--no-commit", "feature"])
        .output()
        .unwrap();
    assert!(!merge.status.success(), "expected a merge conflict");
    assert!(repository.join(".git").join("MERGE_HEAD").exists());
    assert!(!git(&repository, &["ls-files", "--unmerged"]).is_empty());

    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "merge", PlaceRequest::default());

    let refused = commit(&client, &thread, "Should refuse conflict");
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert!(repository.join(".git").join("MERGE_HEAD").exists());
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), head);
    assert!(!git(&repository, &["ls-files", "--unmerged"]).is_empty());

    std::fs::write(repository.join("README.md"), "resolved version\n").unwrap();
    git(&repository, &["add", "README.md"]);
    let resolved = commit(&client, &thread, "Resolved merge");
    assert_eq!(resolved.status, 201, "{}", resolved.body);
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

#[test]
fn concurrent_git_commit_is_refused_while_a_folder_commit_runs() {
    // Catches a commit that releases git's index lock while staging, letting another git commit
    // advance HEAD concurrently instead of being refused by git.
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
    let readme = repository.join("README.md");
    std::fs::write(&readme, "the user's\n").unwrap();
    let user_commit = std::process::Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args(["commit", "--quiet", "-am", "The user's"])
        .output()
        .unwrap();
    engine.release(Gate::Git);

    assert!(
        !user_commit.status.success(),
        "git commit succeeded while Kratos held the index lock"
    );
    let reply = committing.join().unwrap();
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(git(&repository, &["log", "-1", "--format=%s"]), "Two");
    assert_eq!(
        git(&repository, &["show", "HEAD:component/part.txt"]),
        "two"
    );
    assert_eq!(git(&repository, &["show", "HEAD:README.md"]), "hello");
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "the user's\n");
    assert!(!repository.join(".git").join("index.lock").exists());
}

#[test]
fn a_same_size_edit_in_the_same_second_as_the_index_shows_in_diff_and_commit() {
    // Catches copying `.git/index` with a fresh mtime, which turns off git's racy-git check and
    // hides a same-size edit made in the same second as the index write.
    let root = TempDir::new().unwrap();
    let repository = repository_with_component_only(&root);
    let component = repository.join("component");
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let inner = workspace_at(&engine, &component);
    let client = engine.client();
    let thread = create(&client, &inner, "racy", PlaceRequest::default());

    let subsec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_millis();
    std::thread::sleep(std::time::Duration::from_millis(
        u64::from(1000 - subsec) + 20,
    ));
    std::fs::write(component.join("part.txt"), "first value\n").unwrap();
    std::fs::write(repository.join("README.md"), "first outer\n").unwrap();
    git(&repository, &["add", "component/part.txt", "README.md"]);
    git(
        &repository,
        &["commit", "--quiet", "--message", "before racy"],
    );
    std::fs::write(component.join("part.txt"), "later value\n").unwrap();
    std::fs::write(repository.join("README.md"), "later outer\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1200));

    let shown = diff(&client, &thread);
    assert!(
        shown.patch.contains("+later value"),
        "diff missed same-size racy edit:\n{}",
        shown.patch
    );
    let reply = commit(&client, &thread, "Commit racy edit");
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(
        git(&repository, &["show", "HEAD:component/part.txt"]),
        "later value"
    );
    assert_eq!(
        git(&repository, &["show", ":component/part.txt"]),
        "later value"
    );
    assert_eq!(
        git(&repository, &["diff-files", "--name-only"]),
        "README.md",
        "the outside racy edit must remain visible to native git"
    );
}

#[test]
fn a_subfolder_commit_preserves_unrelated_staged_working_versions_and_index_flags() {
    // Catches a subfolder commit that drops unrelated staging, overwrites a different working
    // copy, or clears index flags outside the committed folder.
    let root = TempDir::new().unwrap();
    let repository = repository_with_component_only(&root);
    std::fs::write(repository.join("flagged.txt"), "flagged\n").unwrap();
    git(&repository, &["add", "flagged.txt"]);
    git(&repository, &["commit", "--quiet", "--message", "flagged"]);
    git(
        &repository,
        &["update-index", "--assume-unchanged", "flagged.txt"],
    );
    std::fs::write(repository.join("README.md"), "user staged version\n").unwrap();
    git(&repository, &["add", "README.md"]);
    std::fs::write(repository.join("README.md"), "user unstaged version\n").unwrap();
    std::fs::write(
        repository.join("component").join("part.txt"),
        "component edit\n",
    )
    .unwrap();
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let inner = workspace_at(&engine, &repository.join("component"));
    let client = engine.client();
    let thread = create(&client, &inner, "chat", PlaceRequest::default());

    let reply = commit(&client, &thread, "Update component");

    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(
        git(&repository, &["show", "HEAD:component/part.txt"]),
        "component edit"
    );
    assert_eq!(
        git(&repository, &["show", ":component/part.txt"]),
        "component edit"
    );
    assert_eq!(
        git(&repository, &["show", ":README.md"]),
        "user staged version"
    );
    assert_eq!(
        std::fs::read_to_string(repository.join("README.md")).unwrap(),
        "user unstaged version\n"
    );
    assert_eq!(
        git(&repository, &["ls-files", "-v", "flagged.txt"]),
        "h flagged.txt"
    );
}

#[test]
fn concurrent_git_add_is_refused_while_a_folder_commit_runs() {
    // Catches a commit that does not hold git's index lock while staging, letting another git add
    // mutate the index concurrently instead of being refused by git.
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
        std::thread::spawn(move || commit(&client, &thread, "Two"))
    };
    engine.wait_until_held(Gate::Git);
    let readme = repository.join("README.md");
    std::fs::write(&readme, "user draft\n").unwrap();
    let user_add = std::process::Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args(["add", "README.md"])
        .output()
        .unwrap();
    engine.release(Gate::Git);

    assert!(
        !user_add.status.success(),
        "git add succeeded while Kratos held the index lock"
    );
    let reply = committing.join().unwrap();
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(
        git(&repository, &["show", "HEAD:component/part.txt"]),
        "two"
    );
    assert_eq!(git(&repository, &["show", ":README.md"]), "hello");
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "user draft\n");
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
    let deadline = Instant::now() + WAIT;
    loop {
        let details: ThreadDetails = client
            .get(&routes::thread(&thread.id))
            .unwrap()
            .parse()
            .unwrap();
        if details.running_turn.is_some() {
            return;
        }
        assert!(Instant::now() < deadline, "running stayed false");
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

#[test]
fn diff_and_commit_preserve_sparse_checkout_skip_worktree_files() {
    // Catches building the private index from plain `read-tree HEAD` without re-applying
    // `skip-worktree`, which causes `git add --all` to treat sparse-excluded files as deletions.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    std::fs::create_dir_all(repository.join("included")).unwrap();
    std::fs::create_dir_all(repository.join("excluded")).unwrap();
    std::fs::write(repository.join("included").join("a.txt"), "one\n").unwrap();
    std::fs::write(repository.join("excluded").join("b.txt"), "keep sparse\n").unwrap();
    git(&repository, &["add", "included", "excluded"]);
    git(&repository, &["commit", "--quiet", "--message", "two dirs"]);
    git(
        &repository,
        &["update-index", "--skip-worktree", "excluded/b.txt"],
    );
    std::fs::remove_file(repository.join("excluded").join("b.txt")).unwrap();
    std::fs::write(repository.join("included").join("a.txt"), "two\n").unwrap();

    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "sparse", PlaceRequest::default());

    let patch = diff(&client, &thread);
    assert!(patch.patch.contains("included/a.txt"), "{}", patch.patch);
    assert!(
        !patch.patch.contains("excluded/b.txt"),
        "sparse-excluded file must not appear deleted in diff: {}",
        patch.patch
    );

    let reply = commit(&client, &thread, "edit included only");
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(git(&repository, &["show", "HEAD:included/a.txt"]), "two");
    assert_eq!(
        git(&repository, &["show", "HEAD:excluded/b.txt"]),
        "keep sparse"
    );
}

#[test]
fn diff_and_commit_include_force_added_ignored_files_with_working_content() {
    // Catches `git add --all` in the private index dropping files the user force-added past
    // `.gitignore` (and `reset` then unstaging them), or glob-expanding literal bracket names.
    let root = TempDir::new().unwrap();
    let repository = repository_with_component_only(&root);
    std::fs::write(repository.join(".gitignore"), "*.local\nignored_dir/\n").unwrap();
    git(&repository, &["add", ".gitignore"]);
    git(
        &repository,
        &["commit", "--quiet", "--message", "add gitignore"],
    );

    std::fs::create_dir_all(repository.join("ignored_dir")).unwrap();
    std::fs::write(repository.join("forced.local"), "staged version\n").unwrap();
    std::fs::write(
        repository.join("ignored_dir").join("secret.txt"),
        "secret\n",
    )
    .unwrap();
    std::fs::write(repository.join("a[1].local"), "bracket\n").unwrap();
    std::fs::write(repository.join("a1.local"), "leave ignored\n").unwrap();
    std::fs::write(repository.join("plain.local"), "do not stage\n").unwrap();
    std::fs::write(repository.join("gone.local"), "deleted later\n").unwrap();
    git(
        &repository,
        &[
            "--literal-pathspecs",
            "add",
            "--force",
            "forced.local",
            "ignored_dir/secret.txt",
            "a[1].local",
            "gone.local",
        ],
    );
    std::fs::remove_file(repository.join("gone.local")).unwrap();
    std::fs::write(repository.join("forced.local"), "working edit\n").unwrap();

    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let thread = create(&client, &workspace, "forced", PlaceRequest::default());

    let patch = diff(&client, &thread);
    assert!(patch.patch.contains("forced.local"), "{}", patch.patch);
    assert!(patch.patch.contains("working edit"), "{}", patch.patch);
    assert!(
        patch.patch.contains("ignored_dir/secret.txt"),
        "{}",
        patch.patch
    );
    assert!(patch.patch.contains("a[1].local"), "{}", patch.patch);
    assert!(!patch.patch.contains("plain.local"), "{}", patch.patch);
    assert!(!patch.patch.contains("a1.local"), "{}", patch.patch);
    assert!(!patch.patch.contains("gone.local"), "{}", patch.patch);

    let reply = commit(&client, &thread, "Include force-added ignored");
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(
        git(&repository, &["show", "HEAD:forced.local"]),
        "working edit"
    );
    assert_eq!(git(&repository, &["show", ":forced.local"]), "working edit");
    assert_eq!(
        git(&repository, &["show", "HEAD:ignored_dir/secret.txt"]),
        "secret"
    );
    assert_eq!(
        git(&repository, &["show", ":ignored_dir/secret.txt"]),
        "secret"
    );
    assert_eq!(git(&repository, &["show", "HEAD:a[1].local"]), "bracket");
    assert_eq!(git(&repository, &["show", ":a[1].local"]), "bracket");
    let tracked = git(&repository, &["ls-files"]);
    for absent in ["plain.local", "a1.local", "gone.local"] {
        assert!(!tracked.lines().any(|line| line == absent), "{tracked}");
    }

    // Subfolder commit includes its own force-added ignored file and keeps outside staging intact.
    let component = repository.join("component");
    std::fs::write(component.join("sub.local"), "sub v1\n").unwrap();
    std::fs::write(repository.join("outside.local"), "outside\n").unwrap();
    git(
        &repository,
        &["add", "--force", "component/sub.local", "outside.local"],
    );
    std::fs::write(component.join("sub.local"), "sub v2\n").unwrap();
    let inner = workspace_at(&engine, &component);
    let sub_thread = create(&client, &inner, "sub-forced", PlaceRequest::default());

    let sub_patch = diff(&client, &sub_thread);
    assert!(
        sub_patch.patch.contains("component/sub.local"),
        "{}",
        sub_patch.patch
    );
    assert!(
        !sub_patch.patch.contains("outside.local"),
        "{}",
        sub_patch.patch
    );

    let sub_reply = commit(&client, &sub_thread, "Subfolder force-added");
    assert_eq!(sub_reply.status, 201, "{}", sub_reply.body);
    assert_eq!(
        git(&repository, &["show", "HEAD:component/sub.local"]),
        "sub v2"
    );
    assert_eq!(
        git(&repository, &["show", ":component/sub.local"]),
        "sub v2"
    );
    assert_eq!(git(&repository, &["show", ":outside.local"]), "outside");
    let tree = git(&repository, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.lines().any(|line| line == "outside.local"), "{tree}");

    // A tracked gitignored file marked skip-worktree and still present on disk must not break diff
    // or commit (git add --force rejects pathspecs matching only skip-worktree entries).
    std::fs::write(repository.join("sparse.local"), "committed sparse\n").unwrap();
    git(&repository, &["add", "--force", "sparse.local"]);
    git(
        &repository,
        &["commit", "--quiet", "--message", "track sparse ignored"],
    );
    git(
        &repository,
        &["update-index", "--skip-worktree", "sparse.local"],
    );
    std::fs::write(repository.join("README.md"), "ordinary edit\n").unwrap();

    let sparse_patch = diff(&client, &thread);
    assert!(
        sparse_patch.patch.contains("README.md"),
        "{}",
        sparse_patch.patch
    );
    let sparse_reply = commit(&client, &thread, "Commit alongside skip-worktree ignored");
    assert_eq!(sparse_reply.status, 201, "{}", sparse_reply.body);
    assert_eq!(
        git(&repository, &["show", "HEAD:README.md"]),
        "ordinary edit"
    );
}
