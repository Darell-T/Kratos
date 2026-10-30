//! Account usage (D38), and the stops at a usage limit that the scheduler continues (D39, D41).

mod tracker;

use kratos_protocol::{Provider, ThreadId, TurnId};

pub use tracker::UsageTracker;

/// A turn that stopped at a usage limit, reported by its provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitStop {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub provider: Provider,
}
