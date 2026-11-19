use std::ffi::OsString;
use std::time::{Duration, Instant};

use kratos_codex_client::{
    Codex, ConnectError, Exit, Incoming, Launch, RequestError, connect, methods, types,
};
use kratos_protocol::{ContextLimit, MessageStatus, Provider, RequiredText, ThreadId, TurnId};
use serde_json::json;
use tokio::sync::mpsc;

use super::actor::{Actor, Done, Frame, Opened};
use super::profiles::{Profile, temp_folder};
use super::recovery::{self, EndReason, EndingTurn, Unfinished};
use super::servers::{Server, ServerKey, TerminalCheck};
use super::threads::{Link, ThreadState, Turn};
use super::{CodexSettings, ProviderError, policy, wire};
use crate::mcp::{self, McpServer};
use crate::providers::queue;

const FIRST_RETRY_WAIT: Duration = Duration::from_secs(1);
const LONGEST_RETRY_WAIT: Duration = Duration::from_secs(60);
/// An exit sooner than this after the start counts as a failed start.
const SETTLED_AFTER: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub(super) enum StartError {
    #[error(transparent)]
    Connect(#[from] ConnectError),
    /// The server started, but its sandbox is not ready, so it was closed.
    #[cfg(windows)]
    #[error("Codex did not complete the handshake: {0}")]
    Sandbox(#[source] super::sandbox::SandboxError),
}

#[derive(Default)]
pub(super) struct Restarts {
    failures_in_a_row: u32,
    not_before: Option<Instant>,
    started_at: Option<Instant>,
}

impl Restarts {
    #[cfg(test)]
    pub(super) fn settled_start(&mut self) {
        self.started_at = Some(Instant::now() - SETTLED_AFTER);
    }

    pub(super) fn in_backoff(&self) -> bool {
        self.may_start().is_err()
    }

    /// `Err` with the wait left while the last failure's backoff runs.
    pub(super) fn may_start(&self) -> Result<(), Duration> {
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

/// So a `wait_for_turn` answers `timedOut` itself before Codex gives up on the call.
const MCP_TOOL_TIMEOUT_SECONDS: u64 = mcp::MAX_WAIT_SECONDS + 100;

fn thread_config(
    context_limit: Option<ContextLimit>,
    mcp: Option<&McpServer>,
    thread_id: &ThreadId,
) -> Option<serde_json::Value> {
    let mut config = serde_json::Map::new();
    if let Some(tokens) = context_limit {
        config.insert("model_context_window".to_owned(), json!(tokens.get()));
    }
    if let Some(mcp) = mcp {
        let mut kratos = mcp.launch(thread_id);
        kratos["default_tools_approval_mode"] = json!("approve");
        kratos["tool_timeout_sec"] = json!(MCP_TOOL_TIMEOUT_SECONDS);
        config.insert("mcp_servers".to_owned(), json!({ "kratos": kratos }));
    }
    (!config.is_empty()).then_some(serde_json::Value::Object(config))
}

pub(super) fn launch(settings: &CodexSettings) -> Launch {
    let mut args: Vec<OsString> = vec!["app-server".into()];
    args.extend(settings.extra_args.iter().cloned());
    #[cfg(windows)]
    args.extend(super::sandbox::launch_arguments());
    args.extend(settings.protected.launch_arguments(&temp_folder()));
    Launch {
        program: settings.program.clone(),
        args,
        cwd: settings.server_folder.clone(),
        // Codex and the tools it runs get none of Kratos's own settings (L8).
        env_remove: std::env::vars_os()
            .map(|(name, _)| name)
            .filter(|name| is_kratos_variable(name))
            .collect(),
        env_set: settings.env_set.clone(),
        client_info: types::ClientInfo {
            name: "kratos".to_owned(),
            title: Some("Kratos".to_owned()),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        request_timeout: settings.request_timeout,
        close_timeout: CLOSE_TIMEOUT,
        max_line_bytes: kratos_codex_client::MAX_LINE_BYTES,
    }
}

/// One of Kratos's own variables. Windows ignores the case of a variable's name, so `kratos_x`
/// is read as `KRATOS_X` there; the check folds case on every system, as Claude's launch does.
fn is_kratos_variable(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy()
        .to_ascii_uppercase()
        .starts_with("KRATOS_")
}

impl Actor {
    pub(super) async fn on_connected(
        &mut self,
        key: ServerKey,
        result: Result<(Codex, mpsc::Receiver<Incoming>), StartError>,
    ) {
        match result {
            Ok((codex, incoming)) => {
                let generation = self.servers.generation(&key);
                let slot = self.servers.slot(&key);
                slot.restarts.started();
                slot.server = Server::Up(codex.clone());
                let deferred = std::mem::take(&mut slot.deferred);
                tokio::spawn(forward(
                    key.clone(),
                    generation,
                    incoming,
                    self.frame_sender.clone(),
                ));
                self.read_account(&key, codex);
                for deferred in deferred {
                    self.on_command(deferred).await;
                }
                self.reconcile_waiting().await;
                if let ServerKey::Thread(thread_id) = &key {
                    self.dispatch(thread_id).await;
                }
            }
            Err(error) => {
                let slot = self.servers.slot(&key);
                slot.restarts.failed();
                slot.server = Server::Down;
                let message = error.to_string();
                for deferred in std::mem::take(&mut slot.deferred) {
                    deferred.fail_deferred(ProviderError::Unavailable(message.clone()));
                }
                if let ServerKey::Thread(thread_id) = &key {
                    self.pause_unavailable_if_waiting(thread_id, message).await;
                }
                self.admit_waiting();
            }
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
        let (reload, access) = match &thread.link {
            Link::Attached {
                native_id,
                context_limit,
                ..
            } if *context_limit == thread.settings.context_limit_tokens => {
                return Ok(Some(native_id.clone()));
            }
            Link::Attached {
                native_id, access, ..
            } => (Some(native_id.clone()), *access),
            Link::Attaching => return Ok(None),
            Link::Detached => (None, thread.turn_settings().access()),
        };
        let key = ServerKey::Thread(thread_id.clone());
        let Some(codex) = self.connection(&key)? else {
            return Ok(None);
        };
        let native_id = self
            .store
            .call({
                let thread_id = thread_id.clone();
                move |store| store.provider_thread(&thread_id)
            })
            .await?
            .native_id;
        let thread = self.threads.get_mut(thread_id).expect("loaded above");
        thread.link = Link::Attaching;
        let profile = Profile::for_settings(&thread.settings).id().to_owned();
        let context_limit = thread.settings.context_limit_tokens;
        let config = thread_config(context_limit, self.settings.mcp.as_ref(), thread_id);
        let folder = thread.folder.clone();
        let model = thread
            .settings
            .model
            .as_ref()
            .map(|model| model.as_str().to_owned());
        let thread_id = thread_id.clone();
        self.spawn(&key, async move {
            if let Some(native_id) = reload {
                let params = types::ThreadUnsubscribeParams {
                    thread_id: native_id,
                };
                // A thread Codex already unloaded needs nothing more.
                let _ = codex.request::<methods::ThreadUnsubscribe>(&params).await;
            }
            let result = open_thread(&codex, native_id, folder, model, profile, config).await;
            Done::Attached {
                thread_id,
                context_limit,
                access,
                result,
            }
        });
        Ok(None)
    }

    pub(super) async fn on_attached(
        &mut self,
        thread_id: ThreadId,
        context_limit: Option<ContextLimit>,
        access: kratos_protocol::AccessLimit,
        result: Result<Opened, RequestError>,
    ) {
        let Some(thread) = self.threads.get_mut(&thread_id) else {
            return;
        };
        match result {
            Ok(opened) => {
                thread.link = Link::Attached {
                    native_id: opened.native_id.clone(),
                    model: opened.model.clone(),
                    context_limit,
                    access,
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
                let discards = self
                    .threads
                    .get_mut(&thread_id)
                    .map(|thread| std::mem::take(&mut thread.discards))
                    .unwrap_or_default();
                for (message_id, reply) in discards {
                    self.discard(thread_id.clone(), message_id, reply).await;
                }
                if recorded.is_ok() {
                    self.dispatch(&thread_id).await;
                }
            }
            Err(error) => {
                let message = format!("Codex could not load the thread: {error}");
                thread.link = Link::Detached;
                if matches!(error, RequestError::Rpc { .. }) {
                    for (message_id, reply) in std::mem::take(&mut thread.discards) {
                        self.settle_discard(&thread_id, message_id, None, reply)
                            .await;
                    }
                } else {
                    fail_discards(thread, &message);
                }
                self.pause_unavailable_if_waiting(&thread_id, message).await;
            }
        }
    }

    /// A fresh session keeps the history but drops approvals granted under wider access.
    pub(super) fn fork_for_access(
        &mut self,
        thread_id: &ThreadId,
        limit: Option<kratos_protocol::AccessLimit>,
    ) -> bool {
        let thread = self.threads.get_mut(thread_id).expect("thread loaded");
        let settings = match limit {
            Some(limit) => thread.settings.limited_to(limit),
            None => thread.settings.clone(),
        };
        let Link::Attached {
            native_id, access, ..
        } = &thread.link
        else {
            unreachable!("the caller attached this thread");
        };
        if thread.turn_settings.is_none() || settings.access().covers(*access) {
            return false;
        }
        let context_limit = settings.context_limit_tokens;
        let access = settings.access();
        let mut params: types::ThreadForkParams = wire::required(json!({
            "threadId": native_id, "ephemeral": false, "excludeTurns": true,
            "deferGoalContinuation": true,
        }));
        params.cwd = Some(thread.folder.clone());
        params.runtime_workspace_roots = Some(vec![types::AbsolutePathBuf(thread.folder.clone())]);
        params.approval_policy = Some(policy::APPROVAL_POLICY);
        params.approvals_reviewer = Some(policy::APPROVALS_REVIEWER);
        params.permissions = Some(Profile::for_settings(&settings).id().to_owned());
        params.model = settings.model.map(RequiredText::into_string);
        params.config = thread_config(context_limit, self.settings.mcp.as_ref(), thread_id)
            .and_then(|config| config.as_object().cloned());
        let native_id = native_id.clone();
        thread.link = Link::Attaching;
        let key = ServerKey::Thread(thread_id.clone());
        let codex = self
            .servers
            .codex(&key)
            .expect("an attached thread's server is up");
        let thread_id = thread_id.clone();
        self.spawn(&key, async move {
            let result = codex
                .request::<methods::ThreadFork>(&params)
                .await
                .map(|forked| Opened {
                    native_id: forked.thread.id,
                    model: forked.model,
                    turns: forked.thread.turns,
                    is_new: true,
                });
            if result.is_ok()
                && let Err(error) = codex
                    .request::<methods::ThreadUnsubscribe>(&types::ThreadUnsubscribeParams {
                        thread_id: native_id,
                    })
                    .await
            {
                eprintln!("could not unsubscribe the previous Codex session: {error}");
            }
            Done::Attached {
                thread_id,
                context_limit,
                access,
                result,
            }
        });
        true
    }

    /// Loads each thread waiting on Codex's history, starting Codex when it is down. A thread
    /// keeps its place while Codex starts; a start that fails leaves it for the user to settle.
    pub(super) async fn reconcile_waiting(&mut self) {
        for thread_id in std::mem::take(&mut self.reconcile) {
            let key = ServerKey::Thread(thread_id.clone());
            let waits_for_codex = matches!(self.attached(&thread_id).await, Ok(None))
                && (self
                    .servers
                    .get(&key)
                    .is_some_and(|slot| matches!(slot.server, Server::Starting))
                    || self.servers.waits(&key));
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
        self.write(thread_id, move |write| {
            queue::settle_from_history(write, found)
        })
        .await
    }

    pub(super) async fn detach(&mut self, thread_id: &ThreadId, error: &str) {
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return;
        };
        thread.link = Link::Detached;
        fail_discards(thread, &format!("Codex could not load the thread: {error}"));
        let mut sending = std::mem::take(&mut thread.steering);
        if let Turn::Starting { message_id, .. } = &thread.turn {
            sending.insert(message_id.clone());
        }
        let mut unfinished = Unfinished {
            sending: sending.into_iter().collect(),
            requests: self.requests.close(thread_id, |_| true),
            ..Unfinished::default()
        };
        let stopped = matches!(thread.turn, Turn::Running { stopping: true, .. });
        if let Some((turn_id, work)) = thread.turn.end_running() {
            unfinished.running_turn = Some(EndingTurn {
                id: turn_id,
                stopped,
                changes: work.changes.finish(),
                context: work.context,
            });
        }
        thread.file_items.clear();
        self.end_start(thread_id, Ok(()));
        let reason = EndReason::Exited(error.to_owned());
        let _ = self
            .write(thread_id, move |write| {
                recovery::settle(write, unfinished, reason)
            })
            .await;
    }

    pub(super) async fn on_exit(&mut self, key: ServerKey, exit: Exit) {
        self.servers.advance_generation(&key);
        let slot = self.servers.slot(&key);
        slot.server = Server::Down;
        slot.terminals = TerminalCheck::NotAsked;
        slot.restarts.exited();
        if let ServerKey::Thread(thread_id) = &key {
            self.detach(thread_id, &exit.to_string()).await;
            self.ready.retain(|waiting| waiting != thread_id);
            self.dispatch(thread_id).await;
        }
        self.servers.forget_if_unused(&key);
        self.admit_waiting();
    }

    pub(super) async fn stop_server(&mut self, key: &ServerKey, reason: &str) {
        if let Some(codex) = self.servers.codex(key) {
            eprintln!("closing Codex for {key:?}: {reason}");
            tokio::spawn(async move { codex.close().await });
        }
    }

    pub(super) async fn close_all(&mut self) {
        for key in self.servers.keys() {
            if let Some(codex) = self.servers.codex(&key) {
                codex.close().await;
            }
        }
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

pub(super) async fn connect_ready(
    launch: Launch,
    capabilities: types::InitializeCapabilities,
) -> Result<(Codex, mpsc::Receiver<Incoming>), StartError> {
    #[cfg(windows)]
    let cwd = launch.cwd.clone();
    let (codex, incoming) = connect(launch, capabilities).await?;
    #[cfg(windows)]
    {
        let mut incoming = incoming;
        if let Err(error) = super::sandbox::prepare(&codex, &mut incoming, cwd).await {
            codex.close().await;
            return Err(StartError::Sandbox(error));
        }
        Ok((codex, incoming))
    }
    #[cfg(not(windows))]
    Ok((codex, incoming))
}

fn fail_discards(thread: &mut ThreadState, error: &str) {
    for (_, reply) in std::mem::take(&mut thread.discards) {
        let _ = reply.send(Err(ProviderError::Unavailable(error.to_owned())));
    }
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
            params.approval_policy = Some(policy::APPROVAL_POLICY);
            params.approvals_reviewer = Some(policy::APPROVALS_REVIEWER);
            params.permissions = Some(profile);
            params.config = config;
            let resumed = codex.request::<methods::ThreadResume>(&params).await?;
            Ok(Opened {
                native_id: resumed.thread.id,
                model: resumed.model,
                turns: resumed.thread.turns,
                is_new: false,
            })
        }
        None => {
            let params = types::ThreadStartParams {
                cwd: Some(folder),
                model,
                approval_policy: Some(policy::APPROVAL_POLICY),
                approvals_reviewer: Some(policy::APPROVALS_REVIEWER),
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
            })
        }
    }
}

async fn forward(
    key: ServerKey,
    generation: u64,
    mut incoming: mpsc::Receiver<Incoming>,
    frames: mpsc::Sender<Frame>,
) {
    while let Some(incoming) = incoming.recv().await {
        let frame = Frame {
            key: key.clone(),
            generation,
            incoming,
        };
        if frames.send(frame).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kratos_variables_are_found_in_any_case() {
        for name in ["KRATOS_HOME", "kratos_token", "Kratos_Port"] {
            assert!(is_kratos_variable(std::ffi::OsStr::new(name)), "{name}");
        }
        for name in ["PATH", "KRATOSX", "MY_KRATOS_HOME"] {
            assert!(!is_kratos_variable(std::ffi::OsStr::new(name)), "{name}");
        }
    }
}
