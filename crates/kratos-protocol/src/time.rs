use std::fmt;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// A UTC instant as the engines write it: `2026-10-22T18:45:12.345Z`, the exact text of
/// JavaScript's `Date.toISOString()`.
///
/// Stored and sent as text. Text of this one shape sorts in time order, which the `ORDER BY
/// created_at` listings rely on, so [`Timestamp::at`] is the only place Rust formats a time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp {
    text: String,
    instant: SystemTime,
}

/// Text that is not a timestamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimestampError {
    text: String,
}

impl fmt::Display for TimestampError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?} is not a timestamp", self.text)
    }
}

impl std::error::Error for TimestampError {}

impl Timestamp {
    /// The current wall-clock time, truncated to milliseconds.
    ///
    /// Wall-clock time can step backwards, so a timestamp orders listings for display only. Event
    /// order comes from [`crate::Seq`].
    pub fn now() -> Result<Timestamp, TimestampError> {
        Timestamp::at(SystemTime::now())
    }

    /// `time`, truncated to milliseconds.
    pub fn at(time: SystemTime) -> Result<Timestamp, TimestampError> {
        let mut text = String::new();
        fmt::write(
            &mut text,
            format_args!("{}", humantime::format_rfc3339_millis(time)),
        )
        .map_err(|_| TimestampError {
            text: format!("{time:?}"),
        })?;
        Self::try_from(text)
    }

    /// The instant the text names.
    pub fn instant(&self) -> SystemTime {
        self.instant
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl TryFrom<String> for Timestamp {
    type Error = TimestampError;

    fn try_from(text: String) -> Result<Timestamp, TimestampError> {
        match humantime::parse_rfc3339(&text) {
            Ok(instant) => Ok(Timestamp { text, instant }),
            Err(_) => Err(TimestampError { text }),
        }
    }
}

impl From<Timestamp> for String {
    fn from(timestamp: Timestamp) -> String {
        timestamp.text
    }
}
