use crate::running_engine::run_until_exit;

#[test]
fn an_invalid_port_stops_the_engine_instead_of_falling_back_to_the_default() {
    // Catches a port that falls back to 4200, where the engine would attach to a user's real engine
    // or fail on its port, instead of telling a lane that its configuration is wrong.
    let home = tempfile::tempdir().unwrap();

    for port in ["70000", "-1", "abc", "4200.5"] {
        let (status, stderr) = run_until_exit(home.path(), port);

        assert!(!status.success(), "KRATOS_PORT={port}");
        let expected =
            format!("KRATOS_PORT must be an integer between 0 and 65535, got \"{port}\"");
        assert_eq!(stderr.trim(), expected);
    }
}
