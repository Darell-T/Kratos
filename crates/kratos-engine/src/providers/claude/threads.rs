//! What the actor knows about one Claude thread between store reads: its CLI process, the messages
//! written to it, and the turn they make.

use std::time::{Duration, Instant};

use kratos_claude_client::{Claude, PermissionMode, SessionId};
use kratos_protocol::{MessageId, ThreadSettings, TurnId};

use super::ClaudeSettings;
use super::actor::Reply;

/// The longest wait before a CLI that keeps dying is started again.
const LONGEST_RESTART_WAIT: Duration = Duration::from_secs(60);

/// What a CLI process is launched with. A thread whose next message needs another launch gets a
/// new process, because Claude applies these only when it starts (D87).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SessionConfig {
    pub model: Option<String>,
    pub mode: PermissionMode,
}

impl SessionConfig {
    /// Until Kratos's own permission rules exist, a thread launches in a mode that asks for every
    /// tool use or only plans, and the actor refuses each ask. `FullAccess` and `ApproveForMe` do
    /// not launch the modes that skip the ask, because nothing would then stand between Claude and
    /// a write anywhere.
    pub(super) fn of(settings: &ThreadSettings) -> SessionConfig {
        SessionConfig {
            model: settings
                .model
                .as_ref()
                .map(|model| model.as_str().to_owned()),
            mode: if settings.plan_mode {
                PermissionMode::Plan
            } else {
                PermissionMode::Default
            },
        }
    }
}

pub(super) struct OpenSession {
    pub claude: Claude,
    pub config: SessionConfig,
    /// Claude has begun a turn in this process, so it has taken a message.
    pub started: bool,
    /// Claude finished a turn in this process, so it works.
    pub worked: bool,
    pub plan: Option<String>,
}

pub(super) enum Session {
    Closed,
    Opening,
    Open(OpenSession),
}

/// A message written to the CLI that no result has reported yet. The thread is busy while any
/// remain (D11).
pub(super) struct InFlight {
    pub message_id: MessageId,
    pub native: String,
}

/// Failures of the CLI to run, which delay and finally refuse starting it again (L4).
#[derive(Default)]
pub(super) struct Restarts {
    failures_in_a_row: u32,
    not_before: Option<Instant>,
}

impl Restarts {
    /// `Err` with why a new process may not start now: the last failure's backoff still runs, or
    /// the failures reached the limit and only the user may start it again.
    pub(super) fn may_start(&self, settings: &ClaudeSettings) -> Result<(), String> {
        if self.failures_in_a_row >= settings.max_restarts {
            return Err(format!(
                "Claude failed {} times in a row. Fix what stops it, then resume the thread",
                self.failures_in_a_row
            ));
        }
        match self.not_before {
            Some(not_before) if Instant::now() < not_before => Err(format!(
                "Claude keeps failing to start; Kratos tries again in {} s",
                (not_before - Instant::now()).as_secs().max(1)
            )),
            _ => Ok(()),
        }
    }

    /// Each failure in a row doubles the wait, up to a minute.
    pub(super) fn failed(&mut self, first_wait: Duration) {
        let wait = first_wait
            .saturating_mul(1 << self.failures_in_a_row.min(6))
            .min(LONGEST_RESTART_WAIT);
        self.failures_in_a_row += 1;
        self.not_before = Some(Instant::now() + wait);
    }

    /// A turn finished, or the user resumed the thread.
    pub(super) fn reset(&mut self) {
        *self = Restarts::default();
    }
}

pub(super) struct ThreadState {
    pub folder: String,
    pub settings: ThreadSettings,
    /// The session Claude keeps the thread's history in: the stored one, or the one chosen for a
    /// thread that has none yet.
    pub session_id: Option<SessionId>,
    /// The store holds `session_id`, so Claude has a transcript to resume from.
    pub recorded: bool,
    pub session: Session,
    /// Advances whenever the session ends or another starts, so work for an earlier one is dropped.
    pub epoch: u64,
    pub pending: Vec<InFlight>,
    /// The turn the log shows running. It ends at a result, and messages still in flight after it
    /// start the next turn when Claude takes them.
    pub run: Option<TurnId>,
    pub stopping: bool,
    /// Stops asked for, answered when Claude acknowledges the interrupt or the stopped turn ends.
    pub stop_waiters: Vec<Reply<()>>,
    /// Changes whenever a stop starts or ends, so an earlier stop's timer or answer is ignored.
    pub stop_epoch: u64,
    /// Changes whenever the turn's deadline is set again, so an earlier deadline is ignored.
    pub deadline_epoch: u64,
    /// Why Kratos is closing the CLI, when it chose to, in place of the exit's own text.
    pub close_reason: Option<String>,
    pub restarts: Restarts,
}

impl ThreadState {
    pub(super) fn new(
        folder: String,
        settings: ThreadSettings,
        stored: Option<SessionId>,
    ) -> ThreadState {
        ThreadState {
            folder,
            settings,
            recorded: stored.is_some(),
            session_id: stored,
            session: Session::Closed,
            epoch: 0,
            pending: Vec::new(),
            run: None,
            stopping: false,
            stop_waiters: Vec::new(),
            stop_epoch: 0,
            deadline_epoch: 0,
            close_reason: None,
            restarts: Restarts::default(),
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        !self.pending.is_empty() || matches!(self.session, Session::Opening)
    }

    pub(super) fn open(&self) -> Option<&OpenSession> {
        match &self.session {
            Session::Open(open) => Some(open),
            Session::Closed | Session::Opening => None,
        }
    }

    pub(super) fn open_mut(&mut self) -> Option<&mut OpenSession> {
        match &mut self.session {
            Session::Open(open) => Some(open),
            Session::Closed | Session::Opening => None,
        }
    }
}
