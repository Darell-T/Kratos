//! A stand-in for the GitHub CLI. It appends its arguments, as one JSON array, to the file
//! `FAKE_GH_LOG` names, and answers as `gh pr create` does with the new pull request's address.
//!
//! When the folder `FAKE_GH_GATE` names holds a file `hold`, the fake writes `held` there and
//! waits for a file `open` before it answers, so a test can act while Kratos waits on `gh`.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// The address every pull request the fake opens gets.
const PULL_REQUEST: &str = "https://github.com/example/repo/pull/1";

/// The longest the fake waits at a closed gate, so a test that never opens it still ends.
const MAX_HOLD: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    if let Some(gate) = std::env::var_os("FAKE_GH_GATE") {
        wait_at(Path::new(&gate));
    }
    let Some(log) = std::env::var_os("FAKE_GH_LOG") else {
        eprintln!("fake-gh: FAKE_GH_LOG is not set");
        return ExitCode::FAILURE;
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let line = serde_json::to_string(&args).expect("strings always serialize");
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(error) = written {
        eprintln!("fake-gh: cannot write {}: {error}", log.to_string_lossy());
        return ExitCode::FAILURE;
    }
    println!("{PULL_REQUEST}");
    ExitCode::SUCCESS
}

fn wait_at(gate: &Path) {
    if !gate.join("hold").exists() {
        return;
    }
    if let Err(error) = std::fs::write(gate.join("held"), "") {
        eprintln!("fake-gh: cannot write {}: {error}", gate.display());
        return;
    }
    let deadline = Instant::now() + MAX_HOLD;
    while !gate.join("open").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}
