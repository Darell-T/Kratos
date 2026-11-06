use serde::de::{Deserializer, Error};
use serde::{Deserialize, Serialize};

/// A count of context tokens a client may ask for: at least 1 and at most `i64::MAX`, the largest
/// count the engine's database holds.
///
/// A provider may refuse more, such as a count below its own floor or above the window its model
/// reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ContextLimit(i64);

impl ContextLimit {
    /// `None` for 0 and for a count above `i64::MAX`.
    pub fn new(tokens: u64) -> Option<ContextLimit> {
        i64::try_from(tokens)
            .ok()
            .filter(|tokens| *tokens > 0)
            .map(ContextLimit)
    }

    pub fn get(self) -> u64 {
        self.0.unsigned_abs()
    }
}

impl From<ContextLimit> for i64 {
    fn from(limit: ContextLimit) -> i64 {
        limit.0
    }
}

impl<'de> Deserialize<'de> for ContextLimit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let tokens = u64::deserialize(deserializer)?;
        ContextLimit::new(tokens)
            .ok_or_else(|| D::Error::custom(format!("must be between 1 and {}", i64::MAX)))
    }
}
