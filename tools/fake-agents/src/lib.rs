//! Fakes of the agent CLIs that the engine's tests drive in place of the real ones.
//!
//! `fake-codex` speaks `codex app-server`'s JSON-RPC over stdio. Tests steer it through `test/*`
//! requests: they hold and settle turns, emit notifications, ask the client questions, and read
//! back what the client sent. The engine never links this crate; tests run its binaries.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};

/// The path of the fake named `name`, such as `fake-codex`, built on first use.
///
/// Cargo hands a test the paths of its own package's binaries only, so tests in other packages ask
/// for a fake here. The first call in a test process builds it with the cargo that runs the tests,
/// into the same target folder. Panics when the build fails, because no test can run without it.
pub fn executable(name: &str) -> PathBuf {
    static BUILT: Mutex<Option<HashMap<String, PathBuf>>> = Mutex::new(None);
    let mut built = BUILT.lock().unwrap_or_else(PoisonError::into_inner);
    let built = built.get_or_insert_with(HashMap::new);
    if let Some(path) = built.get(name) {
        return path.clone();
    }
    let path = build(name);
    built.insert(name.to_owned(), path.clone());
    path
}

fn build(name: &str) -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut child = Command::new(cargo)
        .args(["build", "--package", "fake-agents", "--bin", name])
        .args(["--message-format", "json-render-diagnostics"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run cargo to build {name}: {error}"));
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut executable = None;
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.expect("cargo writes UTF-8 JSON lines");
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == name
            && let Some(path) = message["executable"].as_str()
        {
            executable = Some(PathBuf::from(path));
        }
    }
    let status = child.wait().expect("cargo runs to its end");
    assert!(status.success(), "cargo could not build {name}");
    executable.unwrap_or_else(|| panic!("cargo built {name} but named no executable"))
}

/// Whether process `id` still runs. A zombie, which a container's init may never reap, counts as
/// ended.
pub fn process_is_running(id: u32) -> bool {
    if cfg!(windows) {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {id}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("tasklist runs");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{id}\""))
    } else {
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p", &id.to_string()])
            .output()
            .expect("ps runs");
        let state = String::from_utf8_lossy(&output.stdout);
        let state = state.trim();
        !state.is_empty() && !state.starts_with('Z')
    }
}
