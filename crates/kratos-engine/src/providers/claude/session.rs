//! Starting a thread's CLI process and ending it. Each thread has a process of its own, started
//! when its next message needs one (L4, L5), or when the usage scheduler or the skills route needs
//! a session to ask. A CLI that failed to start is tried again by a message, by the user, or by
//! the next usage read or skills request, each of which asks for a session.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kratos_claude_client::{Incoming, Launch, SessionId, SessionStart, SettingSource, connect};
use kratos_protocol::{Provider, ThreadId};
use tokio::sync::mpsc;

use super::actor::{Actor, Connected, Done, SessionEvent};
use super::threads::{OpenSession, Session, SessionConfig, Start, ThreadState, Waiter};
use super::{ClaudeSettings, ProviderError, hooks, permissions, release, wire};

/// The tools Claude Code runs commands with. Kratos runs commands itself, in the sandbox of the
/// thread's permission (D17), so the CLI cannot use its native command tools.
const DISALLOWED_TOOLS: [&str; 3] = ["Bash", "PowerShell", "Monitor"];

impl Actor {
    /// The thread's state, read from the store the first time the actor sees the thread.
    pub(super) async fn load(&mut self, thread_id: &ThreadId) -> Result<(), ProviderError> {
        if self.threads.contains_key(thread_id) {
            return Ok(());
        }
        let stored = self
            .store
            .call({
                let thread_id = thread_id.clone();
                move |store| store.provider_thread(&thread_id)
            })
            .await
            .map_err(|error| match error {
                kratos_store::StoreError::UnknownThread(id) => ProviderError::UnknownThread(id),
                other => ProviderError::Store(other),
            })?;
        if stored.thread.provider != Provider::Claude {
            return Err(ProviderError::Conflict(format!(
                "Thread {thread_id} is not a Claude thread"
            )));
        }
        let session_id = stored
            .native_id
            .as_deref()
            .map(str::parse::<SessionId>)
            .transpose()
            .map_err(|error| {
                ProviderError::Unavailable(format!("The thread's saved Claude session: {error}"))
            })?;
        let (session_id, recorded) = match session_id {
            Some(stored) => (stored, true),
            None => (wire::first_session(thread_id), false),
        };
        self.threads.insert(
            thread_id.clone(),
            ThreadState::new(stored.folder, stored.settings, session_id, recorded),
        );
        Ok(())
    }

    /// Starts the thread's CLI. Inside the wait after a failure it starts nothing: the message
    /// stays queued and a timer asks again when the wait ends. A start that cannot happen at all,
    /// such as one past the failure limit, pauses the thread and leaves its message queued. The
    /// error says why nothing started.
    pub(super) async fn open_session(
        &mut self,
        thread_id: &ThreadId,
        config: SessionConfig,
    ) -> Result<(), String> {
        let thread = self.threads.get_mut(thread_id).expect("dispatch loaded it");
        let launch = match thread
            .restarts
            .start_at(Instant::now(), self.settings.max_restarts)
        {
            Start::Now => launch(&self.settings, thread, &config),
            Start::After(wait) => {
                self.start_after(thread_id, wait);
                return Err(format!(
                    "Claude failed to start a moment ago and may start again in {wait:?}"
                ));
            }
            Start::Refused(why) => Err(why),
        };
        let launch = match launch {
            Ok(launch) => launch,
            Err(why) => {
                self.pause_unavailable_if_waiting(thread_id, why.clone())
                    .await;
                return Err(why);
            }
        };
        thread.epoch += 1;
        thread.session = Session::Opening;
        let epoch = thread.epoch;
        let thread_id = thread_id.clone();
        let enabled = self.settings.tools.is_some();
        let report_format = config.report_format.clone();
        let program = self.settings.program.clone();
        let env_set = self.settings.env_set.clone();
        self.spawn(async move {
            let result = match release::supported(&program, &env_set).await {
                Err(why) => Err(why),
                Ok(()) => connect(
                    launch,
                    hooks::initialize(enabled, report_format),
                    move |control| permissions::startup(control, enabled),
                )
                .await
                .map_err(|error| error.to_string()),
            };
            Done::Opened {
                thread_id,
                epoch,
                config,
                result: result.map(|(claude, incoming, initialized)| {
                    Box::new(Connected {
                        claude,
                        incoming,
                        initialized,
                    })
                }),
            }
        });
        Ok(())
    }

    /// Runs `waiter` once the thread's CLI is up, starting one when none runs. A CLI that cannot
    /// start answers the waiter with why.
    pub(super) async fn when_open(&mut self, thread_id: &ThreadId, waiter: Waiter) {
        let ready = match self.running() {
            Ok(()) => self.load(thread_id).await,
            Err(error) => Err(error),
        };
        if let Err(error) = ready {
            waiter.reject(error);
            return;
        }
        let thread = self
            .threads
            .get_mut(thread_id)
            .expect("when_open loaded the thread above");
        match thread.session {
            Session::Open(ref mut open) => {
                open.last_active = Instant::now();
                self.serve(thread_id, waiter);
            }
            Session::Opening => thread.waiters.push(waiter),
            Session::Closed => {
                thread.waiters.push(waiter);
                let config = SessionConfig::of(&thread.settings);
                if let Err(why) = self.open_session(thread_id, config).await {
                    self.fail_waiters(thread_id, &why);
                }
            }
        }
    }

    fn serve(&self, thread_id: &ThreadId, waiter: Waiter) {
        match waiter {
            Waiter::Usage(reply) => self.read_usage(thread_id, Some(reply)),
            Waiter::Commands(reply) => {
                let open = self.threads.get(thread_id).and_then(|thread| thread.open());
                let _ = reply.send(match open {
                    Some(open) => Ok(wire::slash_commands(&open.commands)),
                    None => Err(ProviderError::Unavailable("Claude is not running".into())),
                });
            }
        }
    }

    fn fail_waiters(&mut self, thread_id: &ThreadId, why: &str) {
        if let Some(thread) = self.threads.get_mut(thread_id) {
            for waiter in std::mem::take(&mut thread.waiters) {
                waiter.fail(why);
            }
        }
    }

    /// Asks the thread to look for work again once `wait` has passed. One timer serves the thread,
    /// however many sends and retries ask in the meantime.
    fn start_after(&mut self, thread_id: &ThreadId, wait: Duration) {
        let thread = self.threads.get_mut(thread_id).expect("dispatch loaded it");
        if thread.restarts.is_waiting() {
            return;
        }
        let (done, thread_id) = (self.done.clone(), thread_id.clone());
        let timer = tokio::spawn({
            let thread_id = thread_id.clone();
            async move {
                tokio::time::sleep(wait).await;
                // The actor holds the receiver for as long as it runs.
                let _ = done.send(Done::RestartDue { thread_id });
            }
        });
        thread.restarts.wait_with(timer.abort_handle());
    }

    /// The wait after a failure ended. The thread starts its CLI if a message still waits.
    pub(super) async fn on_restart_due(&mut self, thread_id: ThreadId) {
        if let Some(thread) = self.threads.get_mut(&thread_id) {
            thread.restarts.cancel_timer();
            self.dispatch(&thread_id).await;
        }
    }

    pub(super) async fn on_opened(
        &mut self,
        thread_id: ThreadId,
        epoch: u64,
        config: SessionConfig,
        result: Result<Box<Connected>, String>,
    ) {
        let waiting = self.threads.get(&thread_id).is_some_and(|thread| {
            thread.epoch == epoch && matches!(thread.session, Session::Opening)
        });
        if !waiting {
            if let Ok(connected) = result {
                tokio::spawn(async move { connected.claude.close().await });
            }
            return;
        }
        match result {
            Ok(connected) => {
                let Connected {
                    claude,
                    incoming,
                    initialized,
                } = *connected;
                let plan = wire::plan(&initialized);
                let commands = initialized.commands;
                self.note_plan(plan.as_deref());
                let thread = self.threads.get_mut(&thread_id).expect("checked above");
                thread.session = Session::Open(OpenSession {
                    claude,
                    config,
                    started: false,
                    worked: false,
                    plan,
                    commands,
                    last_active: Instant::now(),
                });
                tokio::spawn(forward(
                    thread_id.clone(),
                    epoch,
                    incoming,
                    self.frames.clone(),
                ));
                let waiters = std::mem::take(&mut thread.waiters);
                // A new process has seen no rate limit event, so one left by an earlier process
                // must not decide how this process's first turn ended.
                thread.rejection = None;
                self.read_usage(&thread_id, None);
                for waiter in waiters {
                    self.serve(&thread_id, waiter);
                }
                self.dispatch(&thread_id).await;
            }
            Err(error) => {
                let thread = self.threads.get_mut(&thread_id).expect("checked above");
                thread.session = Session::Closed;
                let reason = format!("Claude could not start: {error}");
                thread
                    .restarts
                    .failed(Instant::now(), self.settings.restart_wait, reason.clone());
                self.fail_waiters(&thread_id, &reason);
                self.pause_unavailable_if_waiting(&thread_id, reason).await;
            }
        }
        self.settle_idle_stops(&thread_id);
    }

    /// Ends the thread's CLI while it is idle, so the next message starts one with the settings
    /// then in force. Its exit is not heard.
    pub(super) fn retire_session(&mut self, thread_id: &ThreadId) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        thread.epoch += 1;
        if let Session::Open(open) = std::mem::replace(&mut thread.session, Session::Closed) {
            tokio::spawn(async move { open.claude.close().await });
        }
    }

    /// Closes every session with no turn running that nobody has used for `idle_for`. Its thread
    /// loses nothing: the next message resumes the session, as Zeron does for a warm session
    /// nobody comes back to.
    pub(super) fn close_sessions_idle_for(&mut self, idle_for: Duration) {
        let now = Instant::now();
        let idle: Vec<ThreadId> = self
            .threads
            .iter()
            .filter(|(_, thread)| {
                thread
                    .idle_since()
                    .is_some_and(|since| now.duration_since(since) >= idle_for)
            })
            .map(|(thread_id, _)| thread_id.clone())
            .collect();
        for thread_id in idle {
            self.retire_session(&thread_id);
        }
    }

    /// Ends the thread's CLI because Kratos chose to. Its exit arrives as usual, and `reason`
    /// stands in for the exit's own text.
    pub(super) fn close_session(&mut self, thread_id: &ThreadId, reason: &str) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        let Some(open) = thread.open() else {
            return;
        };
        eprintln!("closing Claude for thread {thread_id}: {reason}");
        let claude = open.claude.clone();
        thread.close_reason = Some(reason.to_owned());
        tokio::spawn(async move { claude.close().await });
    }
}

/// The launch of `thread`'s CLI. Refuses a thread whose saved session has no transcript left to
/// resume, because starting a new session in its place would lose the thread's history without
/// telling the user (L7).
fn launch(
    settings: &ClaudeSettings,
    thread: &mut ThreadState,
    config: &SessionConfig,
) -> Result<Launch, String> {
    let id = thread.session_id;
    let start = SessionStart::for_session(&settings.claude_home, Path::new(&thread.folder), id)
        .map_err(|error| format!("Kratos cannot check Claude's saved session: {error}"))?;
    if thread.recorded && matches!(start, SessionStart::New(_)) {
        return Err(format!(
            "Claude cannot resume this thread: its saved session {id} is gone from Claude's own history. Kratos does not start a new session in its place"
        ));
    }
    let mut launch = Launch::new(
        settings.program.clone(),
        PathBuf::from(&thread.folder),
        start,
    );
    launch.model = config.model.clone();
    launch.permission_mode = config.mode;
    // The user's own settings, never the folder's: a repository cannot change what the agent may do (D21).
    launch.setting_sources = vec![SettingSource::User];
    launch.disallowed_tools = DISALLOWED_TOOLS.map(str::to_owned).to_vec();
    launch.env_set = settings.env_set.clone();
    launch.control_timeout = settings.control_timeout;
    Ok(launch)
}

/// Hands the actor each frame the CLI writes, tagged with its session.
async fn forward(
    thread_id: ThreadId,
    epoch: u64,
    mut incoming: mpsc::Receiver<Incoming>,
    frames: mpsc::Sender<SessionEvent>,
) {
    while let Some(incoming) = incoming.recv().await {
        let event = SessionEvent {
            thread_id: thread_id.clone(),
            epoch,
            incoming,
        };
        if frames.send(event).await.is_err() {
            return;
        }
    }
}
