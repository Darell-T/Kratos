use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// A UTC instant as the engines write it: `2026-10-22T18:45:12.345Z`, the exact text of
/// JavaScript's `Date.toISOString()`.
///
/// Stored and sent as text. Text of this one shape sorts in time order, which the `ORDER BY
/// created_at` listings rely on, so [`Timestamp::now`] is the only place Rust formats a time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(String);

impl Timestamp {
    /// The current wall-clock time, truncated to milliseconds.
    ///
    /// Wall-clock time can step backwards, so a timestamp orders listings for display only. Event
    /// order comes from [`crate::Seq`].
    pub fn now() -> Timestamp {
        Timestamp(humantime::format_rfc3339_millis(SystemTime::now()).to_string())
    }

    pub fn from_stored(text: String) -> Timestamp {
        Timestamp(text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
