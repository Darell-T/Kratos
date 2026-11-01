use crate::running_engine::{RunningEngine, run_until_exit};

#[test]
fn a_second_engine_on_the_same_data_directory_exits_and_the_first_keeps_serving() {
    // Catches a missing lock, which would let two engines migrate and write one database.
    let home = tempfile::tempdir().unwrap();
    let first = RunningEngine::start(home.path());

    let (status, stderr) = run_until_exit(home.path(), "0");

    assert!(!status.success());
    let expected = format!(
        "another Kratos engine is using the data directory {}",
        home.path().display()
    );
    assert!(stderr.contains(&expected), "stderr was: {stderr}");
    assert_eq!(first.client().get("/api/health").unwrap().status, 200);
}
