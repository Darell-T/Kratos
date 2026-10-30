//! The actor: its inputs, its state, and the loop that serves them one at a time.
//!
//! Three inputs: commands from handles, messages from Codex, and completions of the work the actor
//! started (a request to Codex, a deadline). Each handler runs to its end before the next input,
//! so no two handlers see a thread half changed. A handler awaits only the store, never Codex.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

use kratos_codex_client::{Codex, ConnectError, Incoming, RequestError, types};
use kratos_protocol::{
    ApprovalDecision, ApprovalId, ApprovalOutcome, CommandResult, ContinuationId, Message,
    MessageId, MessageInput, MessageOrigin, Provider, QuestionId, QuestionOutcome, Skill, ThreadId,
    ThreadSettings, TurnChanges, TurnOutcome,
};
use kratos_store::{Settle, StoreError, ThreadWrite};
use tokio::sync::{mpsc, oneshot};

use super::requests::Requests;
use super::threads::ThreadState;
use super::turns::{After, Stopper};
use super::{CodexSettings, Continued, ProviderError, Sent};
use crate::hub::Hub;
use crate::store_thread::StoreThread;
use crate::usage::UsageTracker;

/// How many threads may wait on a `turn/start` answer at once. The rest wait their turn, so a
/// burst of messages to many threads does not flood Codex (D85).
pub(super) const MAX_STARTING: usize = 4;

pub(super) type Reply<T> = oneshot::Sender<Result<T, ProviderError>>;

pub(super) enum Command {
    Send {
        thread_id: ThreadId,
        message_id: MessageId,
        input: Vec<MessageInput>,
        origin: MessageOrigin,
        reply: Reply<Sent>,
    },
    Steer {
        thread_id: ThreadId,
        message_id: MessageId,
        reply: Reply<Message>,
    },
    Remove {
        thread_id: ThreadId,
        message_id: MessageId,
        reply: Reply<Message>,
    },
    Retry {
        thread_id: ThreadId,
        message_id: MessageId,
        reply: Reply<Message>,
    },
    Discard {
        thread_id: ThreadId,
        message_id: MessageId,
        reply: Reply<Message>,
    },
    Stop {
        thread_id: ThreadId,
        reply: Reply<()>,
    },
    Resume {
        thread_id: ThreadId,
        reply: Reply<()>,
    },
    SetSettings {
        thread_id: ThreadId,
        settings: ThreadSettings,
        reply: Reply<()>,
    },
    CheckSettings {
        settings: ThreadSettings,
        reply: Reply<()>,
    },
    Decide {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        reply: Reply<ApprovalOutcome>,
    },
    Answer {
        question_id: QuestionId,
        answers: BTreeMap<String, Vec<String>>,
        reply: Reply<QuestionOutcome>,
    },
    Skills {
        thread_id: ThreadId,
        reply: Reply<Vec<Skill>>,
    },
    Run {
        thread_id: ThreadId,
        command: Vec<String>,
        timeout: Duration,
        reply: Reply<CommandResult>,
    },
    RefreshUsage {
        reply: Reply<()>,
    },
    Continue {
        thread_id: ThreadId,
        continuation_id: ContinuationId,
        reply: Reply<Continued>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// Work the actor started, finished. Each names the thread, turn, or attempt it belongs to, so the
/// actor can tell a stale one.
pub(super) enum Done {
    Connected {
        generation: u64,
        result: Result<(Codex, mpsc::Receiver<Incoming>), ConnectError>,
    },
    Account {
        generation: u64,
        plan: Option<String>,
    },
    Attached {
        thread_id: ThreadId,
        generation: u64,
        result: Result<Opened, RequestError>,
    },
    StartAnswered {
        thread_id: ThreadId,
        attempt: u64,
        result: Result<Box<types::Turn>, RequestError>,
    },
    /// The history read after an uncertain `turn/start` or `turn/steer`: the turn that took the
    /// message, if any.
    DeliveryChecked {
        thread_id: ThreadId,
        message_id: MessageId,
        attempt: Option<u64>,
        error: String,
        found: Option<Box<types::Turn>>,
    },
    Steered {
        thread_id: ThreadId,
        message_id: MessageId,
        result: Result<(), RequestError>,
    },
    Interrupted {
        thread_id: ThreadId,
        turn_id: kratos_protocol::TurnId,
        result: Result<(), RequestError>,
        stopper: Stopper,
    },
    StopExpired {
        thread_id: ThreadId,
        turn_id: kratos_protocol::TurnId,
    },
    Deadline {
        thread_id: ThreadId,
        turn_id: kratos_protocol::TurnId,
        epoch: u64,
    },
    HistoryForDiscard {
        thread_id: ThreadId,
        message_id: MessageId,
        result: Result<Box<types::Thread>, RequestError>,
        reply: Reply<Message>,
    },
    UsageRead {
        tracker_generation: u64,
        result: Result<Box<types::GetAccountRateLimitsResponse>, RequestError>,
        reply: Option<Reply<()>>,
    },
}

/// A thread Codex loaded, and whether it is new.
pub(super) struct Opened {
    pub native_id: String,
    pub model: String,
    pub turns: Vec<types::Turn>,
    pub is_new: bool,
    pub context_limit: Option<u64>,
}

/// The app-server, as far as the actor knows.
pub(super) enum Server {
    Down,
    Starting,
    Up {
        codex: Codex,
        incoming: mpsc::Receiver<Incoming>,
    },
}

pub(crate) struct Actor {
    pub(super) settings: CodexSettings,
    pub(super) hub: Hub,
    pub(super) store: StoreThread,
    pub(super) usage: UsageTracker,
    pub(super) server: Server,
    /// Counts app-server starts. A completion from an earlier server is stale.
    pub(super) generation: u64,
    pub(super) restarts: super::connection::Restarts,
    /// Each model's own context window, as Codex reported it with no setting of Kratos's in force
    /// (D76).
    pub(super) windows: HashMap<String, i64>,
    /// The plan that pays for the running server's account (D24).
    pub(super) plan: Option<String>,
    pub(super) threads: HashMap<ThreadId, ThreadState>,
    pub(super) requests: Requests,
    /// Threads waiting for a start slot, oldest first.
    pub(super) ready: VecDeque<ThreadId>,
    pub(super) next_attempt: u64,
    /// Commands that wait for the app-server to come up.
    pub(super) deferred: Vec<Command>,
    /// Threads to load once the app-server is up, so its history settles their uncertain messages.
    pub(super) reconcile: Vec<ThreadId>,
    pub(super) done: mpsc::UnboundedSender<Done>,
    pub(super) completions: mpsc::UnboundedReceiver<Done>,
    /// Where usage-limit stops go: the scheduler owns continuation (D39).
    pub(super) limit_stops: Option<mpsc::UnboundedSender<crate::usage::scheduler::LimitStop>>,
}

impl Actor {
    pub(super) async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        uncertain: Vec<ThreadId>,
    ) {
        for thread_id in uncertain {
            if self.load(&thread_id).await.is_ok() {
                self.reconcile.push(thread_id);
            }
        }
        self.reconcile_waiting().await;
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Shutdown { reply }) => {
                        self.shutdown().await;
                        let _ = reply.send(());
                        return;
                    }
                    Some(command) => self.on_command(command).await,
                    None => {
                        self.shutdown().await;
                        return;
                    }
                },
                Some(done) = self.completions.recv() => self.on_done(done).await,
                incoming = next_incoming(&mut self.server) => self.on_incoming(incoming).await,
            }
        }
    }

    /// Starts `work` in a task of its own and hands its result back as a completion.
    pub(super) fn spawn<F>(&self, work: F)
    where
        F: Future<Output = Done> + Send + 'static,
    {
        let done = self.done.clone();
        tokio::spawn(async move {
            // The actor holds the receiver for as long as it runs.
            let _ = done.send(work.await);
        });
    }

    async fn on_done(&mut self, done: Done) {
        match done {
            Done::Connected { generation, result } => self.on_connected(generation, result).await,
            Done::Account { generation, plan } => self.on_account(generation, plan),
            Done::Attached {
                thread_id,
                generation,
                result,
            } => self.on_attached(thread_id, generation, result).await,
            Done::StartAnswered {
                thread_id,
                attempt,
                result,
            } => self.on_start_answered(thread_id, attempt, result).await,
            Done::DeliveryChecked {
                thread_id,
                message_id,
                attempt,
                error,
                found,
            } => {
                self.on_delivery_checked(thread_id, message_id, attempt, error, found)
                    .await;
            }
            Done::Steered {
                thread_id,
                message_id,
                result,
            } => self.on_steered(thread_id, message_id, result).await,
            Done::Interrupted {
                thread_id,
                turn_id,
                result,
                stopper,
            } => {
                self.on_interrupted(thread_id, turn_id, result, stopper)
                    .await
            }
            Done::StopExpired { thread_id, turn_id } => {
                self.on_stop_expired(&thread_id, &turn_id).await
            }
            Done::Deadline {
                thread_id,
                turn_id,
                epoch,
            } => self.on_deadline(thread_id, turn_id, epoch).await,
            Done::HistoryForDiscard {
                thread_id,
                message_id,
                result,
                reply,
            } => {
                self.on_history_for_discard(thread_id, message_id, result, reply)
                    .await
            }
            Done::UsageRead {
                tracker_generation,
                result,
                reply,
            } => self.on_usage_read(tracker_generation, result, reply),
        }
    }

    /// Runs `job` as one write to the thread's records. A write that fails stops Codex rather
    /// than let it run unrecorded (L2), and the caller gets the error.
    pub(super) async fn write<T, F>(
        &mut self,
        thread_id: &ThreadId,
        job: F,
    ) -> Result<T, ProviderError>
    where
        F: FnOnce(&mut ThreadWrite<'_>) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        match self.hub.write(thread_id.clone(), job).await {
            Ok(written) => Ok(written.value),
            Err(
                error @ (StoreError::UnknownThread(_)
                | StoreError::UnknownMessage { .. }
                | StoreError::UnknownApproval(_)
                | StoreError::UnknownQuestion(_)
                | StoreError::UnknownContinuation(_)
                | StoreError::MessageTransition { .. }
                | StoreError::MessageIdTaken(_)),
            ) => Err(ProviderError::Store(error)),
            Err(error) => {
                eprintln!("cannot record Codex work, so Codex stops: {error}");
                self.stop_server(&format!("Kratos could not record Codex's work: {error}"))
                    .await;
                Err(ProviderError::Store(error))
            }
        }
    }

    /// Records how a turn ended and closes what waited on it, then pauses the queue as `after`
    /// says.
    pub(super) async fn record_turn_end(
        &mut self,
        thread_id: &ThreadId,
        turn_id: kratos_protocol::TurnId,
        outcome: TurnOutcome,
        changes: TurnChanges,
        after: After,
    ) -> Result<(), ProviderError> {
        let context = self
            .threads
            .get_mut(thread_id)
            .and_then(|thread| thread.context.take());
        let closed = self
            .requests
            .close(thread_id, |waiting| waiting.turn_id == turn_id);
        self.write(thread_id, move |write| {
            settle_closed(write, &closed, "The turn ended")?;
            write.complete_turn(turn_id, outcome, changes, context)?;
            match after {
                After::Continue => {}
                After::PauseIfWaiting(reason) => {
                    if !write.pending_messages()?.is_empty() {
                        write.pause(reason)?;
                    }
                }
                After::Pause(reason) => {
                    write.pause(reason)?;
                }
            }
            Ok(())
        })
        .await?;
        if let Some(thread) = self.threads.get_mut(thread_id) {
            thread.file_items.clear();
        }
        Ok(())
    }

    /// Reports a usage-limit stop to the scheduler, which plans the continuation (D39).
    pub(super) fn report_limit_stop(&self, thread_id: &ThreadId, turn_id: kratos_protocol::TurnId) {
        if let Some(stops) = &self.limit_stops {
            let _ = stops.send(crate::usage::scheduler::LimitStop {
                thread_id: thread_id.clone(),
                turn_id,
                provider: Provider::Codex,
            });
        }
    }

    async fn shutdown(&mut self) {
        self.stop_server("Kratos is shutting down").await;
    }
}

/// Settles requests a step closed, in the same write as the step.
pub(super) fn settle_closed(
    write: &mut ThreadWrite<'_>,
    closed: &super::requests::Closed,
    reason: &str,
) -> Result<(), StoreError> {
    for approval in &closed.approvals {
        write.settle_approval(approval, Settle::Close(reason.to_owned()))?;
    }
    for question in &closed.questions {
        write.settle_question(question, Settle::Close(reason.to_owned()))?;
    }
    Ok(())
}

async fn next_incoming(server: &mut Server) -> Incoming {
    match server {
        Server::Up { incoming, .. } => match incoming.recv().await {
            Some(incoming) => incoming,
            // The connection reports its exit before it closes the channel, so this only follows
            // an exit already handled.
            None => std::future::pending().await,
        },
        Server::Down | Server::Starting => std::future::pending().await,
    }
}
