//! The Codex provider over a real store, driving `fake-codex`.

mod approvals;
mod commands;
mod events;
mod guards;
#[allow(
    dead_code,
    reason = "shared with the usage tests, which use other helpers"
)]
#[path = "../support/harness.rs"]
mod harness;
mod queue;
mod recovery;
#[cfg(windows)]
mod sandbox;
mod settings;
mod work;
