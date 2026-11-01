use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{MessageId, Provider, Timestamp, TurnId};

/// The body of `GET /api/usage`: every provider account the engine has read (D38).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub accounts: Vec<AccountUsage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    pub provider: Provider,
    /// The provider's id for the account, never its email (D24), when the provider gave one.
    pub account: Option<String>,
    /// The plan that pays, such as `plus`, when the provider said.
    pub plan: Option<String>,
    pub allowance: Allowance,
    pub buckets: Vec<UsageBucket>,
    /// When Kratos last heard from the provider about this account.
    pub read_at: Timestamp,
}

/// Whether the provider lets the account use its included quota now. Kratos never turns missing
/// information into permission: no answer is `Unavailable`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Allowance {
    Allowed,
    Blocked,
    Unavailable { reason: String },
}

/// One metered limit, such as Codex's `codex` bucket, with its windows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBucket {
    pub limit_id: String,
    pub name: Option<String>,
    /// The shorter window, such as five hours.
    pub primary: Option<UsageWindow>,
    /// The longer window, such as a week.
    pub secondary: Option<UsageWindow>,
    /// Set when the provider says the bucket ran out, for example `rate_limit_reached`.
    pub reached: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub used_percent: i32,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<Timestamp>,
}

/// A continuation's id, issued by the scheduler before it sends anything, so a retry of the same
/// continuation is recognizable (D39).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContinuationId(String);

impl ContinuationId {
    pub fn new(id: String) -> ContinuationId {
        ContinuationId(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContinuationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The scheduler's plan to carry on a turn that stopped at a usage limit (D39, D41).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Continuation {
    pub id: ContinuationId,
    /// The turn that stopped.
    pub turn_id: TurnId,
    pub waits_on: UsageWait,
    /// When the provider said the limit resets, if it said.
    pub resets_at: Option<Timestamp>,
    #[serde(flatten)]
    pub state: ContinuationState,
}

/// The account and buckets a continuation waits on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWait {
    pub provider: Provider,
    pub account: Option<String>,
    pub limit_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ContinuationState {
    /// At `resetsAt` Kratos asks the provider again, and continues once it says the account may
    /// run.
    Scheduled,
    /// Kratos needs the user's go before it schedules the continuation (D41), for example because
    /// the wait is longer than a five-hour window or the reset time is unknown.
    AwaitingUser {
        why: String,
    },
    /// The continuation message is in the thread's queue.
    Sent {
        message_id: MessageId,
    },
    Cancelled {
        reason: String,
    },
}
