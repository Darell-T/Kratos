//! Account usage (D38), and the stops at a usage limit that the scheduler continues (D39, D41).
//!
//! The tracker holds what each provider account may still use. Providers write what their CLI
//! reports; the usage API and the resume scheduler read it. A full read replaces an account's
//! snapshot, and only a full read says whether the account may use its included quota. Missing
//! information never becomes permission: no read, a failed read, or a read without that answer
//! leaves the allowance unavailable. A rolling update only fills in the buckets it names and never
//! clears what it leaves out, as Codex's schema directs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use kratos_protocol::{
    AccountUsage, Allowance, Provider, ThreadId, Timestamp, TurnId, UsageBucket, UsageReport,
};
use tokio::sync::watch;

use crate::clock::{Clock, SystemClock};

/// The time `seconds` after the epoch, for a reset a CLI reports in Unix seconds. A number that is
/// not a time Kratos can write, such as milliseconds or `u64::MAX`, is missing, never a panic.
pub(crate) fn timestamp_of_unix(seconds: u64) -> Option<Timestamp> {
    let time = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))?;
    Timestamp::at(time).ok()
}

/// A turn that stopped at a usage limit, reported by its provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitStop {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub provider: Provider,
}

#[derive(Clone)]
pub struct UsageTracker {
    accounts: Arc<Mutex<HashMap<Provider, Tracked>>>,
    /// Counts changes, so the scheduler wakes when something it waits on may have changed.
    changes: watch::Sender<u64>,
    /// Tells providers what time it is when they judge a snapshot, such as a window that is still
    /// spent. Tests move it by hand.
    clock: Arc<dyn Clock>,
}

struct Tracked {
    usage: Option<AccountUsage>,
    /// Counts account changes. A read that started under an older generation is stale.
    generation: u64,
}

impl Default for UsageTracker {
    fn default() -> UsageTracker {
        UsageTracker::with_clock(Arc::new(SystemClock))
    }
}

impl UsageTracker {
    pub fn with_clock(clock: Arc<dyn Clock>) -> UsageTracker {
        UsageTracker {
            accounts: Arc::default(),
            changes: watch::channel(0).0,
            clock,
        }
    }

    /// The time providers judge a snapshot at.
    pub(crate) fn now(&self) -> SystemTime {
        self.clock.now()
    }

    /// The account generation a read of `provider` starts under.
    pub(crate) fn generation(&self, provider: Provider) -> u64 {
        self.accounts()
            .get(&provider)
            .map_or(0, |tracked| tracked.generation)
    }

    /// Replaces the provider's snapshot with a full read made under `generation`. Returns false,
    /// changing nothing, when the account changed while the read was out.
    pub(crate) fn record_read(&self, generation: u64, usage: AccountUsage) -> bool {
        let recorded = {
            let mut accounts = self.accounts();
            let tracked = accounts.entry(usage.provider).or_insert(Tracked {
                usage: None,
                generation: 0,
            });
            let is_current = tracked.generation == generation;
            if is_current {
                tracked.usage = Some(usage);
            }
            is_current
        };
        if recorded {
            self.changed();
        }
        recorded
    }

    /// Merges a rolling update that states the allowance, such as Claude's `rate_limit_event`. A
    /// window the update carries replaces the snapshot's, and a bucket it names takes its reached
    /// limit, so an allowed event clears what an earlier rejection set. Buckets it leaves out stay.
    /// It creates the snapshot when there is none, because the update says whether the account may
    /// run without a full read.
    pub(crate) fn record_update(&self, update: AccountUsage) {
        {
            let mut accounts = self.accounts();
            let tracked = accounts.entry(update.provider).or_insert(Tracked {
                usage: None,
                generation: 0,
            });
            match tracked.usage.as_mut() {
                None => tracked.usage = Some(update),
                Some(usage) => {
                    usage.allowance = update.allowance;
                    usage.read_at = update.read_at;
                    usage.plan = update.plan.or(usage.plan.take());
                    for bucket in update.buckets {
                        match usage
                            .buckets
                            .iter_mut()
                            .find(|known| known.limit_id == bucket.limit_id)
                        {
                            Some(known) => replace(known, bucket),
                            None => usage.buckets.push(bucket),
                        }
                    }
                }
            }
        }
        self.changed();
    }

    /// A read of the provider failed. What was known stays for display, but nothing may be read as
    /// permission until the next full read.
    pub(crate) fn record_unavailable(&self, provider: Provider, reason: String) {
        if let Some(usage) = self
            .accounts()
            .get_mut(&provider)
            .and_then(|tracked| tracked.usage.as_mut())
        {
            usage.allowance = Allowance::Unavailable { reason };
        }
        self.changed();
    }

    /// Merges a rolling update for one bucket into the latest snapshot. A field the update leaves
    /// out keeps its last known value.
    pub(crate) fn merge_bucket(&self, provider: Provider, update: UsageBucket) {
        let read_at = match Timestamp::now() {
            Ok(read_at) => read_at,
            Err(error) => {
                self.record_unavailable(provider, error.to_string());
                return;
            }
        };
        {
            let mut accounts = self.accounts();
            let Some(usage) = accounts
                .get_mut(&provider)
                .and_then(|tracked| tracked.usage.as_mut())
            else {
                // Nothing to merge into: the next full read brings the whole picture.
                return;
            };
            match usage
                .buckets
                .iter_mut()
                .find(|bucket| bucket.limit_id == update.limit_id)
            {
                Some(bucket) => merge(bucket, update),
                None => usage.buckets.push(update),
            }
            usage.read_at = read_at;
        }
        self.changed();
    }

    /// The signed-in account or its plan changed: every snapshot and read in flight describes an
    /// account that is gone.
    pub(crate) fn account_changed(&self, provider: Provider) {
        {
            let mut accounts = self.accounts();
            let tracked = accounts.entry(provider).or_insert(Tracked {
                usage: None,
                generation: 0,
            });
            tracked.generation += 1;
            tracked.usage = None;
        }
        self.changed();
    }

    pub fn account(&self, provider: Provider) -> Option<AccountUsage> {
        self.accounts()
            .get(&provider)
            .and_then(|tracked| tracked.usage.clone())
    }

    pub fn report(&self) -> UsageReport {
        let accounts = self.accounts();
        let mut report: Vec<AccountUsage> = accounts
            .values()
            .filter_map(|tracked| tracked.usage.clone())
            .collect();
        report.sort_by_key(|usage| usage.provider.as_str());
        UsageReport { accounts: report }
    }

    /// A receiver that wakes after every change.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn changed(&self) {
        self.changes.send_modify(|count| *count += 1);
    }

    fn accounts(&self) -> MutexGuard<'_, HashMap<Provider, Tracked>> {
        // No code panics while holding this lock, so a poisoned lock still guards a consistent map.
        self.accounts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn replace(bucket: &mut UsageBucket, update: UsageBucket) {
    if update.name.is_some() {
        bucket.name = update.name;
    }
    if update.primary.is_some() {
        bucket.primary = update.primary;
    }
    if update.secondary.is_some() {
        bucket.secondary = update.secondary;
    }
    bucket.reached = update.reached;
}

fn merge(bucket: &mut UsageBucket, update: UsageBucket) {
    if update.name.is_some() {
        bucket.name = update.name;
    }
    if update.primary.is_some() {
        bucket.primary = update.primary;
    }
    if update.secondary.is_some() {
        bucket.secondary = update.secondary;
    }
    if update.reached.is_some() {
        bucket.reached = update.reached;
    }
}

#[cfg(test)]
mod tests {
    use kratos_protocol::UsageWindow;

    use super::*;

    fn bucket(limit_id: &str, used_percent: i32) -> UsageBucket {
        UsageBucket {
            limit_id: limit_id.to_owned(),
            name: Some(limit_id.to_owned()),
            primary: Some(UsageWindow {
                used_percent,
                window_minutes: Some(300),
                resets_at: None,
            }),
            secondary: None,
            reached: None,
        }
    }

    fn read(allowance: Allowance, buckets: Vec<UsageBucket>) -> AccountUsage {
        AccountUsage {
            provider: Provider::Codex,
            account: Some("acct".into()),
            plan: Some("plus".into()),
            allowance,
            buckets,
            read_at: Timestamp::now().unwrap(),
        }
    }

    #[test]
    fn a_rolling_update_fills_in_one_bucket_and_never_grants_permission() {
        // Catches a merge that drops the buckets an update leaves out, clears a reached limit, or
        // turns a blocked account into an allowed one because a percentage fell.
        let tracker = UsageTracker::default();
        let mut weekly = bucket("weekly", 100);
        weekly.reached = Some("rate_limit_reached".into());
        tracker.record_read(
            0,
            read(Allowance::Blocked, vec![bucket("codex", 90), weekly]),
        );

        let mut update = bucket("weekly", 10);
        update.name = None;
        tracker.merge_bucket(Provider::Codex, update);

        let usage = tracker.account(Provider::Codex).unwrap();
        assert_eq!(usage.allowance, Allowance::Blocked);
        assert_eq!(usage.buckets.len(), 2);
        let weekly = &usage.buckets[1];
        assert_eq!(weekly.primary.as_ref().unwrap().used_percent, 10);
        assert_eq!(weekly.name.as_deref(), Some("weekly"));
        assert_eq!(weekly.reached.as_deref(), Some("rate_limit_reached"));
    }

    #[test]
    fn a_read_that_started_before_an_account_change_is_dropped() {
        // Catches a stale read for the previous account landing after a switch, which would let
        // the scheduler send work on the strength of another account's quota.
        let tracker = UsageTracker::default();
        let before = tracker.generation(Provider::Codex);
        tracker.account_changed(Provider::Codex);

        let recorded = tracker.record_read(before, read(Allowance::Allowed, Vec::new()));

        assert!(!recorded);
        assert_eq!(tracker.account(Provider::Codex), None);
        assert!(tracker.record_read(
            tracker.generation(Provider::Codex),
            read(Allowance::Allowed, Vec::new())
        ));
    }

    #[test]
    fn an_update_that_states_the_allowance_creates_the_snapshot_and_a_later_one_clears_the_rejection()
     {
        // Catches an event dropped because no full read came first, and a rejection that outlives
        // the allowed event after the reset, which would hold a thread that may run.
        let tracker = UsageTracker::default();
        let mut rejected = bucket("unified", 100);
        rejected.reached = Some("five_hour".into());
        let mut other = read(Allowance::Blocked, vec![rejected, bucket("overage", 100)]);
        other.provider = Provider::Claude;
        tracker.record_update(other);
        assert_eq!(
            tracker.account(Provider::Claude).unwrap().allowance,
            Allowance::Blocked
        );

        let mut allowed = read(Allowance::Allowed, vec![bucket("unified", 20)]);
        allowed.provider = Provider::Claude;
        tracker.record_update(allowed);

        let usage = tracker.account(Provider::Claude).unwrap();
        assert_eq!(usage.allowance, Allowance::Allowed);
        assert_eq!(
            usage.buckets.len(),
            2,
            "the bucket the update left out stays"
        );
        assert_eq!(usage.buckets[0].primary.as_ref().unwrap().used_percent, 20);
        assert_eq!(usage.buckets[0].reached, None);
    }
}
