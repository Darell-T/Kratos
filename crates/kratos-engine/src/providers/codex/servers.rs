//! The Codex app-servers. Each Kratos thread that needs Codex gets one of its own, so a Stop Codex
//! ignores closes only that thread's process and ends no other turn. Work that belongs to no
//! thread, such as the account's usage, uses any server that is up, or an account-only server.
//!
//! At most [`MAX_SERVERS`] run at once. Measured with Codex 0.166 on Windows, an idle app-server
//! with no thread loaded holds a 78 MB working set (46 MB private), so the limit bounds idle memory
//! near 620 MB. At the limit the least recently used idle server closes first. A server is idle
//! when its thread runs no turn and nothing waits on it, and Codex reports no background terminal.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kratos_codex_client::Codex;
use kratos_protocol::ThreadId;

use super::actor::Command;
use super::connection::Restarts;

/// The most app-servers Kratos runs at once.
pub(super) const MAX_SERVERS: usize = 8;
/// A thread's server nobody used for this long closes; the thread's next work starts another,
/// which resumes the thread's Codex session. As Claude's sessions do.
pub(super) const THREAD_IDLE_CLOSE: Duration = Duration::from_secs(30 * 60);
/// The account-only server closes this soon after its last request.
pub(super) const ACCOUNT_IDLE_CLOSE: Duration = Duration::from_secs(60);

/// Which app-server: a thread's own, or the one for work that belongs to no thread.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum ServerKey {
    Thread(ThreadId),
    Account,
}

pub(super) enum Server {
    Down,
    Starting,
    Up(Codex),
}

/// One app-server and what waits on it.
pub(super) struct Slot {
    pub server: Server,
    /// Advances on each start and exit of this server; work fenced by an older value is dropped,
    /// because the process that was to answer it is gone.
    pub generation: u64,
    pub restarts: Restarts,
    /// Commands waiting for this server to start.
    pub deferred: Vec<Command>,
    pub last_used: Instant,
    /// A check that the server holds no background terminal is out, so it is not asked twice.
    pub retiring: bool,
}

impl Slot {
    fn new() -> Slot {
        Slot {
            server: Server::Down,
            generation: 0,
            restarts: Restarts::default(),
            deferred: Vec::new(),
            last_used: Instant::now(),
            retiring: false,
        }
    }

    pub(super) fn is_live(&self) -> bool {
        !matches!(self.server, Server::Down)
    }
}

#[derive(Default)]
pub(super) struct Servers {
    slots: HashMap<ServerKey, Slot>,
    /// Generations are unique across servers, so a slot made again never reuses an old one.
    issued: u64,
}

impl Servers {
    pub(super) fn slot(&mut self, key: &ServerKey) -> &mut Slot {
        self.slots.entry(key.clone()).or_insert_with(Slot::new)
    }

    pub(super) fn get(&self, key: &ServerKey) -> Option<&Slot> {
        self.slots.get(key)
    }

    pub(super) fn generation(&self, key: &ServerKey) -> u64 {
        self.slots.get(key).map_or(0, |slot| slot.generation)
    }

    /// Moves `key` to a new generation and returns it.
    pub(super) fn advance(&mut self, key: &ServerKey) -> u64 {
        self.issued += 1;
        let generation = self.issued;
        self.slot(key).generation = generation;
        generation
    }

    pub(super) fn codex(&self, key: &ServerKey) -> Option<Codex> {
        match &self.slots.get(key)?.server {
            Server::Up(codex) => Some(codex.clone()),
            Server::Down | Server::Starting => None,
        }
    }

    /// Any server that is up, the account's first, for work that belongs to no thread.
    pub(super) fn any_up(&self) -> Option<(ServerKey, Codex)> {
        let up = |key: &ServerKey| self.codex(key).map(|codex| (key.clone(), codex));
        up(&ServerKey::Account).or_else(|| self.slots.keys().find_map(up))
    }

    pub(super) fn live(&self) -> usize {
        self.slots.values().filter(|slot| slot.is_live()).count()
    }

    pub(super) fn keys(&self) -> Vec<ServerKey> {
        self.slots.keys().cloned().collect()
    }

    /// The servers that are up and not already being retired, least recently used first.
    pub(super) fn up_by_age(&self) -> Vec<ServerKey> {
        let mut up: Vec<(&ServerKey, Instant)> = self
            .slots
            .iter()
            .filter(|(_, slot)| matches!(slot.server, Server::Up(_)) && !slot.retiring)
            .map(|(key, slot)| (key, slot.last_used))
            .collect();
        up.sort_by_key(|(_, used)| *used);
        up.into_iter().map(|(key, _)| key.clone()).collect()
    }

    /// Drops the slot of a server that is down and has nothing waiting, so the map holds only
    /// servers that matter.
    pub(super) fn forget_if_unused(&mut self, key: &ServerKey) {
        let unused = self.slots.get(key).is_some_and(|slot| {
            matches!(slot.server, Server::Down)
                && slot.deferred.is_empty()
                && !slot.restarts.waits()
        });
        if unused {
            self.slots.remove(key);
        }
    }
}
