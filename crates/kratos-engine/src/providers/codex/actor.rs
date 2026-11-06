//! The actor: its inputs, its state, and the loop that serves them one at a time.
//!
//! Three inputs: commands from handles, messages from Codex, and completions of the work the actor
//! started (a request to Codex, a deadline). Each handler runs to its end before the next input,
//! so no two handlers see a thread half changed. A handler awaits only the store, never Codex.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

use kratos_codex_client::{Codex, Incoming, RequestError, types};
use kratos_protocol::{
    AccessLimit, ApprovalDecision, ApprovalId, ApprovalOutcome, CommandResult, ContinuationId,
    Message, MessageId, MessageInput, MessageOrigin, MessageStatus, QuestionId, QuestionOutcome,
    Skill, ThreadId, ThreadSettings,
};
use kratos_store::{StoreError, ThreadWrite};
use tokio::sync::{mpsc, oneshot};

use super::approvals::Requests;
use super::connection::StartError;
use super::threads::ThreadState;
use super::turns::{Delivery, Stopper};
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
    Access {
        thread_id: ThreadId,
        reply: Reply<AccessLimit>,
    },
    Decide {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        reply: Reply<ApprovalOutcome>,
    },
    Answer {
        question_id: QuestionId,
        answers: BTreeMap<String, Vec<String>>,
        limit: Option<AccessLimit>,
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

impl Command {
    pub(super) fn fail_deferred(self, error: ProviderError) {
        match self {
            Command::Discard { reply, .. } => drop(reply.send(Err(error))),
            Command::RefreshUsage { reply } => drop(reply.send(Err(error))),
            Command::Skills { reply, .. } => drop(reply.send(Err(error))),
            Command::Run { reply, .. } => drop(reply.send(Err(error))),
            _ => {
                unreachable!("only discard, refresh usage, skills and run commands wait for Codex")
            }
        }
    }
}

/// Finished work that changes actor state, fenced by its process generation in `Completion`.
pub(super) enum Done {
    Connected {
        result: Result<(Codex, mpsc::Receiver<Incoming>), StartError>,
    },
    Account {
        plan: Option<String>,
    },
    Attached {
        thread_id: ThreadId,
        /// The context limit the thread was loaded with (D76).
        context_limit: Option<u64>,
        access: AccessLimit,
        result: Result<Opened, RequestError>,
    },
    StartAnswered {
        thread_id: ThreadId,
        result: Result<Box<types::Turn>, RequestError>,
    },
    /// The history read after an uncertain `turn/start` or `turn/steer`: the turn that took the
    /// message, if any.
    DeliveryChecked {
        thread_id: ThreadId,
        message_id: MessageId,
        delivery: Delivery,
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

/// Finished work tagged with the server generation that started it.
pub(super) struct Completion {
    generation: u64,
    done: Done,
}

impl Completion {
    async fn apply(self, actor: &mut Actor) {
        if self.generation == actor.generation {
            actor.on_done(self.done).await;
        } else {
            self.done.discard_stale().await;
        }
    }
}

fn process_exited() -> ProviderError {
    ProviderError::Unavailable("The Codex process that handled this request has exited".into())
}

impl Done {
    async fn discard_stale(self) {
        match self {
            Done::Connected {
                result: Ok((codex, _)),
            } => codex.close().await,
            Done::HistoryForDiscard { reply, .. } => drop(reply.send(Err(process_exited()))),
            Done::UsageRead {
                reply: Some(reply), ..
            } => drop(reply.send(Err(process_exited()))),
            Done::Interrupted { stopper, .. } => stopper.tell(|| Ok(())),
            Done::Connected { result: Err(_) }
            | Done::UsageRead { reply: None, .. }
            | Done::Account { .. }
            | Done::Attached { .. }
            | Done::StartAnswered { .. }
            | Done::DeliveryChecked { .. }
            | Done::Steered { .. }
            | Done::StopExpired { .. }
            | Done::Deadline { .. } => {}
        }
    }
}

/// A thread Codex loaded, and whether it is new.
pub(super) struct Opened {
    pub native_id: String,
    pub model: String,
    pub turns: Vec<types::Turn>,
    pub is_new: bool,
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
    /// Advances on each app-server start and exit; `Completion::apply` drops work from an older
    /// generation, because the process that was to answer it is gone.
    pub(super) generation: u64,
    pub(super) restarts: super::connection::Restarts,
    /// Each model's own context window, as Codex reported it with no setting of Kratos's in force
    /// (D76).
    pub(super) windows: HashMap<String, u64>,
    /// The plan that pays for the running server's account (D24).
    pub(super) plan: Option<String>,
    pub(super) threads: HashMap<ThreadId, ThreadState>,
    pub(super) requests: Requests,
    /// Threads waiting for a start slot, oldest first.
    pub(super) ready: VecDeque<ThreadId>,
    pub(super) deferred: Vec<Command>,
    /// Threads to load once the app-server is up, so its history settles their uncertain messages.
    pub(super) reconcile: Vec<ThreadId>,
    pub(super) done: mpsc::UnboundedSender<Completion>,
    pub(super) completions: mpsc::UnboundedReceiver<Completion>,
    /// Where usage-limit stops go: the scheduler owns continuation (D39).
    pub(super) limit_stops: mpsc::UnboundedSender<crate::usage::LimitStop>,
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
                Some(done) = self.completions.recv() => done.apply(&mut self).await,
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
        let generation = self.generation;
        tokio::spawn(async move {
            // The actor holds the receiver for as long as it runs.
            let _ = done.send(Completion {
                generation,
                done: work.await,
            });
        });
    }

    async fn on_done(&mut self, done: Done) {
        match done {
            Done::Connected { result } => self.on_connected(result).await,
            Done::Account { plan } => self.plan = plan,
            Done::Attached {
                thread_id,
                context_limit,
                access,
                result,
            } => {
                self.on_attached(thread_id, context_limit, access, result)
                    .await
            }
            Done::StartAnswered { thread_id, result } => {
                self.on_start_answered(thread_id, result).await
            }
            Done::DeliveryChecked {
                thread_id,
                message_id,
                delivery,
                error,
                found,
            } => {
                self.on_delivery_checked(thread_id, message_id, delivery, error, found)
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

    pub(super) async fn set_message_status(
        &mut self,
        thread_id: &ThreadId,
        message_id: &MessageId,
        status: MessageStatus,
    ) -> Result<Message, ProviderError> {
        let message_id = message_id.clone();
        self.write(thread_id, move |write| {
            write.set_message_status(&message_id, status)
        })
        .await
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
            Err(error) if error.is_refusal() => Err(ProviderError::Store(error)),
            Err(error) => {
                eprintln!("cannot record Codex work, so Codex stops: {error}");
                self.stop_server(&format!("Kratos could not record Codex's work: {error}"))
                    .await;
                Err(ProviderError::Store(error))
            }
        }
    }

    async fn shutdown(&mut self) {
        self.stop_server("Kratos is shutting down").await;
    }
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

#[cfg(test)]
mod tests;
