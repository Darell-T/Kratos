//! The Codex provider over a real store, driving `fake-codex`.
//!
//! The files are modules of one test binary so that they share `harness` without an
//! `allow(dead_code)`: each file uses a different part of it.

mod approvals;
mod commands;
mod events;
mod guards;
mod harness;
mod queue;
mod recovery;
#[cfg(windows)]
mod sandbox;
mod settings;
mod usage;
mod work;
