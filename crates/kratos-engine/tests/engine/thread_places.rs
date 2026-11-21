//! Where a thread works (R5), against real git in temporary repositories.

use std::path::{Path, PathBuf};
use std::process::Command;

use kratos_api_client::{Client, ClientError};
use kratos_protocol::{
    CreateThread, CreateWorkspace, Ownership, Place, PlaceRequest, ProjectWorktree, Provider,
    RequiredText, Thread, ThreadDetails, Workspace, Worktree, routes,
};
use serde_json::json;
use tempfile::TempDir;

use super::running_engine::{RunningEngine, post};

pub(super) fn git(folder: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Moves the worktree at `path` to `moved` and makes another checkout at `path` that holds a file
/// of the user's, as a user who overrides Kratos's lock on the worktree would.
pub(super) fn take_place(repository: &Path, path: &str, moved: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["worktree", "unlock", path])
        .output()
        .unwrap();
    git(
        repository,
        &["worktree", "move", path, moved.to_str().unwrap()],
    );
    git(
        repository,
        &["worktree", "add", "--quiet", "-b", "users-branch", path],
    );
    std::fs::write(Path::new(path).join("users.txt"), "the user's\n").unwrap();
}

/// A repository on branch `main` with one commit, at its resolved path.
pub(super) fn new_repository(root: &TempDir) -> PathBuf {
    let path = root.path().join("repo");
    std::fs::create_dir(&path).unwrap();
    git(&path, &["init", "--quiet", "--initial-branch=main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "hello\n").unwrap();
    git(&path, &["add", "README.md"]);
    git(&path, &["commit", "--quiet", "--message", "first"]);
    resolved(&path)
}

pub(super) fn resolved(path: &Path) -> PathBuf {
    let path = path.canonicalize().unwrap();
    let text = path.to_str().unwrap();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))
}

pub(super) fn workspace_at(engine: &RunningEngine, folder: &Path) -> Workspace {
    let request = CreateWorkspace {
        name: RequiredText::new("repo").unwrap(),
        path: RequiredText::new(folder.to_str().unwrap()).unwrap(),
    };
    engine.client().create_workspace(&request).unwrap()
}

pub(super) fn request(workspace: &Workspace, title: &str, place: PlaceRequest) -> CreateThread {
    CreateThread {
        workspace_id: workspace.id.clone(),
        title: RequiredText::new(title).unwrap(),
        provider: Provider::Codex,
        settings: None,
        place: Some(place),
    }
}

pub(super) fn create(
    client: &Client,
    workspace: &Workspace,
    title: &str,
    place: PlaceRequest,
) -> Thread {
    client
        .create_thread(&request(workspace, title, place))
        .unwrap()
}

pub(super) fn existing(path: &Path) -> PlaceRequest {
    PlaceRequest::ExistingWorktree {
        path: RequiredText::new(path.to_str().unwrap()).unwrap(),
    }
}

pub(super) fn place_of(client: &Client, thread: &Thread) -> Place {
    let details: ThreadDetails = client
        .get(&routes::thread(&thread.id))
        .unwrap()
        .parse()
        .unwrap();
    details.place
}

fn threads_in(client: &Client, workspace: &Workspace) -> Vec<Thread> {
    client
        .get(&routes::threads_in(&workspace.id))
        .unwrap()
        .parse()
        .unwrap()
}

pub(super) fn worktree_of(client: &Client, thread: &Thread) -> Worktree {
    match place_of(client, thread) {
        Place::Worktree(worktree) => worktree,
        Place::ProjectFolder => panic!("{} works in its project folder", thread.title),
    }
}

/// The folder Codex was told to work in when the thread's first message opened it.
fn codex_folder(engine: &RunningEngine, thread: &Thread) -> PathBuf {
    post(&engine.client(), thread, "hello");
    let started = engine.fake_codex_held("turn/start", 1);
    let roots = &started[0]["params"]["runtimeWorkspaceRoots"];
    assert_eq!(roots.as_array().map(Vec::len), Some(1), "{roots}");
    let opened = engine.fake_codex("test/threadRequests", json!({}));
    let cwd = opened[0]["params"]["cwd"].as_str().unwrap();
    assert_eq!(
        roots[0], cwd,
        "the sandbox's writable root is the thread's folder"
    );
    PathBuf::from(cwd)
}

pub(super) fn worktree_count(repository: &Path) -> usize {
    git(repository, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
}

pub(super) fn refusal(error: ClientError) -> (u16, String) {
    match error {
        ClientError::Unexpected { status, body } => {
            (status, body["error"].as_str().unwrap().to_owned())
        }
        other => panic!("not a refusal: {other}"),
    }
}

#[test]
fn without_a_place_the_agent_edits_the_project_folder_and_no_worktree_is_made() {
    // Catches a default that still makes a worktree (D26 changed on 2026-11-20), or a thread
    // whose agent runs somewhere other than the folder the user picked.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let thread = create(
        &engine.client(),
        &workspace,
        "chat",
        PlaceRequest::default(),
    );

    assert_eq!(place_of(&engine.client(), &thread), Place::ProjectFolder);
    assert_eq!(resolved(&codex_folder(&engine, &thread)), repository);
    assert_eq!(worktree_count(&repository), 1);
    assert!(!repository.join(".kratos-worktrees").exists());
}

#[test]
fn a_new_worktree_is_a_branch_named_after_the_title_that_the_project_never_shows_as_a_change() {
    // Catches a worktree made on the user's own branch, a branch that starts somewhere other than
    // the project's HEAD, an agent sent to the project folder instead, or a worktrees folder that
    // shows up in the user's `git status`.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let head = git(&repository, &["rev-parse", "HEAD"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let thread = create(
        &engine.client(),
        &workspace,
        "Fix the login bug!",
        PlaceRequest::NewWorktree,
    );

    let worktree = worktree_of(&engine.client(), &thread);
    let expected = repository
        .join(".kratos-worktrees")
        .join("fix-the-login-bug");
    assert_eq!(worktree.branch.as_deref(), Some("fix-the-login-bug"));
    assert_eq!(worktree.ownership, Ownership::CreatedByKratos);
    assert_eq!(PathBuf::from(&worktree.path), expected);
    assert_eq!(resolved(&codex_folder(&engine, &thread)), expected);
    assert_eq!(git(&expected, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&repository, &["branch", "--show-current"]), "main");
    assert_eq!(git(&repository, &["status", "--porcelain"]), "");
}

#[test]
fn threads_started_together_with_one_title_get_their_own_branches_and_leave_the_users_alone() {
    // Catches two threads given one branch, a name check and branch creation that race, or a
    // user's branch that Kratos reuses or moves when its name is taken.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    git(&repository, &["branch", "same-title"]);
    std::fs::write(repository.join("second.txt"), "two\n").unwrap();
    git(&repository, &["add", "second.txt"]);
    git(&repository, &["commit", "--quiet", "--message", "second"]);
    let users = git(&repository, &["rev-parse", "same-title"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);

    let threads: Vec<Thread> = std::thread::scope(|scope| {
        let starts: Vec<_> = (0..3)
            .map(|_| {
                let client = engine.client();
                let workspace = &workspace;
                scope.spawn(move || {
                    create(&client, workspace, "Same title", PlaceRequest::NewWorktree)
                })
            })
            .collect();
        starts
            .into_iter()
            .map(|start| start.join().unwrap())
            .collect()
    });

    let mut branches: Vec<String> = threads
        .iter()
        .map(|thread| worktree_of(&engine.client(), thread).branch.unwrap())
        .collect();
    branches.sort();
    assert_eq!(branches, ["same-title-2", "same-title-3", "same-title-4"]);
    assert_eq!(git(&repository, &["rev-parse", "same-title"]), users);
    assert_eq!(worktree_count(&repository), 4);
}

#[test]
fn threads_share_an_existing_worktree_that_stays_borrowed() {
    // Catches a second record for one worktree, an existing worktree claimed as Kratos's own, a
    // new worktree made instead, or an agent sent to the project folder.
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
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();

    let first = create(&client, &workspace, "one", existing(&theirs));
    let second = create(&client, &workspace, "two", existing(&theirs));

    let worktree = worktree_of(&client, &first);
    assert_eq!(worktree, worktree_of(&client, &second));
    assert_eq!(worktree.ownership, Ownership::Borrowed);
    assert_eq!(worktree.branch.as_deref(), Some("feature"));
    assert_eq!(resolved(&codex_folder(&engine, &first)), theirs);
    assert_eq!(worktree_count(&repository), 2);
}

#[test]
fn a_place_git_rules_out_is_refused_in_plain_words_and_records_nothing() {
    // Catches a raw git error shown to the user (L12), a thread recorded before its place was
    // checked, a worktree of another repository, or a subfolder taken for a worktree.
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
    std::fs::create_dir(theirs.join("src")).unwrap();
    let other_root = TempDir::new().unwrap();
    let other = new_repository(&other_root);
    let plain = root.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let empty = root.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    git(&empty, &["init", "--quiet"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let client = engine.client();
    let project = workspace_at(&engine, &repository);
    let not_git = workspace_at(&engine, &plain);
    let no_commits = workspace_at(&engine, &empty);
    let cases = [
        (
            &not_git,
            PlaceRequest::NewWorktree,
            "is not in a git repository",
        ),
        (&no_commits, PlaceRequest::NewWorktree, "has no commits yet"),
        (&project, existing(&other), "of a different repository"),
        (&project, existing(&repository), "is the project folder"),
        (
            &project,
            existing(&theirs.join("src")),
            "Name the worktree's top folder",
        ),
        (
            &project,
            existing(&root.path().join("missing")),
            "is not an existing folder",
        ),
    ];

    for (workspace, place, words) in cases {
        let refused = client
            .create_thread(&request(workspace, "chat", place.clone()))
            .unwrap_err();
        let (status, message) = refusal(refused);
        assert_eq!(status, 400, "{place:?}: {message}");
        assert!(message.contains(words), "{place:?}: {message}");
        assert!(!message.contains("fatal:"), "{place:?}: {message}");
    }
    for workspace in [&project, &not_git, &no_commits] {
        assert_eq!(threads_in(&client, workspace), []);
    }
    assert!(!plain.join(".kratos-worktrees").exists());
}

#[test]
fn a_repository_hook_never_runs_when_kratos_makes_a_worktree() {
    // Catches Kratos's git running a hook the repository ships, which would run outside every
    // sandbox. The same hook runs for the user's own `git worktree add`, so the hook works.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let marker = root.path().join("hook-ran");
    let hook = repository.join(".git").join("hooks").join("post-checkout");
    let marker_text = marker.to_str().unwrap().replace('\\', "/");
    std::fs::write(&hook, format!("#!/bin/sh\necho ran > '{marker_text}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let probe = root.path().join("probe");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "probe",
            probe.to_str().unwrap(),
        ],
    );
    assert!(marker.exists(), "the hook runs for the user's own git");
    std::fs::remove_file(&marker).unwrap();
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);

    let thread = create(
        &engine.client(),
        &workspace,
        "hooked",
        PlaceRequest::NewWorktree,
    );

    assert_eq!(
        worktree_of(&engine.client(), &thread).branch.as_deref(),
        Some("hooked")
    );
    assert!(!marker.exists(), "Kratos's git ran the repository's hook");
}

#[test]
fn an_agent_below_full_access_can_start_threads_only_where_it_works() {
    // Catches a worktree's agent that reaches the main checkout, or makes git write new branches,
    // by starting a thread somewhere else instead of asking the user.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let user = engine.client();
    let parent = create(&user, &workspace, "parent", PlaceRequest::NewWorktree);
    let agent = engine.client().acting_for(parent.id.clone());

    let child = agent
        .create_thread(&CreateThread {
            place: None,
            ..request(&workspace, "child", PlaceRequest::default())
        })
        .unwrap();
    assert_eq!(worktree_of(&user, &child), worktree_of(&user, &parent));

    for place in [PlaceRequest::ProjectFolder, PlaceRequest::NewWorktree] {
        let refused = agent
            .create_thread(&request(&workspace, "escape", place.clone()))
            .unwrap_err();
        assert_eq!(refusal(refused).0, 403, "{place:?}");
    }
    assert_eq!(threads_in(&user, &workspace).len(), 2);
    assert_eq!(worktree_count(&repository), 2);
}

#[test]
fn a_worktrees_folder_that_links_elsewhere_is_refused_and_nothing_is_written_there() {
    // Catches Kratos following a `.kratos-worktrees` link, which a cloned repository can commit,
    // and checking out the project into a folder outside it.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    link_folder(&outside, &repository.join(".kratos-worktrees"));
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);

    let refused = engine
        .client()
        .create_thread(&request(&workspace, "linked", PlaceRequest::NewWorktree))
        .unwrap_err();

    let (status, message) = refusal(refused);
    assert_eq!(status, 400, "{message}");
    assert!(message.contains("is a link"), "{message}");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    assert_eq!(worktree_count(&repository), 1);
}

#[test]
fn a_title_that_names_a_folder_of_branches_moves_on_to_the_next_name() {
    // Catches a branch name git refuses because `taken/login` exists, reported to the user as a
    // raw git failure instead of trying `taken-2`.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    git(&repository, &["branch", "taken/login"]);
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);

    let thread = create(
        &engine.client(),
        &workspace,
        "Taken",
        PlaceRequest::NewWorktree,
    );

    assert_eq!(
        worktree_of(&engine.client(), &thread).branch.as_deref(),
        Some("taken-2")
    );
}

#[cfg(unix)]
fn link_folder(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// A junction, which Windows lets any user make, unlike a symbolic link.
#[cfg(windows)]
fn link_folder(target: &Path, link: &Path) {
    let made = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
}

#[test]
fn the_project_lists_every_worktree_of_its_repository_and_a_thread_can_start_in_any() {
    // Catches a list that leaves out worktrees the user made, shows the project folder or a
    // worktree whose folder is gone, or claims a user's worktree as Kratos's own.
    let root = TempDir::new().unwrap();
    let repository = new_repository(&root);
    let theirs = root.path().join("feature");
    let gone = root.path().join("gone");
    for (name, folder) in [("feature/login", &theirs), ("gone", &gone)] {
        let folder = folder.to_str().unwrap();
        git(
            &repository,
            &["worktree", "add", "--quiet", "-b", name, folder],
        );
    }
    std::fs::remove_dir_all(&gone).unwrap();
    let engine = RunningEngine::start(&root.path().join("kratos"));
    let workspace = workspace_at(&engine, &repository);
    let client = engine.client();
    let made = create(&client, &workspace, "Made", PlaceRequest::NewWorktree);

    let mut listed: Vec<ProjectWorktree> = client
        .get(&routes::workspace_worktrees(&workspace.id))
        .unwrap()
        .parse()
        .unwrap();
    listed.sort_by(|a, b| a.path.cmp(&b.path));

    let theirs = resolved(&theirs);
    let mut expected = vec![
        ProjectWorktree {
            path: theirs.to_str().unwrap().to_owned(),
            branch: Some("feature/login".into()),
            ownership: Ownership::Borrowed,
        },
        ProjectWorktree {
            path: worktree_of(&client, &made).path,
            branch: Some("made".into()),
            ownership: Ownership::CreatedByKratos,
        },
    ];
    expected.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(listed, expected);
    let thread = create(&client, &workspace, "work there", existing(&theirs));
    assert_eq!(PathBuf::from(worktree_of(&client, &thread).path), theirs);
}
