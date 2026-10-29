//! Every process Codex starts ends with the engine (D5). On Windows the job object Kratos puts
//! Codex in ends them even when the engine is killed; on Unix the engine ends Codex's process
//! group when it stops, and a killed engine leaving them behind is later work (D17).

use std::thread;
use std::time::{Duration, Instant};

use fake_agents::process_is_running;
use serde_json::json;

use crate::running_engine::{RunningEngine, WAIT};

/// Starts Codex through the engine and has it start a process of its own, as an agent's tool
/// would. Returns both process ids.
fn codex_with_a_tool(engine: &RunningEngine) -> [u32; 2] {
    // Codex starts on demand; asking for the usage is a demand.
    engine.client().get("/api/usage").unwrap();
    let codex = engine.fake_codex("test/pid", json!({}));
    let tool = engine.fake_codex("test/spawnChild", json!({}));
    [codex, tool].map(|id| u32::try_from(id.as_u64().unwrap()).unwrap())
}

fn wait_until_gone(processes: [u32; 2]) {
    let deadline = Instant::now() + WAIT;
    while processes.iter().any(|&id| process_is_running(id)) {
        assert!(Instant::now() < deadline, "still running: {processes:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
#[test]
fn killing_the_engine_ends_every_process_codex_started() {
    // Catches agent tools that outlive a crashed engine, which the Claude prototype left four of:
    // Windows ends the job when the engine's handle to it closes.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let processes = codex_with_a_tool(&engine);
    assert!(processes.iter().all(|&id| process_is_running(id)));

    engine.kill();

    wait_until_gone(processes);
}

#[cfg(unix)]
#[test]
fn stopping_the_engine_ends_every_process_codex_started() {
    // Catches a shutdown that closes Codex but leaves the tools it started running.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let processes = codex_with_a_tool(&engine);
    assert!(processes.iter().all(|&id| process_is_running(id)));

    let status = engine.interrupt();

    assert!(status.success(), "{status:?}");
    wait_until_gone(processes);
}
