//! The Teams rules, driven end to end through the same loop the engine runs. The loop folds the log,
//! asks `next_step`, runs the step with scripted agents, appends the result, and lets the user decide.
//!
//! One file per behavior. Each test names the mistake it would catch.

mod observe;
mod replay;
mod support;
mod typescript;

mod cancellation;
mod disputes;
mod evidence_rules;
mod handoff;
mod lane_logs;
mod permissions;
mod plan_changes;
mod planning;
mod probes;
mod prompts;
mod questions;
mod reference_logs;
mod report_formats;
mod review_depth;
mod review_loop;
mod role_changes;
mod security_triggers;
mod usage_limits;
mod user_messages;
