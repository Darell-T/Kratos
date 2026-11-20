//! A stand-in for the GitHub CLI. It appends its arguments, as one JSON array, to the file
//! `FAKE_GH_LOG` names, and answers as `gh pr create` does with the new pull request's address.

use std::io::Write;
use std::process::ExitCode;

/// The address every pull request the fake opens gets.
const PULL_REQUEST: &str = "https://github.com/example/repo/pull/1";

fn main() -> ExitCode {
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
