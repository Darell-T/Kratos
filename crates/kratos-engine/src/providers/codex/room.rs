//! Which thread's app-server may start, which waits for room while [`MAX_SERVERS`] run, and
//! which idle server closes.

use std::time::{Duration, Instant};

use kratos_codex_client::{Codex, methods, types};
use serde_json::json;

use super::ProviderError;
use super::actor::{Actor, Done};
use super::connection::{connect_ready, launch};
use super::servers::{MAX_SERVERS, Server, ServerKey, TerminalCheck};
use super::threads::Turn;

impl Actor {
    /// The connection to `key`'s server, or `None` while it starts or waits for room. Starts the
    /// server when it is down, unless its last failure's backoff still runs.
    pub(super) fn connection(&mut self, key: &ServerKey) -> Result<Option<Codex>, ProviderError> {
        if let Some(codex) = self.servers.codex(key) {
            self.servers.slot(key).last_used = Instant::now();
            return Ok(Some(codex));
        }
        let slot = self.servers.slot(key);
        if slot.is_live() {
            return Ok(None);
        }
        if let Err(wait) = slot.restarts.may_start() {
            return Err(ProviderError::Unavailable(format!(
                "Codex keeps failing to start; Kratos tries again in {} s",
                wait.as_secs().max(1)
            )));
        }
        if self.servers.live() >= MAX_SERVERS {
            self.wait_for_room(key);
        } else {
            self.start_server(key);
        }
        Ok(None)
    }

    fn start_server(&mut self, key: &ServerKey) {
        self.servers.advance_generation(key);
        let slot = self.servers.slot(key);
        slot.server = Server::Starting;
        slot.last_used = Instant::now();
        let launch = launch(&self.settings);
        self.spawn(key, async move {
            let capabilities = serde_json::from_value(json!({ "experimentalApi": true }))
                .expect("the capabilities match the generated type");
            Done::Connected {
                result: connect_ready(launch, capabilities).await,
            }
        });
    }

    /// Queues `key` for a server while [`MAX_SERVERS`] run, and asks the least recently used idle
    /// one to close. When every server works, the thread's message waits, and the thread is told.
    fn wait_for_room(&mut self, key: &ServerKey) {
        self.servers.wait(key);
        self.servers.recheck_busy();
        if self.retire_one_idle() {
            return;
        }
        if let ServerKey::Thread(thread_id) = key {
            self.hub.publish(
                thread_id,
                kratos_protocol::LiveUpdate::Waiting {
                    reason: format!(
                        "Codex is running {MAX_SERVERS} threads, the most Kratos runs at once. \
                         This thread's message waits and starts when one of them finishes"
                    ),
                },
            );
        }
    }

    pub(super) fn admit_waiting(&mut self) {
        while self.servers.live() < MAX_SERVERS {
            let Some(key) = self.servers.next_waiting() else {
                return;
            };
            if !self.servers.get(&key).is_some_and(|slot| slot.is_live()) {
                self.start_server(&key);
            }
        }
        self.make_room_if_waited_for();
    }

    /// Something changed. When something waits for a server, an idle one may now close, including
    /// one found busy before.
    pub(super) fn make_room_if_waited_for(&mut self) {
        if self.servers.anyone_waits() {
            self.servers.recheck_busy();
            self.retire_one_idle();
        }
    }

    /// Asks the least recently used idle server to close. False when none is idle.
    fn retire_one_idle(&mut self) -> bool {
        let candidate = self
            .servers
            .up_by_age()
            .into_iter()
            .find(|key| self.is_idle(key) && !self.servers.waits(key));
        let Some(key) = candidate else {
            return false;
        };
        self.retire_if_quiet(&key);
        true
    }

    /// Closes each server nobody used for its idle time, once it is idle and quiet. A background
    /// terminal can end without a word from Codex, so a thread waiting for a server asks again,
    /// and a server found busy is asked again after another idle time.
    pub(super) fn retire_idle(&mut self, now: Instant) {
        self.make_room_if_waited_for();
        self.servers.recheck_busy_unused(now);
        for key in self.servers.up_by_age() {
            let unused_for = self
                .servers
                .get(&key)
                .map_or(Duration::ZERO, |slot| now.duration_since(slot.last_used));
            if unused_for >= key.idle_close() && self.is_idle(&key) {
                self.retire_if_quiet(&key);
            }
        }
    }

    /// Nothing runs or waits on the server: its thread runs no turn, sends nothing, and waits on
    /// no answer, and no command waits for the server. The account's server is idle between reads.
    fn is_idle(&self, key: &ServerKey) -> bool {
        let quiet_slot = self
            .servers
            .get(key)
            .is_some_and(|slot| slot.deferred.is_empty());
        let ServerKey::Thread(thread_id) = key else {
            return quiet_slot;
        };
        let thread_idle = self.threads.get(thread_id).is_none_or(|thread| {
            matches!(thread.turn, Turn::Idle)
                && thread.steering.is_empty()
                && thread.discards.is_empty()
        });
        quiet_slot && thread_idle && !self.requests.awaits_user(thread_id)
    }

    /// Closes an idle server once Codex reports it holds no background terminal, which closing it
    /// would end. The account's server holds none.
    fn retire_if_quiet(&mut self, key: &ServerKey) {
        let native_id = match key {
            ServerKey::Thread(thread_id) => self
                .threads
                .get(thread_id)
                .and_then(|thread| thread.native_id().map(str::to_owned)),
            ServerKey::Account => None,
        };
        let Some(codex) = self.servers.codex(key) else {
            return;
        };
        self.servers.slot(key).terminals = TerminalCheck::Asking;
        self.spawn(key, async move {
            let quiet = match native_id {
                None => Ok(true),
                Some(thread_id) => {
                    let params = types::ThreadBackgroundTerminalsListParams {
                        thread_id,
                        cursor: None,
                        limit: None,
                    };
                    codex
                        .request::<methods::ThreadBackgroundTerminalsList>(&params)
                        .await
                        .map(|listed| listed.data.is_empty())
                        .map_err(|error| error.to_string())
                }
            };
            Done::Quiet { quiet }
        });
    }

    /// The answer to [`Actor::retire_if_quiet`]'s question. A server found busy counts as used
    /// now, so the idle sweep asks again only after another idle time. A server Codex would not
    /// answer for counts as busy: closing it could end a terminal Kratos cannot see.
    pub(super) async fn on_quiet(&mut self, key: ServerKey, quiet: Result<bool, String>) {
        let quiet = quiet.unwrap_or_else(|error| {
            eprintln!("Codex did not list the background terminals of {key:?}: {error}");
            false
        });
        if quiet && self.is_idle(&key) {
            self.servers.slot(&key).terminals = TerminalCheck::NotAsked;
            self.stop_server(&key, "nobody used it, and its thread is idle")
                .await;
            return;
        }
        let slot = self.servers.slot(&key);
        slot.terminals = if quiet {
            TerminalCheck::NotAsked
        } else {
            TerminalCheck::Busy
        };
        slot.last_used = Instant::now();
        if self.servers.anyone_waits() {
            self.retire_one_idle();
        }
    }
}
