use std::process::{Command, Output};

/// Nothing listens on port 9, so a command that reached the network would exit 1, not 2.
const NOWHERE: &str = "http://127.0.0.1:9";

fn kratos_client(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kratos-client"))
        .args(["--url", NOWHERE])
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn a_mistyped_command_line_is_a_usage_error_and_sends_nothing() {
    // Catches a lane that silently runs a different request after a typo: an unknown command or
    // option, a missing argument, or a number that is not one.
    let mistakes: [&[&str]; 8] = [
        &[],
        &["nope"],
        &["health", "extra"],
        &["post", "only-a-thread-id"],
        &["subscribe", "thread", "--afte", "1"],
        &["subscribe", "thread", "--count", "many"],
        &["request", "GET", "/api/health", "--body-bytes", "3"],
        &["request", "POST", "/api/threads", "--header", "no-colon"],
    ];
    for mistake in mistakes {
        let output = kratos_client(mistake);
        assert_eq!(output.status.code(), Some(2), "{mistake:?}");
        assert!(output.stdout.is_empty(), "{mistake:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage: kratos-client"),
            "{mistake:?}"
        );
    }
}

#[test]
fn an_unreachable_engine_exits_1_with_a_json_error() {
    // Catches a client that prints nothing, or exits 0, when the engine is down, so a lane would
    // read a dead engine as a pass.
    let output = kratos_client(&["health"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .starts_with("cannot reach the engine at 127.0.0.1:9")
    );
}
