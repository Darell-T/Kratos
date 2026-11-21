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

use super::running_engine::{RunningEngine, WAIT, post};
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

/// A repository with a committed `component` folder, an engine, and workspaces at the repository
/// and at the folder.
fn repository_with_component(root: &TempDir) -> (std::path::PathBuf, RunningEngine) {
    let repository = new_repository(root);
    let component = repository.join("component");
    std::fs::create_dir(&component).unwrap();
    std::fs::write(
        component.join("part.txt"),
        "one
",
    )
    .unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "--quiet", "--message", "part"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    (repository, engine)
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
        assert!(Instant::now() < deadline, "the turn never started");
        std::thread::yield_now();
    }
}

#[test]
fn a_pull_request_pushes_the_branch_and_gives_gh_each_value_as_its_own_argument() {
    // Catches a branch never pushed, or a title or body that a shell or `gh` itself would read as
    // code or a flag.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let remote = root.path().join("remote.git");
    git(
        root.path(),
        &["init", "--quiet", "--bare", remote.to_str().unwrap()],
    );
    git(
        &repository,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
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
