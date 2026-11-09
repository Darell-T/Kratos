//! The Claude provider over a real store, driving the scripted `fake-claude`.
//!
//! The files are modules of one test binary so that they share `harness` without an
//! `allow(dead_code)`: each file uses a different part of it.

mod harness;
mod permissions;
mod reliability;
mod rules;
mod turns;
