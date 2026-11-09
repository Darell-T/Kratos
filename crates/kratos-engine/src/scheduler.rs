//! The one owner of automatic continuation (D39).
//!
//! When a turn stops at a usage limit, the scheduler plans its continuation and keeps the plan in
//! the store, so a restart picks it up (L6). A stop at a five-hour window resumes on its own at the
//! reset; any longer or unknown wait waits for the user's go (D41). At the reset the scheduler asks
//! the provider for the account's usage again, and sends the continuation only when that fresh read
//! says the account may run and no bucket it waited on is still spent. A reset time is when to
//! ask, never permission to send.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use kratos_protocol::{
    AccountUsage, Allowance, Continuation, ContinuationId, ContinuationState, PauseReason,
    Provider, ThreadId, Timestamp, UsageBucket, UsageWait, UsageWindow,
};
use kratos_store::StoreError;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

pub use crate::clock::{Clock, SystemClock};
use crate::hub::Hub;
use crate::providers::claude::ClaudeProvider;
use crate::providers::codex::CodexProvider;
use crate::providers::{Agent, Continued, ProviderError};
use crate::store_thread::StoreThread;
use crate::usage::{LimitStop, UsageTracker};

/// D41: a stop at a window this long resumes on its own at the reset.
const AUTOMATIC_WINDOW_MINUTES: i64 = 300;
/// When a check finds no usable answer, the next one waits this long.
const RECHECK_AFTER: Duration = Duration::from_secs(5 * 60);
/// Checks in a row without a usable answer before the user decides.
const MAX_UNANSWERED_CHECKS: u32 = 3;
/// The first wait before trying again after the store failed. Each further failure doubles it.
const STORE_RETRY_BASE: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("Unknown continuation {0}")]
    UnknownContinuation(ContinuationId),
    #[error("{0}")]
    Conflict(String),
    /// The scheduler has stopped, as it does when the engine shuts down. The text is the one the
    /// API answered when the scheduler reported its failures as the provider's.
    #[error("The Codex provider has stopped")]
    Stopped,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The providers the scheduler reaches, each only while it runs. A provider that is not running
/// answers a usage read as failed, so its continuations go to the user.
#[derive(Clone, Default)]
pub struct Providers {
    pub codex: Option<CodexProvider>,
    pub claude: Option<ClaudeProvider>,
}

impl From<CodexProvider> for Providers {
    fn from(codex: CodexProvider) -> Providers {
        Providers {
            codex: Some(codex),
            claude: None,
        }
    }
}

impl Providers {
    pub fn is_empty(&self) -> bool {
        self.codex.is_none() && self.claude.is_none()
    }

    fn agent(&self, provider: Provider) -> Result<Agent<'_>, ProviderError> {
        let agent = match provider {
            Provider::Codex => self.codex.as_ref().map(Agent::Codex),
            Provider::Claude => self.claude.as_ref().map(Agent::Claude),
        };
        agent.ok_or_else(|| {
            ProviderError::Unavailable(format!("{} is not running", display(provider)))
        })
    }
}

/// The provider's name as the plan texts spell it.
fn display(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "Codex",
        Provider::Claude => "Claude",
    }
}

/// What the scheduler plans for a stop, from the account's latest usage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub waits_on: UsageWait,
    pub resets_at: Option<Timestamp>,
    pub state: ContinuationState,
}

/// Plans a continuation from the usage known at the stop (D41). Automatic only when every spent
/// window is a five-hour window with a known reset; anything else waits for the user, who may let
/// Kratos resume at the reset.
pub fn plan(provider: Provider, account: Option<&AccountUsage>) -> Plan {
    let spent = account.map(spent_windows).unwrap_or_default();
    let waits_on = UsageWait {
        provider,
        account: account.and_then(|account| account.account.clone()),
        limit_ids: spent.iter().map(|(bucket, _)| bucket.clone()).collect(),
    };
    let resets_at = spent
        .iter()
        .map(|(_, window)| window.resets_at.clone())
        .collect::<Option<Vec<Timestamp>>>()
        .and_then(|resets| resets.into_iter().max_by_key(Timestamp::instant));
    let name = display(provider);
    let why = if spent.is_empty() {
        Some(format!("{name} did not say which limit stopped the turn"))
    } else if resets_at.is_none() {
        Some(format!("{name} did not say when the limit resets"))
    } else if spent
        .iter()
        .any(|(_, window)| window.window_minutes != Some(AUTOMATIC_WINDOW_MINUTES))
    {
        Some("The wait is longer than the five-hour window".to_owned())
    } else {
        None
    };
    let state = match why {
        Some(why) => ContinuationState::AwaitingUser { why },
        None => ContinuationState::Scheduled,
    };
    Plan {
        waits_on,
        resets_at,
        state,
    }
}

/// Each bucket the account has spent, with the window that ran out: a reached bucket, or a window
/// at 100 percent.
fn spent_windows(account: &AccountUsage) -> Vec<(String, UsageWindow)> {
    account
        .buckets
        .iter()
        .filter_map(|bucket| spent_window(bucket).map(|window| (bucket.limit_id.clone(), window)))
        .collect()
}

fn spent_window(bucket: &UsageBucket) -> Option<UsageWindow> {
    let full = |window: &&UsageWindow| window.used_percent >= 100;
    let windows = [bucket.secondary.as_ref(), bucket.primary.as_ref()];
    let full_window = windows.into_iter().flatten().find(full);
    match (full_window, &bucket.reached) {
        (Some(window), _) => Some(window.clone()),
        // Spent, though no window says which: the longer one is the safe guess for when to ask.
        (None, Some(_)) => bucket
            .secondary
            .clone()
            .or_else(|| bucket.primary.clone())
            .or(Some(UsageWindow {
                used_percent: 100,
                window_minutes: None,
                resets_at: None,
            })),
        (None, None) => None,
    }
}

/// Whether a fresh read lets the continuation go: the account may use its included quota, it is
/// the account the continuation waited on, and none of the buckets it waited on is still spent.
pub(crate) fn may_continue(waits_on: &UsageWait, account: Option<&AccountUsage>) -> Readiness {
    let Some(account) = account else {
        return Readiness::Unanswered;
    };
    if waits_on.account.is_some() && account.account != waits_on.account {
        return Readiness::AccountChanged;
    }
    match &account.allowance {
        Allowance::Unavailable { .. } => return Readiness::Unanswered,
        Allowance::Blocked => return Readiness::StillSpent,
        Allowance::Allowed => {}
    }
    let still_spent = spent_windows(account)
        .iter()
        .any(|(limit_id, _)| waits_on.limit_ids.contains(limit_id));
    if still_spent {
        Readiness::StillSpent
    } else {
        Readiness::Ready
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Readiness {
    Ready,
    StillSpent,
    /// No answer, or an answer that says nothing about permission.
    Unanswered,
    AccountChanged,
}

enum Request {
    /// The user's go for a continuation that waits on them (D41).
    Schedule {
        continuation_id: ContinuationId,
        reply: oneshot::Sender<Result<Continuation, SchedulerError>>,
    },
}

#[derive(Clone)]
pub struct SchedulerHandle {
    requests: mpsc::Sender<Request>,
}

impl SchedulerHandle {
    /// Lets Kratos resume the continuation on its own at the reset. Refused when the reset time is
    /// unknown, because then there is nothing to wait for.
    pub async fn schedule(
        &self,
        continuation_id: ContinuationId,
    ) -> Result<Continuation, SchedulerError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(Request::Schedule {
                continuation_id,
                reply,
            })
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        answer.await.map_err(|_| SchedulerError::Stopped)?
    }
}

struct Scheduler {
    hub: Hub,
    store: StoreThread,
    usage: UsageTracker,
    providers: Providers,
    clock: Arc<dyn Clock>,
    /// Checks in a row without a usable answer, by continuation.
    unanswered: HashMap<ContinuationId, u32>,
    /// Store failures in a row, by continuation, which set how long its next retry waits.
    store_failures: HashMap<ContinuationId, u32>,
    due: mpsc::UnboundedSender<ContinuationId>,
}

/// Re-arms open continuations and recovers durable quota pauses missing their plans (L6), then
/// plans each stop its provider reports. Runs until every [`SchedulerHandle`] is dropped.
pub async fn start(
    hub: Hub,
    store: StoreThread,
    usage: UsageTracker,
    providers: impl Into<Providers>,
    clock: Arc<dyn Clock>,
    mut stops: mpsc::UnboundedReceiver<LimitStop>,
) -> Result<SchedulerHandle, StoreError> {
    let (due, mut due_ids) = mpsc::unbounded_channel();
    let mut scheduler = Scheduler {
        hub,
        store: store.clone(),
        usage,
        providers: providers.into(),
        clock,
        unanswered: HashMap::new(),
        store_failures: HashMap::new(),
        due,
    };
    for (_, continuation) in store.call(|store| store.open_continuations()).await? {
        if continuation.state == ContinuationState::Scheduled {
            scheduler.arm(&continuation.id, continuation.resets_at.as_ref());
        }
    }
    let mut unplanned_stops = Vec::new();
    for provider in [Provider::Codex, Provider::Claude] {
        let stops = store
            .call(move |store| store.unplanned_usage_stops(provider))
            .await?;
        unplanned_stops.extend(stops.into_iter().map(|(thread_id, turn_id)| LimitStop {
            thread_id,
            turn_id,
            provider,
        }));
    }
    let (requests, mut incoming) = mpsc::channel(16);
    tokio::spawn(async move {
        // Provider refreshes must not delay the HTTP listener becoming usable.
        for stop in unplanned_stops {
            if let Err(error) = scheduler.on_stop(stop).await {
                eprintln!("cannot recover a usage continuation: {error}");
            }
        }
        loop {
            tokio::select! {
                Some(stop) = stops.recv() => {
                    if let Err(error) = scheduler.on_stop(stop).await {
                        eprintln!("cannot plan a usage continuation: {error}");
                    }
                },
                Some(id) = due_ids.recv() => scheduler.on_due(id).await,
                request = incoming.recv() => match request {
                    Some(request) => scheduler.on_request(request).await,
                    // The engine dropped every handle: it is shutting down.
                    None => return,
                },
            }
        }
    });
    Ok(SchedulerHandle { requests })
}

impl Scheduler {
    fn arm(&self, id: &ContinuationId, at: Option<&Timestamp>) {
        let at = at
            .map(Timestamp::instant)
            .unwrap_or_else(|| self.clock.now() + RECHECK_AFTER);
        self.arm_at(id, at);
    }

    fn arm_at(&self, id: &ContinuationId, at: SystemTime) {
        let (sleep, due, id) = (self.clock.sleep_until(at), self.due.clone(), id.clone());
        tokio::spawn(async move {
            sleep.await;
            let _ = due.send(id);
        });
    }

    async fn on_stop(&self, stop: LimitStop) -> Result<(), StoreError> {
        // A fresh read, so the plan rests on what the provider says now. A failed read leaves the
        // last known usage, which the plan treats as no evidence where it lacks any.
        let _ = self.refresh(stop.provider, &stop.thread_id).await;
        let planned = plan(stop.provider, self.usage.account(stop.provider).as_ref());
        let continuation = Continuation {
            id: ContinuationId::new(Uuid::new_v4().to_string()),
            turn_id: stop.turn_id.clone(),
            waits_on: planned.waits_on,
            resets_at: planned.resets_at,
            state: planned.state,
        };
        let (id, resets_at, is_scheduled) = (
            continuation.id.clone(),
            continuation.resets_at.clone(),
            continuation.state == ContinuationState::Scheduled,
        );
        let added = self
            .hub
            .write(stop.thread_id, move |write| {
                let thread = write.provider_thread()?;
                let is_source_pause = matches!(thread.paused, Some(PauseReason::UsageLimit { turn_id, .. }) if turn_id == stop.turn_id);
                if thread.thread.provider != stop.provider
                    || thread.running_turn.is_some()
                    || !is_source_pause
                    || write.open_continuation()?.is_some()
                {
                    // The refresh may finish after the user changed the pause or turn.
                    return Ok(false);
                }
                write.add_continuation(continuation)
            })
            .await?;
        if added.value && is_scheduled {
            self.arm(&id, resets_at.as_ref());
        }
        Ok(())
    }

    /// A scheduled continuation's time came: ask the provider, then send, wait again, or hand the
    /// decision to the user.
    async fn on_due(&mut self, id: ContinuationId) {
        let lookup = id.clone();
        let found = match self
            .store
            .call(move |store| store.continuation(&lookup))
            .await
        {
            Ok(found) => found,
            Err(error) => return self.retry_after_store_failure(&id, None, &error),
        };
        let Some((thread_id, continuation)) = found else {
            return;
        };
        if continuation.state != ContinuationState::Scheduled {
            // Cancelled, sent, or back with the user since it was armed: the timer is obsolete.
            return;
        }
        match self.resume_or_wait(&thread_id, &continuation).await {
            Ok(()) => {
                self.store_failures.remove(&id);
            }
            Err(error) => self.retry_after_store_failure(&id, Some(&thread_id), &error),
        }
    }

    /// The timer is the only thing that wakes a scheduled continuation, so a failed store call
    /// must arm it again or the thread never resumes. The wait doubles with each failure in a row,
    /// up to the recheck wait.
    fn retry_after_store_failure(
        &mut self,
        id: &ContinuationId,
        thread_id: Option<&ThreadId>,
        error: &StoreError,
    ) {
        let failures = self.store_failures.entry(id.clone()).or_default();
        let wait = STORE_RETRY_BASE
            .saturating_mul(2u32.saturating_pow(*failures))
            .min(RECHECK_AFTER);
        *failures = failures.saturating_add(1);
        eprintln!(
            "cannot update continuation {id} of thread {}, trying again in {wait:?}: {error}",
            thread_id.map_or_else(|| "(not read yet)".to_owned(), ToString::to_string),
        );
        self.arm_at(id, self.clock.now() + wait);
    }

    /// Asks the provider for the account's usage, through a session of `thread_id` where the
    /// provider needs one.
    async fn refresh(&self, provider: Provider, thread_id: &ThreadId) -> Result<(), ProviderError> {
        self.providers
            .agent(provider)?
            .refresh_usage(thread_id)
            .await
    }

    async fn resume_or_wait(
        &mut self,
        thread_id: &ThreadId,
        continuation: &Continuation,
    ) -> Result<(), StoreError> {
        let provider = continuation.waits_on.provider;
        let refreshed = self.refresh(provider, thread_id).await;
        let account = self.usage.account(provider);
        let readiness = match refreshed {
            Ok(()) => may_continue(&continuation.waits_on, account.as_ref()),
            Err(_) => Readiness::Unanswered,
        };
        match readiness {
            Readiness::Ready => self.send(thread_id, continuation).await,
            Readiness::StillSpent => {
                self.wait_again(thread_id, continuation, account.as_ref())
                    .await
            }
            Readiness::Unanswered => {
                let why = format!(
                    "{} did not say whether the account may run",
                    display(provider)
                );
                self.unanswered(thread_id, continuation, &why).await
            }
            Readiness::AccountChanged => {
                self.hand_to_user(thread_id, continuation, "The signed-in account changed")
                    .await
            }
        }
    }

    async fn send(
        &mut self,
        thread_id: &ThreadId,
        continuation: &Continuation,
    ) -> Result<(), StoreError> {
        self.forget(&continuation.id);
        let sent = match self.providers.agent(continuation.waits_on.provider) {
            Ok(agent) => {
                agent
                    .continue_thread(thread_id.clone(), continuation.id.clone())
                    .await
            }
            Err(error) => Err(error),
        };
        match sent {
            Ok(Continued::Queued | Continued::NotOpen) => Ok(()),
            Ok(Continued::NeedsUser(why)) => self.hand_to_user(thread_id, continuation, &why).await,
            // Nothing was queued, so trying again later reuses this continuation's id.
            Err(_) => {
                self.arm(&continuation.id, None);
                Ok(())
            }
        }
    }

    async fn wait_again(
        &mut self,
        thread_id: &ThreadId,
        continuation: &Continuation,
        account: Option<&AccountUsage>,
    ) -> Result<(), StoreError> {
        let planned = plan(continuation.waits_on.provider, account);
        let has_reset_passed = planned
            .resets_at
            .as_ref()
            .map(Timestamp::instant)
            .is_some_and(|reset| reset <= self.clock.now());
        if has_reset_passed {
            // A timer armed at this reset would fire at once, so the read says nothing about when
            // to ask again.
            let why = format!(
                "{} still reports the limit spent after its reset time",
                display(continuation.waits_on.provider)
            );
            return self.unanswered(thread_id, continuation, &why).await;
        }
        self.forget(&continuation.id);
        let is_scheduled = planned.state == ContinuationState::Scheduled;
        let resets_at = planned.resets_at.clone();
        let is_open = self
            .update(thread_id, continuation, planned.resets_at, planned.state)
            .await?;
        if is_open && is_scheduled {
            self.arm(&continuation.id, resets_at.as_ref());
        }
        Ok(())
    }

    async fn unanswered(
        &mut self,
        thread_id: &ThreadId,
        continuation: &Continuation,
        why: &str,
    ) -> Result<(), StoreError> {
        let checks = self.unanswered.entry(continuation.id.clone()).or_default();
        *checks += 1;
        if *checks < MAX_UNANSWERED_CHECKS {
            self.arm(&continuation.id, None);
            return Ok(());
        }
        self.forget(&continuation.id);
        self.hand_to_user(thread_id, continuation, why).await
    }

    async fn hand_to_user(
        &self,
        thread_id: &ThreadId,
        continuation: &Continuation,
        why: &str,
    ) -> Result<(), StoreError> {
        let state = ContinuationState::AwaitingUser {
            why: why.to_owned(),
        };
        self.update(
            thread_id,
            continuation,
            continuation.resets_at.clone(),
            state,
        )
        .await?;
        Ok(())
    }

    /// Writes the continuation's new state. False when it is no longer open.
    async fn update(
        &self,
        thread_id: &ThreadId,
        continuation: &Continuation,
        resets_at: Option<Timestamp>,
        state: ContinuationState,
    ) -> Result<bool, StoreError> {
        let id = continuation.id.clone();
        let updated = self
            .hub
            .write(thread_id.clone(), move |write| {
                write.update_continuation(&id, resets_at, state)
            })
            .await?;
        Ok(updated.value.is_some())
    }

    fn forget(&mut self, id: &ContinuationId) {
        self.unanswered.remove(id);
    }

    async fn on_request(&self, request: Request) {
        match request {
            Request::Schedule {
                continuation_id,
                reply,
            } => {
                let _ = reply.send(self.schedule(continuation_id).await);
            }
        }
    }

    async fn schedule(&self, id: ContinuationId) -> Result<Continuation, SchedulerError> {
        let lookup = id.clone();
        let (thread_id, continuation) = self
            .store
            .call(move |store| store.continuation(&lookup))
            .await?
            .ok_or_else(|| SchedulerError::UnknownContinuation(id.clone()))?;
        match &continuation.state {
            ContinuationState::Scheduled => return Ok(continuation),
            ContinuationState::AwaitingUser { .. } => {}
            ContinuationState::Sent { .. } | ContinuationState::Cancelled { .. } => {
                return Err(SchedulerError::Conflict(
                    "The continuation was already sent or cancelled".into(),
                ));
            }
        }
        let Some(resets_at) = continuation.resets_at.clone() else {
            return Err(SchedulerError::Conflict(format!(
                "{} did not say when the limit resets, so there is no time to resume at",
                display(continuation.waits_on.provider)
            )));
        };
        let write_id = id.clone();
        let updated = self
            .hub
            .write(thread_id, move |write| {
                write.update_continuation(&write_id, Some(resets_at), ContinuationState::Scheduled)
            })
            .await?
            .value
            .ok_or_else(|| SchedulerError::Conflict("The continuation is no longer open".into()))?;
        self.arm(&updated.id, updated.resets_at.as_ref());
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(minutes: i64, used_percent: i32, resets_at: &str) -> UsageWindow {
        UsageWindow {
            used_percent,
            window_minutes: Some(minutes),
            resets_at: Some(Timestamp::try_from(resets_at.to_owned()).unwrap()),
        }
    }

    fn account(allowance: Allowance, buckets: Vec<UsageBucket>) -> AccountUsage {
        AccountUsage {
            provider: Provider::Codex,
            account: Some("acct".into()),
            plan: Some("plus".into()),
            allowance,
            buckets,
            read_at: Timestamp::now().unwrap(),
        }
    }

    fn bucket(
        limit_id: &str,
        primary: Option<UsageWindow>,
        secondary: Option<UsageWindow>,
    ) -> UsageBucket {
        UsageBucket {
            limit_id: limit_id.into(),
            name: None,
            primary,
            secondary,
            reached: None,
        }
    }

    const SOON: &str = "2030-01-01T05:00:00.000Z";
    const LATER: &str = "2030-01-07T00:00:00.000Z";

    #[test]
    fn only_a_spent_five_hour_window_with_a_reset_resumes_on_its_own() {
        // Catches D41 broken either way: a weekly stop resumed without the user, or a five-hour
        // stop left waiting on them.
        let five_hours = account(
            Allowance::Blocked,
            vec![bucket(
                "codex",
                Some(window(300, 100, SOON)),
                Some(window(10080, 40, LATER)),
            )],
        );
        let weekly = account(
            Allowance::Blocked,
            vec![bucket(
                "codex",
                Some(window(300, 20, SOON)),
                Some(window(10080, 100, LATER)),
            )],
        );
        let unknown_reset = account(
            Allowance::Blocked,
            vec![bucket(
                "codex",
                Some(UsageWindow {
                    resets_at: None,
                    ..window(300, 100, SOON)
                }),
                None,
            )],
        );

        let automatic = plan(Provider::Codex, Some(&five_hours));
        assert_eq!(automatic.state, ContinuationState::Scheduled);
        assert_eq!(
            automatic.resets_at.as_ref().map(Timestamp::as_str),
            Some(SOON)
        );
        assert_eq!(automatic.waits_on.limit_ids, ["codex"]);
        for (asks, usage) in [
            ("weekly", Some(&weekly)),
            ("unknown reset", Some(&unknown_reset)),
            ("no read", None),
        ] {
            let planned = plan(Provider::Codex, usage);
            assert!(
                matches!(planned.state, ContinuationState::AwaitingUser { .. }),
                "{asks}: {planned:?}"
            );
        }
        assert_eq!(
            plan(Provider::Codex, Some(&weekly))
                .resets_at
                .as_ref()
                .map(Timestamp::as_str),
            Some(LATER)
        );
    }

    #[test]
    fn the_latest_reset_of_the_spent_windows_wins_by_time_not_by_text() {
        // Catches resets compared as text, where the shorter form without milliseconds sorts after
        // a later instant written with them.
        let earlier = "2030-01-01T05:00:00Z";
        let later = "2030-01-01T05:00:00.500Z";
        let two_spent = account(
            Allowance::Blocked,
            vec![
                bucket("codex", Some(window(300, 100, earlier)), None),
                bucket("codex_other", Some(window(300, 100, later)), None),
            ],
        );

        let planned = plan(Provider::Codex, Some(&two_spent));
        assert_eq!(
            planned.resets_at.as_ref().map(Timestamp::as_str),
            Some(later)
        );
    }

    #[test]
    fn a_continuation_goes_only_on_an_allowed_read_with_every_waited_bucket_back() {
        // Catches permission read from a reset time or a percentage, a second bucket still spent
        // being ignored, and a read of another account counting.
        let waits_on = UsageWait {
            provider: Provider::Codex,
            account: Some("acct".into()),
            limit_ids: vec!["codex".into(), "codex_other".into()],
        };
        let back = || {
            vec![
                bucket("codex", Some(window(300, 0, LATER)), None),
                bucket("codex_other", Some(window(300, 0, LATER)), None),
            ]
        };
        let one_spent = vec![
            bucket("codex", Some(window(300, 0, LATER)), None),
            bucket("codex_other", Some(window(300, 100, LATER)), None),
        ];
        let mut other_account = account(Allowance::Allowed, back());
        other_account.account = Some("someone else".into());
        let unavailable = Allowance::Unavailable {
            reason: "no answer".into(),
        };

        assert_eq!(
            may_continue(&waits_on, Some(&account(Allowance::Allowed, back()))),
            Readiness::Ready
        );
        assert_eq!(
            may_continue(&waits_on, Some(&account(Allowance::Allowed, one_spent))),
            Readiness::StillSpent
        );
        assert_eq!(
            may_continue(&waits_on, Some(&account(Allowance::Blocked, back()))),
            Readiness::StillSpent
        );
        assert_eq!(
            may_continue(&waits_on, Some(&account(unavailable, back()))),
            Readiness::Unanswered
        );
        assert_eq!(may_continue(&waits_on, None), Readiness::Unanswered);
        assert_eq!(
            may_continue(&waits_on, Some(&other_account)),
            Readiness::AccountChanged
        );
    }
}
