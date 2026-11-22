//! Runs `FAKE_GIT_REAL` with the same arguments and streams. When consecutive arguments match the
//! whitespace-separated tokens in `FAKE_GIT_HOLD_ON` and `FAKE_GIT_GATE` contains `hold`, the
//! first matching run renames `hold` to `held` and waits for `open` before starting git.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

/// The longest the fake waits at a closed gate, so a test that never opens it still ends.
const MAX_HOLD: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let Some(real) = std::env::var_os("FAKE_GIT_REAL") else {
        eprintln!("fake-git: FAKE_GIT_REAL is not set");
        return ExitCode::FAILURE;
    };
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if let (Some(gate), Some(hold_on)) = (
        std::env::var_os("FAKE_GIT_GATE"),
        std::env::var_os("FAKE_GIT_HOLD_ON"),
    ) && matches_hold(&args, &hold_on)
    {
        wait_at(Path::new(&gate));
    }
    match Command::new(&real).args(&args).status() {
        Ok(status) => ExitCode::from(
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1),
        ),
        Err(error) => {
            eprintln!("fake-git: cannot run {}: {error}", real.to_string_lossy());
            ExitCode::FAILURE
        }
    }
}

fn matches_hold(args: &[OsString], hold_on: &std::ffi::OsStr) -> bool {
    let Some(text) = hold_on.to_str() else {
        return args.iter().any(|arg| arg == hold_on);
    };
    let needle: Vec<&str> = text.split_whitespace().collect();
    !needle.is_empty()
        && args.windows(needle.len()).any(|window| {
            window
                .iter()
                .zip(&needle)
                .all(|(arg, wanted)| arg == wanted)
        })
}

fn wait_at(gate: &Path) {
    // A rename succeeds once, so only the first matching run waits.
    if std::fs::rename(gate.join("hold"), gate.join("held")).is_err() {
        return;
    }
    let deadline = Instant::now() + MAX_HOLD;
    while !gate.join("open").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}
