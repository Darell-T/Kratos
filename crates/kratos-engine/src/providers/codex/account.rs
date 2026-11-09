//! The account Codex runs under: the plan that pays for it (D24), its usage (D38), and the turns
//! that stop at its usage limit (D39).

use kratos_codex_client::{Codex, RequestError, methods, types};
use kratos_protocol::Provider;

use super::actor::{Actor, Command, Done, Reply};
use super::{ProviderError, wire};

impl Actor {
    /// Reads the plan that pays for the account (D24) and the account's usage (D38).
    pub(super) fn read_account(&self, codex: Codex) {
        let reader = codex.clone();
        self.spawn(async move {
            let plan = match reader
                .request::<methods::AccountRead>(&types::GetAccountParams::default())
                .await
            {
                Ok(read) => wire::plan(&read),
                Err(_) => None,
            };
            Done::Account { plan }
        });
        self.read_usage(codex, None);
    }

    pub(super) fn refresh_usage(&mut self, reply: Reply<()>) {
        match self.connection() {
            Ok(Some(codex)) => self.read_usage(codex, Some(reply)),
            Ok(None) => self.deferred.push(Command::RefreshUsage { reply }),
            Err(error) => drop(reply.send(Err(error))),
        }
    }

    /// Reads the account's usage; a read finished after the account changed is dropped (D38).
    pub(super) fn read_usage(&self, codex: Codex, reply: Option<Reply<()>>) {
        let tracker_generation = self.usage.generation(Provider::Codex);
        self.spawn(async move {
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
