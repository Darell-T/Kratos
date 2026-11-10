//! What the actor knows about one Claude thread between store reads: its CLI process, the messages
//! written to it, and the turn they make.

use std::time::{Duration, Instant};

use kratos_claude_client::{Claude, PermissionMode, SessionId};
use kratos_protocol::{MessageId, SlashCommand, ThreadSettings, Timestamp, TurnId};
use tokio::task::AbortHandle;

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
    /// Default and plan modes preserve the permission callbacks Kratos decides (D21).
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

/// The server refused a request, as the latest `rate_limit_event` says. Only the turn's result
/// decides whether the turn stopped at the usage limit (D39), so the refusal is kept until then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Rejection {
    pub resets_at: Option<Timestamp>,
    /// Extra usage pays for the requests, so the refusal of the included quota stops nothing.
    pub is_using_overage: bool,
}

/// A caller waiting for the thread's CLI to be up.
pub(super) enum Waiter {
    /// Reads the account's usage through the session.
    Usage(Reply<()>),
    /// Lists the slash commands the session's `initialize` reported.
    Commands(Reply<Vec<SlashCommand>>),
}

impl Waiter {
    /// Answers the waiter that the CLI cannot be had, with `reason`.
    pub(super) fn fail(self, reason: &str) {
        self.reject(super::ProviderError::Unavailable(reason.to_owned()));
    }

    pub(super) fn reject(self, error: super::ProviderError) {
        match self {
            Waiter::Usage(reply) => drop(reply.send(Err(error))),
            Waiter::Commands(reply) => drop(reply.send(Err(error))),
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
    /// The slash commands `initialize` listed, which the CLI fixes for the life of the process.
    pub commands: Vec<kratos_claude_client::SlashCommand>,
    /// When the process last sent a frame or served a request.
    pub last_active: Instant,
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
    last_error: String,
    /// The timer that starts the CLI when the wait ends. At most one runs.
    timer: Option<AbortHandle>,
}

/// Whether a new CLI may start now.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Start {
    Now,
    /// The last failure's wait has not ended.
    After(Duration),
    /// The failures reached the limit, so only the user may start the CLI again. Holds why.
    Refused(String),
}

impl Restarts {
    pub(super) fn start_at(&self, now: Instant, max_restarts: u32) -> Start {
        if self.failures_in_a_row >= max_restarts {
            return Start::Refused(format!(
                "Claude failed {} times in a row; the last failure was: {}. Fix what stops it, then resume the thread",
                self.failures_in_a_row, self.last_error
            ));
        }
        match self.not_before {
            Some(not_before) if now < not_before => Start::After(not_before - now),
            _ => Start::Now,
        }
    }

    /// Each failure in a row doubles the wait, up to a minute.
    pub(super) fn failed(&mut self, now: Instant, first_wait: Duration, error: String) {
        let wait = first_wait
            .saturating_mul(1 << self.failures_in_a_row.min(6))
            .min(LONGEST_RESTART_WAIT);
        self.failures_in_a_row += 1;
        self.not_before = Some(now + wait);
        self.last_error = error;
    }

    /// Whether a timer will start the CLI when the wait ends.
    pub(super) fn is_waiting(&self) -> bool {
        self.timer.is_some()
    }

    pub(super) fn wait_with(&mut self, timer: AbortHandle) {
        self.cancel_timer();
        self.timer = Some(timer);
    }

    /// Drops the pending start. A timer that already fired is harmless: it only asks the thread to
    /// look for work again.
    pub(super) fn cancel_timer(&mut self) {
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
    }

    /// A turn finished, or the user resumed the thread.
    pub(super) fn reset(&mut self) {
        self.cancel_timer();
        *self = Restarts::default();
    }
}

pub(super) struct ThreadState {
    pub commands: std::collections::HashMap<String, AbortHandle>,
    pub turn_settings: Option<ThreadSettings>,
    pub changes: crate::providers::changes::ChangesRecorder,
    pub command_grants: Vec<super::tools::CommandArgs>,
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
    /// Why Kratos is closing the CLI, when it chose to, in place of the exit's own text.
    pub close_reason: Option<String>,
    pub restarts: Restarts,
    /// The refusal the latest rate limit event reports, or nothing when that event allowed. Read by
    /// the turn's result.
    pub rejection: Option<Rejection>,
    /// The model that wrote the last reply, which names the window of the turn's context (D76).
    pub answering_model: Option<String>,
    /// Callers that wait for the CLI to be up, answered when it is or when it cannot start.
    pub waiters: Vec<Waiter>,
}

impl ThreadState {
    pub(super) fn new(
        folder: String,
        settings: ThreadSettings,
        stored: Option<SessionId>,
    ) -> ThreadState {
        ThreadState {
            commands: std::collections::HashMap::new(),
            turn_settings: None,
            changes: crate::providers::changes::ChangesRecorder::default(),
            command_grants: Vec::new(),
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
            close_reason: None,
            restarts: Restarts::default(),
            rejection: None,
            answering_model: None,
            waiters: Vec::new(),
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        !self.pending.is_empty() || matches!(self.session, Session::Opening)
    }

    /// When the open process last did anything, if it has nothing to do: no message in flight,
    /// no stop, and no command running.
    pub(super) fn idle_since(&self) -> Option<Instant> {
        let open = self.open()?;
        let has_work = !self.pending.is_empty() || self.stopping || !self.commands.is_empty();
        (!has_work).then_some(open.last_active)
    }

    pub(super) fn turn_settings(&self) -> &ThreadSettings {
        self.turn_settings.as_ref().unwrap_or(&self.settings)
    }

    pub(super) fn cancel_commands(&mut self, control_id: Option<&str>) {
        match control_id {
            Some(id) => {
                if let Some(command) = self.commands.remove(id) {
                    command.abort();
                }
            }
            None => {
                for (_, command) in self.commands.drain() {
                    command.abort();
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST_WAIT: Duration = Duration::from_secs(1);

    fn failed_times(restarts: &mut Restarts, start: Instant, times: u32) {
        let mut now = start;
        for number in 1..=times {
            restarts.failed(now, FIRST_WAIT, format!("failure {number}"));
            now += Duration::from_secs(1000);
        }
    }

    #[test]
    fn each_failure_in_a_row_doubles_the_wait_before_the_next_start() {
        // Catches a wait that stays the same after a second death, which would restart a dying
        // CLI at a steady rate instead of backing off (L4).
        let start = Instant::now();
        let mut restarts = Restarts::default();

        assert_eq!(restarts.start_at(start, 5), Start::Now);
        restarts.failed(start, FIRST_WAIT, "one".into());
        assert_eq!(restarts.start_at(start, 5), Start::After(FIRST_WAIT));
        let later = start + FIRST_WAIT;
        assert_eq!(restarts.start_at(later, 5), Start::Now, "the wait ended");
        restarts.failed(later, FIRST_WAIT, "two".into());
        assert_eq!(restarts.start_at(later, 5), Start::After(FIRST_WAIT * 2));
        let again = later + FIRST_WAIT * 2;
        restarts.failed(again, FIRST_WAIT, "three".into());
        assert_eq!(restarts.start_at(again, 5), Start::After(FIRST_WAIT * 4));
    }

    #[test]
    fn the_wait_stops_growing_at_a_minute() {
        // Catches a wait that doubles without end, which would hold a thread for hours.
        let start = Instant::now();
        let mut restarts = Restarts::default();
        failed_times(&mut restarts, start, 4);
        let now = start + Duration::from_secs(10_000);

        restarts.failed(now, Duration::from_secs(40), "slow".into());

        assert_eq!(
            restarts.start_at(now, 99),
            Start::After(LONGEST_RESTART_WAIT)
        );
    }

    #[test]
    fn the_failure_that_reaches_the_limit_refuses_with_the_last_error_until_a_reset() {
        // Catches a stop that hides what the CLI last failed with (L8), and a reset that leaves the
        // count behind, so a user who fixed the problem still cannot start it.
        let start = Instant::now();
        let mut restarts = Restarts::default();
        failed_times(&mut restarts, start, 3);

        let refused = restarts.start_at(start + Duration::from_secs(10_000), 3);
        restarts.reset();

        assert!(
            matches!(&refused, Start::Refused(why)
                if why.contains("3 times in a row") && why.contains("failure 3")),
            "{refused:?}"
        );
        assert_eq!(restarts.start_at(start, 3), Start::Now);
    }
}
