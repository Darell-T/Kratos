//! The account Codex runs under: the plan that pays for it (D24), its usage (D38), and the turns
//! that stop at its usage limit (D39).

use kratos_codex_client::{Codex, RequestError, methods, types};
use kratos_protocol::Provider;

use super::actor::{Actor, Command, Done, Reply};
use super::servers::ServerKey;
use super::{ProviderError, wire};

impl Actor {
    pub(super) fn read_account(&self, key: &ServerKey, codex: Codex) {
        let reader = codex.clone();
        let pending_request = self.servers.request_guard(key);
        self.spawn(key, async move {
            let _guard = pending_request;
            let plan = match reader
                .request::<methods::AccountRead>(&types::GetAccountParams::default())
                .await
            {
                Ok(read) => wire::plan(&read),
                Err(_) => None,
            };
            Done::Account { plan }
        });
        self.read_usage(key, codex, None);
    }

    pub(super) fn refresh_usage(&mut self, reply: Reply<()>) {
        if let Some((key, codex)) = self.servers.any_up() {
            self.servers.slot(&key).last_used = std::time::Instant::now();
            self.read_usage(&key, codex, Some(reply));
            return;
        }
        match self.connection(&ServerKey::Account) {
            Ok(Some(codex)) => self.read_usage(&ServerKey::Account, codex, Some(reply)),
            Ok(None) => self
                .servers
                .slot(&ServerKey::Account)
                .deferred
                .push(Command::RefreshUsage { reply }),
            Err(error) => drop(reply.send(Err(error))),
        }
    }

    /// Reads the account's usage; a read finished after the account changed is dropped (D38).
    pub(super) fn read_usage(&self, key: &ServerKey, codex: Codex, reply: Option<Reply<()>>) {
        let tracker_generation = self.usage.generation(Provider::Codex);
        let pending_request = self.servers.request_guard(key);
        self.spawn(key, async move {
            let _guard = pending_request;
            let result = codex
                .request::<methods::RateLimitsRead>(&None)
                .await
                .map(Box::new);
            Done::UsageRead {
                tracker_generation,
                result,
                reply,
            }
        });
    }

    pub(super) fn on_usage_read(
        &mut self,
        tracker_generation: u64,
        result: Result<Box<types::GetAccountRateLimitsResponse>, RequestError>,
        reply: Option<Reply<()>>,
    ) {
        let result = result
            .map_err(|error| format!("Codex did not report the account's usage: {error}"))
            .and_then(|read| {
                wire::account_usage(&read, self.plan.clone())
                    .map_err(|error| format!("Kratos could not timestamp the usage read: {error}"))
            });
        let answer = match result {
            Ok(usage) => {
                self.usage.record_read(tracker_generation, usage);
                Ok(())
            }
            Err(reason) => {
                self.usage
                    .record_unavailable(Provider::Codex, reason.clone());
                Err(ProviderError::Unavailable(reason))
            }
        };
        if let Some(reply) = reply {
            let _ = reply.send(answer);
        }
    }
}
