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
            .and_then(ContextLimit::from_stored)
    }

    /// Reads the signed storage representation, refusing counts below 1.
    pub fn from_stored(tokens: i64) -> Option<ContextLimit> {
        (tokens > 0).then_some(ContextLimit(tokens))
    }

    pub fn get(self) -> u64 {
        // Both constructors require a positive i64, so unsigned_abs never discards a sign.
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

#[cfg(test)]
mod tests {
    use super::ContextLimit;

    #[test]
    fn stored_context_limits_keep_the_positive_signed_range() {
        // Catches negative or zero stored counts becoming valid unsigned limits.
        assert_eq!(ContextLimit::from_stored(-1), None);
        assert_eq!(ContextLimit::from_stored(0), None);
        assert_eq!(ContextLimit::from_stored(1).unwrap().get(), 1);
        let largest = ContextLimit::from_stored(i64::MAX).unwrap();
        assert_eq!(i64::from(largest), i64::MAX);
        assert_eq!(largest.get(), 9_223_372_036_854_775_807);
    }
}
