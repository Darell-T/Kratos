use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use kratos_protocol::{
    CreateThread, CreateWorkspace, Seq, Thread, ThreadEventBody, ThreadId, WorkspaceId,
};
use kratos_store::{
    BackupId, DirectoryError, OpenError, RestoreError, SCHEMA_VERSION, Store, StoreError, restore,
};
use rusqlite::Connection;
use rusqlite::types::Value;
use serde_json::json;
use tempfile::TempDir;

const STEPS: [&str; 4] = [
    include_str!("../migrations/0001_workspaces_and_threads.sql"),
    include_str!("../migrations/0002_codex_threads.sql"),
    include_str!("../migrations/0003_claude_threads.sql"),
    include_str!("../migrations/0004_plan_mode.sql"),
];

fn home() -> TempDir {
    tempfile::tempdir().unwrap()
}

fn live_database(home: &Path) -> PathBuf {
    home.join("kratos.db")
}

fn backup_files(home: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(home.join("backups")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn workspace_request(name: &str, path: &str) -> CreateWorkspace {
    serde_json::from_value(json!({ "name": name, "path": path })).unwrap()
}

fn thread_request(workspace_id: &WorkspaceId, title: &str, provider: &str) -> CreateThread {
    let body =
        json!({ "workspaceId": workspace_id.as_str(), "title": title, "provider": provider });
    serde_json::from_value(body).unwrap()
}

fn message(text: &str) -> ThreadEventBody {
    ThreadEventBody::UserMessage {
        text: text.to_owned(),
    }
}

fn seed_thread(store: &Store, title: &str) -> Thread {
    let workspace = store
        .create_workspace(&workspace_request("repo", "/tmp/repo"))
        .unwrap();
    store
        .create_thread(&thread_request(&workspace.id, title, "codex"))
        .unwrap()
}

fn seqs(store: &Store, thread_id: &ThreadId, after: u64, max_bytes: usize) -> Vec<u64> {
    let events = store
        .events_after(thread_id, Seq::new(after), max_bytes)
        .unwrap();
    events.iter().map(|event| event.seq.get()).collect()
}

/// Writes `home/kratos.db` at schema `version`, with one workspace, thread, and event, plus a Codex
/// thread at version 2 and up and a Claude thread at version 3 and up.
fn database_at_version(home: &Path, version: u32) {
    fs::create_dir_all(home).unwrap();
    let connection = Connection::open(live_database(home)).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    for (index, step) in STEPS.iter().take(version as usize).enumerate() {
        connection.execute_batch(step).unwrap();
        connection
            .pragma_update(None, "user_version", index as u32 + 1)
            .unwrap();
    }
    connection
        .execute_batch(
            "INSERT INTO workspaces (id, name, path, created_at)
               VALUES ('workspace-1', 'repo', '/work/repo', '2030-01-01T00:00:00.000Z');
             INSERT INTO threads (id, workspace_id, title, provider, created_at, updated_at)
               VALUES ('thread-1', 'workspace-1', 'chat', 'codex', '2030-01-01T00:00:01.000Z', '2030-01-01T00:00:02.000Z');
             INSERT INTO thread_events (thread_id, seq, at, event)
               VALUES ('thread-1', 1, '2030-01-01T00:00:02.000Z', '{\"kind\":\"user.message\",\"text\":\"old\"}');",
        )
        .unwrap();
    if version >= 2 {
        connection
            .execute_batch(
                "INSERT INTO codex_threads (id, folder, model, permissions, paused_error)
                 VALUES ('thread-1', '/work/repo', 'gpt-test', 'askMe', NULL);",
            )
            .unwrap();
    }
    if version >= 3 {
        connection
            .execute_batch(
                "INSERT INTO claude_threads (id, folder, model, permissions, paused_error, running_turn)
                 VALUES ('thread-1', '/work/repo', NULL, 'askMe', NULL, NULL);",
            )
            .unwrap();
    }
}

fn user_version(database: &Path) -> u32 {
    let connection = Connection::open(database).unwrap();
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn contents(database: &Path) -> Vec<String> {
    let connection = Connection::open(database).unwrap();
    let mut lines = vec![format!("user_version = {}", user_version(database))];
    let mut tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    let names: Vec<String> = tables
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|name| name.unwrap())
        .collect();
    for table in names {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table}"))
            .unwrap();
        let width = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                (0..width)
                    .map(|index| row.get::<_, Value>(index))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap();
        lines.extend(rows.map(|values| format!("{table}: {:?}", values.unwrap())));
    }
    lines
}

fn rows_of(contents: &[String], table: &str) -> Vec<String> {
    let prefix = format!("{table}: ");
    contents
        .iter()
        .filter(|line| line.starts_with(&prefix))
        .cloned()
        .collect()
}

fn schema(database: &Path) -> Vec<(String, String, String, Option<String>)> {
    let connection = Connection::open(database).unwrap();
    let mut statement = connection
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap();
    let rows = statement.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    });
    rows.unwrap().map(|row| row.unwrap()).collect()
}

fn open_error(home: &Path) -> OpenError {
    Store::open(home).err().expect("the open should fail")
}

fn typescript_fixture() -> (PathBuf, PathBuf) {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    (
        fixtures.join("typescript-v4.db"),
        fixtures.join("typescript-v4.json"),
    )
}

#[test]
fn numbers_events_per_thread_from_one() {
    // Catches a seq shared across threads or starting at 0, and a last_seq that disagrees with the
    // log.
    let home = home();
    let mut store = Store::open(home.path()).unwrap();
    let first = seed_thread(&store, "first");
    let second = seed_thread(&store, "second");
    let quiet = seed_thread(&store, "quiet");

    let appended = [
        store.append_event(&first.id, &message("a")).unwrap().seq,
        store.append_event(&first.id, &message("b")).unwrap().seq,
        store.append_event(&second.id, &message("c")).unwrap().seq,
    ];

    assert_eq!(appended, [Seq::new(1), Seq::new(2), Seq::new(1)]);
    let all = store
        .events_after(&first.id, Seq::BEFORE_FIRST, usize::MAX)
        .unwrap();
    assert_eq!(
        all.iter()
            .map(|event| event.event.clone())
            .collect::<Vec<_>>(),
        [message("a"), message("b")]
    );
    assert_eq!(seqs(&store, &first.id, 1, usize::MAX), [2]);
    assert!(seqs(&store, &first.id, 2, usize::MAX).is_empty());
    assert_eq!(store.last_seq(&first.id).unwrap(), Seq::new(2));
    assert_eq!(store.last_seq(&second.id).unwrap(), Seq::new(1));
    assert_eq!(store.last_seq(&quiet.id).unwrap(), Seq::BEFORE_FIRST);
}

#[test]
fn keeps_events_across_reopening() {
    // Catches commits lost on close, numbering that restarts, and a backup taken of a current
    // database, which would add one to the disk on every start.
    let home = home();
    let thread = {
        let mut store = Store::open(home.path()).unwrap();
        assert!(
            store.backup_taken().is_none(),
            "a new database has nothing to back up"
        );
        let thread = seed_thread(&store, "persisted");
        store
            .append_event(&thread.id, &message("before restart"))
            .unwrap();
        thread
    };

    let mut reopened = Store::open(home.path()).unwrap();
    assert!(
        reopened.backup_taken().is_none(),
        "a current database is not backed up"
    );
    let listed = reopened.list_threads(&thread.workspace_id).unwrap();
    assert_eq!(
        (listed[0].id.clone(), listed[0].title.as_str()),
        (thread.id.clone(), "persisted")
    );
    assert_eq!(seqs(&reopened, &thread.id, 0, usize::MAX), [1]);
    assert_eq!(
        reopened
            .append_event(&thread.id, &message("after"))
            .unwrap()
            .seq,
        Seq::new(2)
    );
    drop(reopened);
    drop(Store::open(home.path()).unwrap());
    assert!(backup_files(home.path()).is_empty());
}

#[test]
fn appending_sets_the_thread_updated_at_to_the_event_time() {
    // Catches an append that forgets the thread, which would sort the thread list wrongly.
    let home = home();
    let mut store = Store::open(home.path()).unwrap();
    let thread = seed_thread(&store, "clock");

    thread::sleep(Duration::from_millis(10));
    let event = store.append_event(&thread.id, &message("tick")).unwrap();

    let listed = &store.list_threads(&thread.workspace_id).unwrap()[0];
    assert_eq!(listed.created_at, thread.created_at);
    assert_eq!(listed.updated_at, event.at);
    assert_ne!(listed.updated_at, listed.created_at);
}

#[test]
fn refuses_events_for_an_unknown_thread_without_writing() {
    // Catches orphan event rows, and a missing thread reported as success.
    let home = home();
    let mut store = Store::open(home.path()).unwrap();
    let missing = ThreadId::new("missing".into());

    let error = store.append_event(&missing, &message("x")).unwrap_err();
    assert!(matches!(&error, StoreError::UnknownThread(id) if *id == missing));
    assert_eq!(error.to_string(), "Unknown thread missing");
    assert!(matches!(
        store.last_seq(&missing),
        Err(StoreError::UnknownThread(_))
    ));
    assert!(seqs(&store, &missing, 0, usize::MAX).is_empty());
    assert!(rows_of(&contents(&live_database(home.path())), "thread_events").is_empty());
}

#[test]
fn lists_only_a_workspace_own_threads_oldest_first() {
    // Catches a missing WHERE (another workspace's threads leak in) or a descending order.
    let home = home();
    let store = Store::open(home.path()).unwrap();
    let mine = store
        .create_workspace(&workspace_request("mine", "/tmp/mine"))
        .unwrap();
    let other = store
        .create_workspace(&workspace_request("other", "/tmp/other"))
        .unwrap();
    let a = store
        .create_thread(&thread_request(&mine.id, "a", "claude"))
        .unwrap();
    let b = store
        .create_thread(&thread_request(&mine.id, "b", "codex"))
        .unwrap();
    let elsewhere = store
        .create_thread(&thread_request(&other.id, "elsewhere", "codex"))
        .unwrap();
    let c = store
        .create_thread(&thread_request(&mine.id, "c", "claude"))
        .unwrap();

    assert_eq!(store.list_threads(&mine.id).unwrap(), [a, b, c]);
    assert_eq!(store.list_threads(&other.id).unwrap(), [elsewhere]);
    assert_eq!(store.list_workspaces().unwrap(), [mine, other]);
}

#[test]
fn reports_an_unknown_workspace_when_listing_or_creating_threads() {
    // Catches an empty list where the API must answer 404, and a thread created in a missing
    // workspace.
    let home = home();
    let store = Store::open(home.path()).unwrap();
    let unknown = WorkspaceId::new("nope".into());

    let listing = store.list_threads(&unknown).unwrap_err();
    assert!(matches!(&listing, StoreError::UnknownWorkspace(id) if *id == unknown));
    assert_eq!(listing.to_string(), "Unknown workspace nope");
    let creating = store
        .create_thread(&thread_request(&unknown, "t", "codex"))
        .unwrap_err();
    assert!(matches!(&creating, StoreError::UnknownWorkspace(id) if *id == unknown));
    assert!(rows_of(&contents(&live_database(home.path())), "threads").is_empty());
}

#[test]
fn pages_events_by_size_and_always_moves_forward() {
    // Catches a follower stuck forever behind one large event, and a page that overshoots its
    // budget.
    let home = home();
    let mut store = Store::open(home.path()).unwrap();
    let thread = seed_thread(&store, "paged");
    // Each of these events is stored as `{"kind":"user.message","text":"xxxx…"}`: 133 bytes.
    let text = "x".repeat(100);
    for _ in 0..3 {
        store.append_event(&thread.id, &message(&text)).unwrap();
    }

    assert_eq!(
        seqs(&store, &thread.id, 0, 266),
        [1, 2],
        "two events fit exactly"
    );
    assert_eq!(
        seqs(&store, &thread.id, 0, 265),
        [1],
        "one byte short of two"
    );
    assert_eq!(
        seqs(&store, &thread.id, 0, 1),
        [1],
        "an event over the budget still comes, alone"
    );
    assert_eq!(seqs(&store, &thread.id, 1, 1), [2]);
    assert_eq!(seqs(&store, &thread.id, 2, 1), [3]);
    assert!(seqs(&store, &thread.id, 3, 1).is_empty());
    assert_eq!(seqs(&store, &thread.id, 0, usize::MAX), [1, 2, 3]);
}

#[test]
fn opens_a_database_the_typescript_engine_wrote() {
    // Catches any drift in the wire JSON or the row mapping, which the engine's own tests cannot
    // see because they compare the Rust types with themselves.
    let home = home();
    let (database, served) = typescript_fixture();
    fs::copy(database, live_database(home.path())).unwrap();
    let served: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(served).unwrap()).unwrap();

    let mut store = Store::open(home.path()).unwrap();

    assert!(
        store.backup_taken().is_none(),
        "the fixture is already at the current version"
    );
    let workspaces = store.list_workspaces().unwrap();
    assert_eq!(
        serde_json::to_value(&workspaces).unwrap(),
        served["workspaces"]
    );
    let (mut threads_seen, mut events_seen) = (0, 0);
    for workspace in &workspaces {
        let threads = store.list_threads(&workspace.id).unwrap();
        assert_eq!(
            serde_json::to_value(&threads).unwrap(),
            served["threads"][workspace.id.as_str()]
        );
        for thread in &threads {
            let events = store
                .events_after(&thread.id, Seq::BEFORE_FIRST, usize::MAX)
                .unwrap();
            assert_eq!(
                serde_json::to_value(&events).unwrap(),
                served["events"][thread.id.as_str()]
            );
            let next = store
                .append_event(&thread.id, &message("continues"))
                .unwrap();
            assert_eq!(
                next.seq.get(),
                events.len() as u64 + 1,
                "numbering continues after the TypeScript events"
            );
            threads_seen += 1;
            events_seen += events.len();
        }
    }
    assert_eq!((workspaces.len(), threads_seen, events_seen), (2, 3, 5));
}

#[test]
fn creates_the_schema_text_the_typescript_engine_created() {
    // Catches a migration file that drifted from the TypeScript statements, or a checkout that
    // rewrote its line endings. SQLite stores each CREATE statement's text, so either would make
    // the two engines' files differ even though both open.
    let rust_home = home();
    drop(Store::open(rust_home.path()).unwrap());
    let typescript_home = home();
    fs::copy(
        typescript_fixture().0,
        live_database(typescript_home.path()),
    )
    .unwrap();

    let rust = live_database(rust_home.path());
    let typescript = live_database(typescript_home.path());
    assert_eq!(schema(&rust), schema(&typescript));
    assert_eq!(user_version(&rust), user_version(&typescript));
}

#[test]
fn backs_up_each_older_version_before_migrating_it() {
    // Catches a missing, partial, or post-migration backup: the backup must be the database as the
    // older build left it, and the migrated database must keep every row.
    for version in 1..SCHEMA_VERSION {
        let home = home();
        database_at_version(home.path(), version);
        let live = live_database(home.path());
        let before = contents(&live);

        let store = Store::open(home.path()).unwrap();
        let backup = store
            .backup_taken()
            .expect("an older database is backed up")
            .clone();
        drop(store);

        assert_eq!(backup.source_version, version);
        let name = format!("kratos-v{version}-{}.db", backup.id);
        assert_eq!(
            backup_files(home.path()),
            [name],
            "only the finished backup is left in the folder"
        );
        assert_eq!(
            contents(&backup.path),
            before,
            "the backup is the database before the migration"
        );
        let after = contents(&live);
        assert_eq!(user_version(&live), SCHEMA_VERSION);
        for table in ["workspaces", "threads", "thread_events"] {
            assert_eq!(
                rows_of(&after, table),
                rows_of(&before, table),
                "{table} after migrating from {version}"
            );
        }
    }
}

#[test]
fn restores_a_backup_and_keeps_the_replaced_database_as_a_backup() {
    // Catches a restore that overwrites a backup, loses the database it replaced, or leaves a log
    // behind.
    for version in 1..SCHEMA_VERSION {
        let home = home();
        database_at_version(home.path(), version);
        let live = live_database(home.path());
        let at_version = contents(&live);
        let mut store = Store::open(home.path()).unwrap();
        let backup = store.backup_taken().unwrap().clone();
        store
            .append_event(
                &ThreadId::new("thread-1".into()),
                &message("after the migration"),
            )
            .unwrap();
        drop(store);
        let migrated = contents(&live);
        assert_ne!(migrated, at_version);

        let restored = restore(home.path(), &backup.id).unwrap();

        assert_eq!(restored.restored, backup);
        assert!(
            !home.path().join("kratos.db-wal").exists()
                && !home.path().join("kratos.db-shm").exists()
        );
        assert_eq!(
            contents(&live),
            at_version,
            "the live database is the backup again"
        );
        let replaced = restored.replaced.expect("the migrated database was saved");
        assert_eq!(replaced.source_version, SCHEMA_VERSION);
        assert_eq!(contents(&replaced.path), migrated);

        let store = Store::open(home.path()).unwrap();
        let again = store
            .backup_taken()
            .expect("the restored database is older again");
        assert_eq!(again.source_version, version);
        assert_eq!(
            backup_files(home.path()).len(),
            3,
            "the backup, the replaced database, and the new backup"
        );
    }
}

#[test]
fn restores_into_a_home_that_lost_its_database() {
    // Catches a restore that needs a live database to replace, when the case it exists for is a
    // database that was moved aside after it broke.
    let home = home();
    database_at_version(home.path(), 2);
    let at_version = contents(&live_database(home.path()));
    let backup = Store::open(home.path())
        .unwrap()
        .backup_taken()
        .unwrap()
        .clone();
    fs::remove_file(live_database(home.path())).unwrap();

    let restored = restore(home.path(), &backup.id).unwrap();

    assert!(restored.replaced.is_none());
    assert_eq!(contents(&live_database(home.path())), at_version);
}

/// The write-ahead log is left behind, as a crash would leave it.
fn crash_while_writing(home: &Path) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["crashed_writer", "--exact", "--ignored", "--nocapture"])
        .env("KRATOS_CRASH_HOME", home)
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(3),
        "the writer should stop at its simulated crash"
    );
}

#[test]
#[ignore = "runs only as the child process of crash_while_writing"]
fn crashed_writer() {
    let home = PathBuf::from(std::env::var_os("KRATOS_CRASH_HOME").unwrap());
    let connection = Connection::open(live_database(&home)).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    connection
        .execute(
            "INSERT INTO workspaces (id, name, path, created_at) VALUES ('crashed', 'crashed', '/x', '2030-01-01T00:00:09.000Z')",
            [],
        )
        .unwrap();
    std::process::exit(3);
}

#[test]
fn restore_discards_the_log_a_crash_left_behind_and_saves_what_it_held() {
    // Catches a stale write-ahead log replayed into the restored file, which would mix two
    // databases.
    let home = home();
    database_at_version(home.path(), 2);
    let backup = Store::open(home.path())
        .unwrap()
        .backup_taken()
        .unwrap()
        .clone();
    crash_while_writing(home.path());
    assert!(
        home.path().join("kratos.db-wal").exists(),
        "the crash leaves a log, or this test proves nothing"
    );

    let restored = restore(home.path(), &backup.id).unwrap();

    assert!(!home.path().join("kratos.db-wal").exists());
    assert_eq!(
        contents(&live_database(home.path())),
        contents(&backup.path)
    );
    let saved = contents(&restored.replaced.unwrap().path);
    assert!(
        rows_of(&saved, "workspaces")
            .iter()
            .any(|row| row.contains("crashed")),
        "the replaced database keeps the log's commit"
    );
}

#[test]
fn refuses_a_damaged_backup_and_changes_nothing() {
    // Catches a restore that installs a cut-short or mislabeled backup over a working database.
    let home = home();
    database_at_version(home.path(), 2);
    let backup = Store::open(home.path())
        .unwrap()
        .backup_taken()
        .unwrap()
        .clone();
    let live = live_database(home.path());
    let live_before = fs::read(&live).unwrap();

    let file = File::options().write(true).open(&backup.path).unwrap();
    file.set_len(fs::metadata(&backup.path).unwrap().len() / 2)
        .unwrap();
    drop(file);
    let cut_short = restore(home.path(), &backup.id).unwrap_err();
    assert!(
        matches!(cut_short, RestoreError::DamagedBackup { .. }),
        "{cut_short}"
    );

    let renamed = backup
        .path
        .with_file_name(format!("kratos-v1-{}.db", backup.id));
    fs::write(&renamed, fs::read(&backup.path).unwrap()).unwrap();
    fs::remove_file(&backup.path).unwrap();
    let mislabeled = restore(home.path(), &backup.id).unwrap_err();
    assert!(
        matches!(mislabeled, RestoreError::DamagedBackup { .. }),
        "{mislabeled}"
    );

    assert_eq!(fs::read(&live).unwrap(), live_before);
    assert_eq!(
        backup_files(home.path()).len(),
        1,
        "no database was saved, because nothing was replaced"
    );
}

#[test]
fn refuses_a_database_newer_than_this_build_and_leaves_it_untouched() {
    // Catches a build that downgrades or damages a database a newer build wrote.
    let home = home();
    database_at_version(home.path(), SCHEMA_VERSION);
    Connection::open(live_database(home.path()))
        .unwrap()
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    let before = fs::read(live_database(home.path())).unwrap();

    let error = open_error(home.path());

    assert!(
        matches!(
            error,
            OpenError::NewerSchema {
                found: 5,
                supported: 4
            }
        ),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "Database schema version 5 is newer than this build supports (4)"
    );
    assert_eq!(fs::read(live_database(home.path())).unwrap(), before);
    assert!(backup_files(home.path()).is_empty());
}

#[test]
fn a_failed_migration_keeps_the_previous_version_and_its_backup() {
    // Catches migrations run outside their own transaction: migration 4's first statement succeeds
    // and its second fails, and a half-applied step would make every later start fail on a
    // duplicate column.
    let home = home();
    database_at_version(home.path(), 3);
    let live = live_database(home.path());
    Connection::open(&live)
        .unwrap()
        .execute_batch("ALTER TABLE claude_threads ADD COLUMN plan INTEGER NOT NULL DEFAULT 0;")
        .unwrap();
    let before = contents(&live);

    let error = open_error(home.path());

    assert!(
        matches!(error, OpenError::Migration { version: 4, .. }),
        "{error}"
    );
    assert_eq!(user_version(&live), 3);
    let connection = Connection::open(&live).unwrap();
    let added: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('codex_threads') WHERE name = 'plan'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        added, 0,
        "the step that succeeded was rolled back with the one that failed"
    );
    let backups = backup_files(home.path());
    assert_eq!(backups.len(), 1);
    assert!(backups[0].starts_with("kratos-v3-"));
    assert_eq!(
        contents(&home.path().join("backups").join(&backups[0])),
        before
    );
}

#[test]
fn refuses_a_second_open_and_a_restore_while_the_directory_is_held() {
    // Catches a missing lock, which would let two engines migrate and write one database, and a
    // lock that is never released.
    let home = home();
    let store = Store::open(home.path()).unwrap();
    let id: BackupId = "018f0000-0000-7000-8000-000000000000".parse().unwrap();

    let second = open_error(home.path());
    assert!(
        matches!(second, OpenError::Directory(DirectoryError::InUse(_))),
        "{second}"
    );
    assert_eq!(
        second.to_string(),
        format!(
            "another Kratos engine is using the data directory {}",
            home.path().display()
        )
    );
    let during = restore(home.path(), &id).unwrap_err();
    assert!(
        matches!(during, RestoreError::Directory(DirectoryError::InUse(_))),
        "{during}"
    );

    drop(store);
    drop(Store::open(home.path()).unwrap());
    let after = restore(home.path(), &id).unwrap_err();
    assert!(
        matches!(after, RestoreError::UnknownBackup { .. }),
        "the lock was free, so restore got as far as the lookup: {after}"
    );
}
