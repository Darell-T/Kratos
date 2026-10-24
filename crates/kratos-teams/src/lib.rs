//! The Teams rules are pure functions over a recorded log of facts, with no I/O, clocks, or randomness.
//!
//! A Team is two or three members that plan, build, and review each other's work on one task. Kratos's
//! own code, and not a model, holds everyone to the process (D29), and this crate is that code. The
//! engine runs it with real agents, git, and SQLite.
//!
//! ```text
//! facts --fold--> TaskView --next_step--> Step --the engine runs it--> new fact --append--> facts
//! ```
//!
//! | Module              | Owns |
//! | ------------------- | ---- |
//! | `facts`             | ids, settings, and the fact union the engine appends |
//! | `members`           | roles, members, and which member holds which role |
//! | `view`              | the task view: phases, turns, findings, and the questions asked of them |
//! | `fold`              | replaying facts into the view, and refusing a fact that does not apply |
//! | `next_step`         | the one step that must happen next |
//! | `decisions`         | what the user may do, when, and what each choice does |
//! | `task_plan`         | the plan document, the scope, and probe constraints |
//! | `reports`           | what agents hand back: wire shapes, schemas, and typed reports |
//! | `evidence`          | location, quote, and command checks, and the diff parser |
//! | `prompts`           | every message an agent receives, and the task title, commit subject, and branch name |
//! | `security_triggers` | which diffs get a security review |
//! | `permissions`       | what each role may do without asking |
//!
//! A restart needs no special code. Fold the same facts and ask `next_step` again. The engine appends a
//! result only after `TaskView::accepts` says it still applies.

pub mod decisions;
pub mod evidence;
pub mod facts;
pub mod fold;
pub mod members;
pub mod next_step;
pub mod permissions;
pub mod prompts;
pub mod reports;
pub mod security_triggers;
pub mod task_plan;
pub mod view;

// These add methods to `TaskView`. They have nothing to export.
mod rounds;
mod turns;
mod user_messages;
