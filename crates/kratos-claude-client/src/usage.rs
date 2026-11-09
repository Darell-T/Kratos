//! What the CLI says about the account's limits and about the context a turn used, in typed form.
//!
//! The CLI updates itself, so every enum here keeps a value it does not know as `Unrecognized`
//! instead of failing the frame that holds it. The two sources of utilization use different
//! units, and the types here keep each as it arrived: a `rate_limit_event` reports a fraction and
//! Unix seconds, `get_usage` reports a whole percent and an ISO 8601 text.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// The server's verdict in a `rate_limit_event`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RateLimitStatus {
    Allowed,
    AllowedWarning,
    Rejected,
    Unrecognized(String),
}

impl From<String> for RateLimitStatus {
    fn from(text: String) -> Self {
        match text.as_str() {
            "allowed" => Self::Allowed,
            "allowed_warning" => Self::AllowedWarning,
            "rejected" => Self::Rejected,
            _ => Self::Unrecognized(text),
        }
    }
}

impl<'de> Deserialize<'de> for RateLimitStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}

/// Which limit a `rate_limit_event` reports on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RateLimitType {
    FiveHour,
    SevenDay,
    SevenDayOpus,
    SevenDaySonnet,
    SevenDayOverageIncluded,
    Overage,
    Unrecognized(String),
}

impl RateLimitType {
    /// The CLI's own name for the limit, as it sent it.
    pub fn as_str(&self) -> &str {
        match self {
            Self::FiveHour => "five_hour",
            Self::SevenDay => "seven_day",
            Self::SevenDayOpus => "seven_day_opus",
            Self::SevenDaySonnet => "seven_day_sonnet",
            Self::SevenDayOverageIncluded => "seven_day_overage_included",
            Self::Overage => "overage",
            Self::Unrecognized(text) => text,
        }
    }
}

impl From<String> for RateLimitType {
    fn from(text: String) -> Self {
        match text.as_str() {
            "five_hour" => Self::FiveHour,
            "seven_day" => Self::SevenDay,
            "seven_day_opus" => Self::SevenDayOpus,
            "seven_day_sonnet" => Self::SevenDaySonnet,
            "seven_day_overage_included" => Self::SevenDayOverageIncluded,
            "overage" => Self::Overage,
            _ => Self::Unrecognized(text),
        }
    }
}

impl<'de> Deserialize<'de> for RateLimitType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}

/// The answer to `get_usage`. Every key the crate does not read is ignored: the CLI lists about
/// thirty more under `rate_limits`, most of them null.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct UsageRead {
    /// The plan in the spelling `get_usage` uses, which differs from `initialize`'s.
    pub subscription_type: Option<String>,
    /// False when plan limits do not apply to the session, such as an API key.
    pub rate_limits_available: bool,
    pub rate_limits: Option<RateLimits>,
}

/// The windows `get_usage` reports.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct RateLimits {
    pub five_hour: Option<LimitWindow>,
    pub seven_day: Option<LimitWindow>,
    pub seven_day_opus: Option<LimitWindow>,
    pub seven_day_sonnet: Option<LimitWindow>,
}

/// One window of `get_usage`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct LimitWindow {
    /// A whole percent from 0 to 100.
    pub utilization: Option<f64>,
    /// ISO 8601 with microseconds and an offset, such as `2030-01-01T05:00:00.123456+00:00`.
    pub resets_at: Option<String>,
}

/// A `system/api_retry` frame: the CLI's request failed and the CLI will try again by itself.
/// Kratos reads it as a sign that the CLI is still working, never as a reason to send anything.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ApiRetry {
    pub attempt: u32,
    pub max_retries: u32,
    pub retry_delay_ms: u64,
    pub error_status: Option<u16>,
    /// The failure's kind, such as `rate_limit`, as the CLI names it.
    pub error: String,
}

/// The tokens one request of a turn used, from the last entry of the result's `usage.iterations`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IterationTokens {
    pub input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub output_tokens: i64,
}

/// What the result says of the turn's context: the last request's tokens, and each model's window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnContext {
    /// `None` when the result lists no request, so the turn's context is unknown, not zero.
    pub last: Option<IterationTokens>,
    /// The context window of each model in the result's `modelUsage`.
    pub windows: BTreeMap<String, i64>,
}

impl TurnContext {
    pub(crate) fn of(result: &Value) -> TurnContext {
        let last = result["usage"]["iterations"]
            .as_array()
            .and_then(|iterations| iterations.last())
            .and_then(|iteration| {
                let count = |key: &str| iteration[key].as_i64();
                Some(IterationTokens {
                    input_tokens: count("input_tokens")?,
                    cache_creation_input_tokens: count("cache_creation_input_tokens").unwrap_or(0),
                    cache_read_input_tokens: count("cache_read_input_tokens").unwrap_or(0),
                    output_tokens: count("output_tokens")?,
                })
            });
        let windows = result["modelUsage"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(model, usage)| Some((model.clone(), usage["contextWindow"].as_i64()?)))
            .collect();
        TurnContext { last, windows }
    }

    /// The window of the model that answered. A result with one model names it; with several, the
    /// caller says which answered, and an unknown answerer has no window rather than a guessed one.
    pub fn window_of(&self, answering: Option<&str>) -> Option<i64> {
        match (answering, self.windows.len()) {
            (_, 1) => self.windows.values().next().copied(),
            (Some(model), _) => self.windows.get(model).copied(),
            (None, _) => None,
        }
    }
}
