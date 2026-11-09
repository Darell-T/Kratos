//! The account Claude runs under: the plan that pays for it (D24), its usage (D38), and the turns
//! that stop at its usage limit (D39).
//!
//! Claude gives no account id, and the email is never kept (D24), so one account stands for the
//! provider and the plan its `initialize` reports is the only way to notice that it changed.
//! Usage arrives two ways. A `rate_limit_event` says what the server just applied, and a
//! `get_usage` read after each session starts, and whenever the scheduler asks, says what the
//! usage endpoint reports (see `limits`).

use kratos_claude_client::{RateLimitEvent, RateLimitStatus, RequestError, UsageRead};
use kratos_protocol::{Provider, SlashCommand, ThreadId, Timestamp};

use super::ProviderError;
use super::actor::{Actor, Done, Reply};
use super::limits;
use super::threads::{SeenLimit, Waiter};
use crate::usage::LimitStop;

impl Actor {
    /// A session reported `plan`. A plan other than the one the usage was read under means the
    /// signed-in account changed, so every snapshot and read in flight describes one that is gone.
    pub(super) fn note_plan(&self, plan: &Option<String>) {
        let known = self.usage.account(Provider::Claude);
        if known.is_some_and(|known| known.plan != *plan) {
            self.usage.account_changed(Provider::Claude);
        }
    }

    /// Reads the account's usage through a session of the thread, starting one when none runs.
    pub(super) async fn refresh_usage(&mut self, thread_id: ThreadId, reply: Reply<()>) {
        let ready = match self.running() {
            Ok(()) => self.load(&thread_id).await,
            Err(error) => Err(error),
        };
        if let Err(error) = ready {
            let _ = reply.send(Err(error));
            return;
        }
        self.when_open(&thread_id, Waiter::Usage(reply)).await;
    }

    /// Lists the slash commands the thread's session reports, starting one when none runs.
    pub(super) async fn slash_commands(
        &mut self,
        thread_id: ThreadId,
        reply: Reply<Vec<SlashCommand>>,
    ) {
        let ready = match self.running() {
            Ok(()) => self.load(&thread_id).await,
            Err(error) => Err(error),
        };
        match ready {
            Ok(()) => self.when_open(&thread_id, Waiter::Commands(reply)).await,
            Err(error) => drop(reply.send(Err(error))),
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

    /// Records what a `rate_limit_event` states, and remembers it for the end of the turn. The event
    /// changes nothing else: only the turn's result decides a stop (D39).
    pub(super) fn on_rate_limit(&mut self, thread_id: &ThreadId, event: RateLimitEvent) {
        let info = event.rate_limit_info;
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        thread.rate_limit = Some(SeenLimit {
            rejected: info.status == RateLimitStatus::Rejected,
            resets_at: info.resets_at.and_then(limits::timestamp_of_unix),
        });
        let plan = thread.open().and_then(|open| open.plan.clone());
        match Timestamp::at(self.usage.now()) {
            Ok(read_at) => self
                .usage
                .record_update(limits::account_from_event(&info, plan, read_at)),
            Err(error) => self
                .usage
                .record_unavailable(Provider::Claude, error.to_string()),
        }
    }

    /// Reports a usage-limit stop to the scheduler, which plans the continuation (D39).
    pub(super) fn report_limit_stop(&self, thread_id: &ThreadId, turn_id: kratos_protocol::TurnId) {
        let _ = self.limit_stops.send(LimitStop {
            thread_id: thread_id.clone(),
            turn_id,
            provider: Provider::Claude,
        });
    }
}
