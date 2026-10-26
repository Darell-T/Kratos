//! Starting the app-server when a thread needs it, loading threads into it, and what its exit
//! ends.
//!
//! Codex starts on demand, never on a timer (L4, L5). Each failure in a row doubles the wait
//! before the next start, up to a minute, so a Codex that keeps dying costs one start a minute at
//! most. A thread Codex cannot resume stays unavailable and says why; Kratos never starts a fresh
//! Codex thread in its place (L7).

use std::ffi::OsString;
use std::time::{Duration, Instant};

use kratos_codex_client::{
    Codex, ConnectError, Exit, Incoming, Launch, RequestError, connect, methods, types,
};
use kratos_protocol::{MessageStatus, PauseReason, Provider, ThreadId, TurnId, TurnOutcome};
use kratos_store::{StoreError, ThreadWrite};
use serde_json::json;
use tokio::sync::mpsc;

use super::actor::{Actor, Done, Opened, Server, settle_closed};
use super::profiles::Profile;
use super::threads::{Link, ThreadState, Turn};
use super::wire;
use super::{CodexSettings, ProviderError};

const FIRST_RETRY_WAIT: Duration = Duration::from_secs(1);
const LONGEST_RETRY_WAIT: Duration = Duration::from_secs(60);
/// An exit sooner than this after the start counts as a failed start.
const SETTLED_AFTER: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct Restarts {
    failures_in_a_row: u32,
    not_before: Option<Instant>,
    started_at: Option<Instant>,
}

impl Restarts {
    /// `Err` with the wait left while the last failure's backoff runs.
    fn may_start(&self) -> Result<(), Duration> {
        match self.not_before {
            Some(not_before) if Instant::now() < not_before => Err(not_before - Instant::now()),
            _ => Ok(()),
        }
    }

    fn started(&mut self) {
        self.started_at = Some(Instant::now());
    }

    /// A failed start, or an exit soon after one.
    fn failed(&mut self) {
        let wait = FIRST_RETRY_WAIT
            .saturating_mul(1 << self.failures_in_a_row.min(6))
            .min(LONGEST_RETRY_WAIT);
        self.failures_in_a_row += 1;
        self.not_before = Some(Instant::now() + wait);
    }

    fn exited(&mut self) {
        let settled = self
            .started_at
            .is_some_and(|started| started.elapsed() >= SETTLED_AFTER);
        if settled {
            self.failures_in_a_row = 0;
            self.not_before = None;
        } else {
            self.failed();
        }
    }
}

fn launch(settings: &CodexSettings) -> Launch {
    let mut args: Vec<OsString> = vec!["app-server".into()];
    args.extend(settings.extra_args.iter().cloned());
    args.extend(settings.protected.launch_arguments(&std::env::temp_dir()));
    Launch {
        program: settings.program.clone(),
        args,
        cwd: settings.server_folder.clone(),
        // Codex and the tools it runs get none of Kratos's own settings (L8).
        env_remove: std::env::vars_os()
            .map(|(name, _)| name)
            .filter(|name| name.to_string_lossy().starts_with("KRATOS_"))
            .collect(),
        env_set: settings.env_set.clone(),
        client_info: types::ClientInfo {
            name: "kratos".to_owned(),
            title: Some("Kratos".to_owned()),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        request_timeout: settings.request_timeout,
        close_timeout: CLOSE_TIMEOUT,
    }
}

impl Actor {
    /// The running connection, or `None` while it starts. Starts Codex when it is down, unless the
    /// last failure's backoff still runs.
    pub(super) fn connection(&mut self) -> Result<Option<Codex>, ProviderError> {
        match &self.server {
            Server::Up { codex, .. } => return Ok(Some(codex.clone())),
            Server::Starting => return Ok(None),
            Server::Down => {}
        }
        if let Err(wait) = self.restarts.may_start() {
            return Err(ProviderError::Unavailable(format!(
                "Codex keeps failing to start; Kratos tries again in {} s",
                wait.as_secs().max(1)
            )));
        }
        self.generation += 1;
        self.server = Server::Starting;
        let generation = self.generation;
        let launch = launch(&self.settings);
        self.spawn(async move {
            let capabilities = serde_json::from_value(json!({ "experimentalApi": true }))
                .expect("the capabilities match the generated type");
            Done::Connected {
                generation,
                result: connect(launch, capabilities).await,
            }
        });
        Ok(None)
    }

    pub(super) async fn on_connected(
        &mut self,
        generation: u64,
        result: Result<(Codex, mpsc::Receiver<Incoming>), ConnectError>,
    ) {
        if generation != self.generation {
            if let Ok((codex, _)) = result {
                codex.close().await;
            }
            return;
        }
        match result {
            Ok((codex, incoming)) => {
                self.restarts.started();
                self.server = Server::Up {
                    codex: codex.clone(),
                    incoming,
                };
                self.read_account(generation, codex);
                for command in std::mem::take(&mut self.deferred) {
                    self.on_command(command).await;
                }
                self.reconcile_waiting().await;
                let threads: Vec<ThreadId> = self.threads.keys().cloned().collect();
                for thread_id in threads {
                    self.dispatch(&thread_id).await;
                }
            }
            Err(error) => {
                self.restarts.failed();
                self.server = Server::Down;
                let message = error.to_string();
                for command in std::mem::take(&mut self.deferred) {
                    command.fail(ProviderError::Unavailable(message.clone()));
                }
                let threads: Vec<ThreadId> = self.threads.keys().cloned().collect();
                for thread_id in threads {
                    self.pause_if_waiting(
                        &thread_id,
                        PauseReason::Unavailable {
                            error: message.clone(),
                        },
                    )
                    .await;
                }
            }
        }
    }

    /// Reads the plan that pays for the account (D24) and the account's usage (D38).
    fn read_account(&self, generation: u64, codex: Codex) {
        let reader = codex.clone();
        self.spawn(async move {
            let plan = match reader
                .request::<methods::AccountRead>(&types::GetAccountParams::default())
                .await
            {
                Ok(read) => wire::plan(&read),
                Err(_) => None,
            };
            Done::Account { generation, plan }
        });
        self.read_usage(codex, None);
    }

    pub(super) fn on_account(&mut self, generation: u64, plan: Option<String>) {
        if generation == self.generation {
            self.plan = plan;
        }
    }

    /// Loads the thread into the running app-server, resuming its Codex thread or starting one for
    /// a thread that has none. Returns its Codex id once loaded; `None` while that is under way.
    pub(super) async fn attached(
        &mut self,
        thread_id: &ThreadId,
    ) -> Result<Option<String>, ProviderError> {
        self.load(thread_id).await?;
        let thread = self.threads.get(thread_id).expect("loaded above");
        // Codex applies a context limit only when it loads the thread, so a thread loaded under
        // another limit is unloaded and loaded again.
        let reload = match &thread.link {
            Link::Attached {
                native_id,
                context_limit,
                ..
            } if *context_limit == thread.settings.context_limit_tokens => {
                return Ok(Some(native_id.clone()));
            }
            Link::Attached { native_id, .. } => Some(native_id.clone()),
            Link::Attaching => return Ok(None),
            Link::Detached | Link::Failed => None,
        };
        let Some(codex) = self.connection()? else {
            return Ok(None);
        };
        let stored = self
            .store
            .call({
                let thread_id = thread_id.clone();
                move |store| store.provider_thread(&thread_id)
            })
            .await?;
        let thread = self.threads.get_mut(thread_id).expect("loaded above");
        thread.link = Link::Attaching;
        let profile = Profile::for_settings(&stored.settings).id().to_owned();
        let config = stored
            .settings
            .context_limit_tokens
            .map(|tokens| json!({ "model_context_window": tokens }));
        let (thread_id, generation) = (thread_id.clone(), self.generation);
        let context_limit = stored.settings.context_limit_tokens;
        self.spawn(async move {
            if let Some(native_id) = reload {
                let params = types::ThreadUnsubscribeParams {
                    thread_id: native_id,
                };
                // A thread Codex already unloaded needs nothing more.
                let _ = codex.request::<methods::ThreadUnsubscribe>(&params).await;
            }
            let result = open_thread(
                &codex,
                stored.native_id,
                stored.folder,
                stored.settings.model,
                profile,
                config,
            )
            .await
            .map(|opened| Opened {
                context_limit,
                ..opened
            });
            Done::Attached {
                thread_id,
                generation,
                result,
            }
        });
        Ok(None)
    }

    pub(super) async fn on_attached(
        &mut self,
        thread_id: ThreadId,
        generation: u64,
        result: Result<Opened, RequestError>,
    ) {
        let Some(thread) = self.threads.get_mut(&thread_id) else {
            return;
        };
        if generation != self.generation {
            thread.link = Link::Detached;
            return;
        }
        match result {
            Ok(opened) => {
                thread.link = Link::Attached {
                    native_id: opened.native_id.clone(),
                    model: opened.model.clone(),
                    context_limit: opened.context_limit,
                };
                let recorded = if opened.is_new {
                    let plan = self.plan.clone();
                    let native_id = opened.native_id.clone();
                    self.write(&thread_id, move |write| {
                        write.begin_provider_session(Provider::Codex, &native_id, plan)
                    })
                    .await
                } else {
                    self.reconcile_history(&thread_id, &opened.turns).await
                };
                if recorded.is_ok() {
                    self.dispatch(&thread_id).await;
                }
            }
            Err(error) => {
                let message = format!("Codex could not load the thread: {error}");
                thread.link = Link::Failed;
                self.pause_if_waiting(&thread_id, PauseReason::Unavailable { error: message })
                    .await;
            }
        }
    }

    /// Loads each thread waiting on Codex's history, starting Codex when it is down. A thread
    /// keeps its place while Codex starts; a start that fails leaves it for the user to settle.
    pub(super) async fn reconcile_waiting(&mut self) {
        for thread_id in std::mem::take(&mut self.reconcile) {
            let waits_for_codex = matches!(self.attached(&thread_id).await, Ok(None))
                && matches!(self.server, Server::Starting);
            if waits_for_codex {
                self.reconcile.push(thread_id);
            }
        }
    }

    /// Settles each message whose delivery was uncertain against Codex's history: a message the
    /// history holds was accepted. One the history lacks stays uncertain for the user (D11). Once
    /// nothing uncertain is left, the pause lifts, unless messages wait: the turn that took the
    /// message ended with the Codex that ran it, so it counts as interrupted.
    pub(super) async fn reconcile_history(
        &mut self,
        thread_id: &ThreadId,
        turns: &[types::Turn],
    ) -> Result<(), ProviderError> {
        let found: Vec<(kratos_protocol::MessageId, TurnId)> = self
            .store
            .call({
                let thread_id = thread_id.clone();
                move |store| store.pending_messages(&thread_id)
            })
            .await?
            .into_iter()
            .filter(|message| matches!(message.status, MessageStatus::Unknown { .. }))
            .filter_map(|message| {
                wire::turn_with_message(turns, message.id.as_str())
                    .map(|turn| (message.id, TurnId::new(turn.id.clone())))
            })
            .collect();
        if found.is_empty() {
            return Ok(());
        }
        self.write(thread_id, move |write| settle_from_history(write, found))
            .await
    }

    /// Codex ended. A turn it was running ended with it; a start it was answering stays uncertain
    /// until its own completion arrives. Waiting requests close, and every thread must load again.
    pub(super) async fn on_exit(&mut self, exit: Exit) {
        self.server = Server::Down;
        self.restarts.exited();
        let error = exit.to_string();
        let thread_ids: Vec<ThreadId> = self.threads.keys().cloned().collect();
        for thread_id in &thread_ids {
            let thread = self.threads.get_mut(thread_id).expect("listed above");
            thread.link = Link::Detached;
            let Turn::Running { turn_id, .. } = &thread.turn else {
                continue;
            };
            let turn_id = turn_id.clone();
            thread.turn = Turn::Idle;
            let changes = thread.changes.take();
            let outcome = TurnOutcome::Failed {
                error: error.clone(),
            };
            let pause = PauseReason::TurnFailed {
                turn_id: turn_id.clone(),
                error: error.clone(),
            };
            let _ = self
                .record_turn_end(
                    thread_id,
                    turn_id,
                    outcome,
                    changes,
                    super::turns::After::PauseIfWaiting(pause),
                )
                .await;
        }
        for thread_id in self.requests.threads() {
            let closed = self.requests.close(&thread_id, |_| true);
            let _ = self
                .write(&thread_id, move |write| {
                    settle_closed(write, &closed, "Codex exited")
                })
                .await;
        }
        self.ready.clear();
    }

    /// Closes the app-server and waits for it to end; its exit arrives as usual.
    pub(super) async fn stop_server(&mut self, reason: &str) {
        if let Server::Up { codex, .. } = &self.server {
            eprintln!("closing Codex: {reason}");
            codex.close().await;
        }
    }

    /// Pauses the thread for `reason` when messages wait in its queue, so the user sees why
    /// nothing starts (L7).
    pub(super) async fn pause_if_waiting(&mut self, thread_id: &ThreadId, reason: PauseReason) {
        let _ = self
            .write(thread_id, move |write| {
                if !write.pending_messages()?.is_empty() {
                    write.pause(reason)?;
                }
                Ok(())
            })
            .await;
    }

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
        if stored.thread.provider != Provider::Codex {
            return Err(ProviderError::Conflict(format!(
                "Thread {thread_id} is not a Codex thread"
            )));
        }
        self.threads.insert(
            thread_id.clone(),
            ThreadState::new(stored.folder, stored.settings),
        );
        Ok(())
    }
}

/// Records each found message as accepted by its turn, then lifts a delivery pause once nothing
/// uncertain is left, or turns it into an interrupted pause while messages wait.
fn settle_from_history(
    write: &mut ThreadWrite<'_>,
    found: Vec<(kratos_protocol::MessageId, TurnId)>,
) -> Result<(), StoreError> {
    for (message_id, turn_id) in found {
        write.set_message_status(&message_id, MessageStatus::Accepted { turn_id })?;
    }
    let pending = write.pending_messages()?;
    let uncertain = pending.iter().any(|message| {
        matches!(
            message.status,
            MessageStatus::Unknown { .. } | MessageStatus::Failed { .. }
        )
    });
    let paused_for_delivery = matches!(
        write.provider_thread()?.paused,
        Some(PauseReason::DeliveryUncertain { .. })
    );
    match (paused_for_delivery && !uncertain, pending.is_empty()) {
        (false, _) => {}
        (true, true) => {
            write.resume()?;
        }
        (true, false) => {
            write.pause(PauseReason::Interrupted)?;
        }
    }
    Ok(())
}

/// Resumes the Codex thread `native_id`, or starts one in `folder`, with every turn's approvals
/// routed to Kratos.
async fn open_thread(
    codex: &Codex,
    native_id: Option<String>,
    folder: String,
    model: Option<String>,
    profile: String,
    config: Option<serde_json::Value>,
) -> Result<Opened, RequestError> {
    let config = config.and_then(|config| config.as_object().cloned());
    match native_id {
        Some(native_id) => {
            let mut params: types::ThreadResumeParams =
                wire::required(json!({ "threadId": native_id }));
            params.cwd = Some(folder);
            params.model = model;
            params.approval_policy = Some(types::AskForApproval::Untrusted);
            params.approvals_reviewer = Some(types::ApprovalsReviewer::User);
            params.permissions = Some(profile);
            params.config = config;
            let resumed = codex.request::<methods::ThreadResume>(&params).await?;
            Ok(Opened {
                native_id: resumed.thread.id,
                model: resumed.model,
                turns: resumed.thread.turns,
                is_new: false,
                context_limit: None,
            })
        }
        None => {
            let params = types::ThreadStartParams {
                cwd: Some(folder),
                model,
                approval_policy: Some(types::AskForApproval::Untrusted),
                approvals_reviewer: Some(types::ApprovalsReviewer::User),
                permissions: Some(profile),
                config,
                ephemeral: Some(false),
                ..types::ThreadStartParams::default()
            };
            let started = codex.request::<methods::ThreadStart>(&params).await?;
            Ok(Opened {
                native_id: started.thread.id,
                model: started.model,
                turns: started.thread.turns,
                is_new: true,
                context_limit: None,
            })
        }
    }
}
