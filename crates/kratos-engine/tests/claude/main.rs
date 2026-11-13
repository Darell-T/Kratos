//! The Claude provider over a real store, driving the scripted `fake-claude`.
//!
//! The files are modules of one test binary so that they share `harness` without an
//! `allow(dead_code)`: each file uses a different part of it.

mod clock;
mod context;
mod guards;
mod harness;
mod limit_failures;
mod perf;
mod permissions;
mod protected_reads;
mod release;
mod reliability;
mod reports;
mod requests;
mod rules;
mod tools;
mod turns;
mod usage;
