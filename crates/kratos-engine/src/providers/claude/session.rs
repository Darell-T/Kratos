//! Starting a thread's CLI process and ending it. Each thread has a process of its own, started
//! when its next message needs one and never on a timer (L4, L5).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kratos_claude_client::{
    Incoming, Initialize, Launch, SessionId, SessionStart, SettingSource, connect,
};
use kratos_protocol::{Provider, ThreadId};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::actor::{Actor, Connected, Done, SessionEvent};
use super::threads::{OpenSession, Session, SessionConfig, Start, ThreadState};
use super::{ClaudeSettings, ProviderError, permissions, wire};

/// The tools Claude Code runs commands with. Kratos runs commands itself, in the sandbox of the
/// thread's permission (D17), so the CLI is left with neither.
const DISALLOWED_TOOLS: [&str; 2] = ["Bash", "PowerShell"];

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
        self.threads.insert(
            thread_id.clone(),
            ThreadState::new(stored.folder, stored.settings, session_id),
        );
        Ok(())
    }

    /// Starts the thread's CLI. Inside the wait after a failure it starts nothing: the message
    /// stays queued and a timer asks again when the wait ends. A start that cannot happen at all,
    /// such as one past the failure limit, pauses the thread and leaves its message queued.
    pub(super) async fn open_session(&mut self, thread_id: &ThreadId, config: SessionConfig) {
        let thread = self.threads.get_mut(thread_id).expect("dispatch loaded it");
        let launch = match thread
            .restarts
            .start_at(Instant::now(), self.settings.max_restarts)
        {
            Start::Now => launch(&self.settings, thread, &config),
            Start::After(wait) => {
                self.start_after(thread_id, wait);
                return;
            }
            Start::Refused(why) => Err(why),
        };
        let launch = match launch {
            Ok(launch) => launch,
            Err(why) => {
                self.pause_unavailable_if_waiting(thread_id, why).await;
                return;
            }
        };
        thread.epoch += 1;
        thread.session = Session::Opening;
        let epoch = thread.epoch;
        let thread_id = thread_id.clone();
        self.spawn(async move {
            let result = connect(launch, Initialize::default(), permissions::answer).await;
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
        result: Result<Box<Connected>, kratos_claude_client::ConnectError>,
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
                let thread = self.threads.get_mut(&thread_id).expect("checked above");
                thread.session = Session::Open(OpenSession {
                    claude,
                    config,
                    started: false,
                    worked: false,
                    plan: wire::plan(&initialized),
                });
                tokio::spawn(forward(
                    thread_id.clone(),
                    epoch,
                    incoming,
                    self.frames.clone(),
                ));
                self.dispatch(&thread_id).await;
            }
            Err(error) => {
                let thread = self.threads.get_mut(&thread_id).expect("checked above");
                thread.session = Session::Closed;
                let reason = format!("Claude could not start: {error}");
                thread
                    .restarts
                    .failed(Instant::now(), self.settings.restart_wait, reason.clone());
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
    let id = *thread
        .session_id
        .get_or_insert_with(|| SessionId::from(Uuid::new_v4()));
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
