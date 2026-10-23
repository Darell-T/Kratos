//! The files are modules of one test binary so that they share `running_engine` without an
//! `allow(dead_code)`: each file uses a different part of it.

mod data_directory;
mod event_stream;
mod running_engine;
mod server;
mod startup;
