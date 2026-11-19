//! One unloaded Codex 0.166 app-server on Windows used a 78 MB working set. Loaded threads can use more.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use kratos_codex_client::Codex;
use kratos_protocol::ThreadId;

use super::actor::Command;
use super::connection::Restarts;

pub(super) const MAX_SERVERS: usize = 8;
pub(super) const THREAD_IDLE_CLOSE: Duration = Duration::from_secs(30 * 60);
pub(super) const ACCOUNT_IDLE_CLOSE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum ServerKey {
    Thread(ThreadId),
    Account,
}

impl ServerKey {
    pub(super) fn idle_close(&self) -> Duration {
        match self {
            ServerKey::Thread(_) => THREAD_IDLE_CLOSE,
            ServerKey::Account => ACCOUNT_IDLE_CLOSE,
        }
    }
}

pub(super) enum Server {
    Down,
    Starting,
    Up(Codex),
}

pub(super) struct Slot {
    pub server: Server,
    pub generation: u64,
    pub restarts: Restarts,
    pub deferred: Vec<Command>,
    pub last_used: Instant,
    pub terminals: TerminalCheck,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalCheck {
    NotAsked,
    Asking,
    Busy,
}

impl Slot {
    fn new() -> Slot {
        Slot {
            server: Server::Down,
            generation: 0,
            restarts: Restarts::default(),
            deferred: Vec::new(),
            last_used: Instant::now(),
            terminals: TerminalCheck::NotAsked,
        }
    }

    pub(super) fn is_live(&self) -> bool {
        !matches!(self.server, Server::Down)
    }
}

#[derive(Default)]
pub(super) struct Servers {
    slots: HashMap<ServerKey, Slot>,
    issued: u64,
    waiting: VecDeque<ServerKey>,
}

impl Servers {
    pub(super) fn wait(&mut self, key: &ServerKey) {
        if !self.waiting.contains(key) {
            self.waiting.push_back(key.clone());
        }
    }

    pub(super) fn next_waiting(&mut self) -> Option<ServerKey> {
        self.waiting.pop_front()
    }

    pub(super) fn anyone_waits(&self) -> bool {
        !self.waiting.is_empty()
    }

    pub(super) fn waits(&self, key: &ServerKey) -> bool {
        self.waiting.contains(key)
    }

    pub(super) fn slot(&mut self, key: &ServerKey) -> &mut Slot {
        self.slots.entry(key.clone()).or_insert_with(Slot::new)
    }

    pub(super) fn get(&self, key: &ServerKey) -> Option<&Slot> {
        self.slots.get(key)
    }

    pub(super) fn generation(&self, key: &ServerKey) -> u64 {
        self.slots.get(key).map_or(0, |slot| slot.generation)
    }

    pub(super) fn advance_generation(&mut self, key: &ServerKey) {
        self.issued += 1;
        let generation = self.issued;
        self.slot(key).generation = generation;
    }

    pub(super) fn codex(&self, key: &ServerKey) -> Option<Codex> {
        match &self.slots.get(key)?.server {
            Server::Up(codex) => Some(codex.clone()),
            Server::Down | Server::Starting => None,
        }
    }

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

    pub(super) fn recheck_busy(&mut self) {
        for slot in self.slots.values_mut() {
            if slot.terminals == TerminalCheck::Busy {
                slot.terminals = TerminalCheck::NotAsked;
            }
        }
    }

    pub(super) fn recheck_busy_unused(&mut self, now: Instant) {
        for (key, slot) in &mut self.slots {
            let unused_for = now.duration_since(slot.last_used);
            if slot.terminals == TerminalCheck::Busy && unused_for >= key.idle_close() {
                slot.terminals = TerminalCheck::NotAsked;
            }
        }
    }

    pub(super) fn up_by_age(&self) -> Vec<ServerKey> {
        let mut up: Vec<(&ServerKey, Instant)> = self
            .slots
            .iter()
            .filter(|(_, slot)| {
                matches!(slot.server, Server::Up(_)) && slot.terminals == TerminalCheck::NotAsked
            })
            .map(|(key, slot)| (key, slot.last_used))
            .collect();
        up.sort_by_key(|(_, used)| *used);
        up.into_iter().map(|(key, _)| key.clone()).collect()
    }

    pub(super) fn forget_if_unused(&mut self, key: &ServerKey) {
        let unused = self.slots.get(key).is_some_and(|slot| {
            matches!(slot.server, Server::Down)
                && slot.deferred.is_empty()
                && !slot.restarts.in_backoff()
        });
        if unused {
            self.slots.remove(key);
        }
    }
}
