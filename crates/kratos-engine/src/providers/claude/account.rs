//! The account Claude runs under: the plan that pays for it (D24), its usage (D38), and the turns
//! that stop at its usage limit (D39).
//!
//! Claude gives no account id, and the email is never kept (D24), so one account stands for the
//! provider and the plan its `initialize` reports is the only way to notice that it changed.
//! Usage arrives two ways. A `rate_limit_event` says what the server just applied, and a
//! `get_usage` read after each session starts, and whenever the scheduler asks, says what the
//! usage endpoint reports (see `limits`).

use kratos_claude_client::{RateLimitEvent, RateLimitStatus, RequestError, UsageRead};
use kratos_protocol::{Provider, ThreadId, Timestamp};

use super::ProviderError;
use super::actor::{Actor, Done, Reply};
use super::limits;
use super::threads::Rejection;
use crate::usage::timestamp_of_unix;

impl Actor {
    /// A session reported `plan`, and is now the account that is signed in. A plan other than the
    /// one the usage was read under means the account changed, so every snapshot and read in
    /// flight describes one that is gone, and sessions still open under the old plan no longer
    /// write usage (see [`Actor::is_signed_in`]).
    pub(super) fn note_plan(&mut self, plan: Option<&str>) {
        let known = self.usage.account(Provider::Claude);
        if known.is_some_and(|known| known.plan.as_deref() != plan) {
            self.usage.account_changed(Provider::Claude);
        }
        self.signed_in = plan.map(str::to_owned);
    }

    /// Whether a session of `plan` is of the account that signed in last.
    fn is_signed_in(&self, plan: Option<&str>) -> bool {
        self.signed_in.as_deref() == plan
    }

    /// Reads the usage through the session used last, so a report shows the account as it is
    /// now. A process is never started only to read: with none open, the read fails.
    pub(super) fn refresh_open_usage(&self, reply: Reply<()>) {
        let used_last = self
            .threads
            .iter()
            .filter_map(|(thread_id, thread)| Some((thread.open()?.last_active, thread_id)))
            .max_by_key(|(last_active, _)| *last_active)
            .map(|(_, thread_id)| thread_id.clone());
        match used_last {
            Some(thread_id) => self.read_usage(&thread_id, Some(reply)),
            None => {
                let _ = reply.send(Err(ProviderError::Unavailable(
                    "no Claude session is open to ask".into(),
                )));
            }
        }
    }

    /// Asks the thread's open session for the account's usage. A read finished after the account
    /// changed is dropped (D38).
    pub(super) fn read_usage(&self, thread_id: &ThreadId, reply: Option<Reply<()>>) {
        let open = self.threads.get(thread_id).and_then(|thread| thread.open());
        let Some(open) = open else {
            if let Some(reply) = reply {
                let _ = reply.send(Err(ProviderError::Unavailable(
                    "Claude is not running".into(),
                )));
            }
            return;
        };
        let (claude, plan) = (open.claude.clone(), open.plan.clone());
        let generation = self.usage.generation(Provider::Claude);
        self.spawn(async move {
            Done::UsageRead {
                generation,
                plan,
                result: claude.usage().await,
                reply,
            }
        });
    }

    pub(super) fn on_usage_read(
        &mut self,
        generation: u64,
        plan: Option<String>,
        result: Result<UsageRead, RequestError>,
        reply: Option<Reply<()>>,
    ) {
        if !self.is_signed_in(plan.as_deref()) {
            if let Some(reply) = reply {
                let _ = reply.send(Err(ProviderError::Unavailable(
                    "The session belongs to an account that was signed out, so its usage is not the account's now"
                        .into(),
                )));
            }
            return;
        }
        let answer = match result {
            Ok(read) => self.record_read(generation, plan, &read),
            Err(error) => Err(format!(
                "Claude did not report the account's usage: {error}"
            )),
        };
        let answer = answer.map_err(|reason| {
            self.usage
                .record_unavailable(Provider::Claude, reason.clone());
            ProviderError::Unavailable(reason)
        });
        if let Some(reply) = reply {
            let _ = reply.send(answer);
        }
    }

    fn record_read(
        &self,
        generation: u64,
        plan: Option<String>,
        read: &UsageRead,
    ) -> Result<(), String> {
        let now = self.usage.now();
        let read_at = Timestamp::at(now)
            .map_err(|error| format!("Kratos could not timestamp the usage read: {error}"))?;
        let previous = self.usage.account(Provider::Claude);
        let account = limits::account_from_read(read, plan, previous.as_ref(), now, read_at);
        self.usage.record_read(generation, account);
        Ok(())
    }

    /// Records what a `rate_limit_event` states, and remembers it for the end of the turn. The
    /// event changes nothing else: only the turn's result decides a stop (D39). A status Kratos
    /// does not know is written on the running turn, since Kratos never continues on it.
    pub(super) async fn on_rate_limit(&mut self, thread_id: &ThreadId, event: RateLimitEvent) {
        let info = event.rate_limit_info;
        if let RateLimitStatus::Unrecognized(status) = &info.status {
            let message = format!(
                "Claude reported the usage limit status {status:?}, which Kratos does not know, \
                 so Kratos will not continue this turn on its own"
            );
            self.provider_error(thread_id, message).await;
        }
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        thread.rejection = (info.status == RateLimitStatus::Rejected).then(|| Rejection {
            resets_at: info.resets_at.and_then(timestamp_of_unix),
            is_using_overage: info.is_using_overage == Some(true),
        });
        let plan = thread.open().and_then(|open| open.plan.clone());
        if !self.is_signed_in(plan.as_deref()) {
            return;
        }
        match Timestamp::at(self.usage.now()) {
            Ok(read_at) => self
                .usage
                .record_update(limits::account_from_event(&info, plan, read_at)),
            Err(error) => self
                .usage
                .record_unavailable(Provider::Claude, error.to_string()),
        }
    }
}
