//! The Kratos engine: one process per machine that owns the records, drives the agent CLIs, and
//! serves the local API. `main.rs` starts it; the integration tests in `tests/` drive its parts.

pub mod clock;
pub mod config;
pub mod hub;
pub mod mcp;
pub mod providers;
pub mod scheduler;
pub mod server;
pub mod store_thread;
pub mod usage;
