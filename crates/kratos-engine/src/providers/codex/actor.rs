use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use kratos_codex_client::{Codex, Incoming, RequestError, types};
use kratos_protocol::{
    AccessLimit, ApprovalDecision, ApprovalId, ApprovalOutcome, CommandResult, ContextLimit,
    ContinuationId, Message, MessageId, MessageInput, MessageOrigin, MessageStatus, QuestionId,
    QuestionOutcome, Skill, ThreadId, ThreadSettings,
};
use kratos_store::{StoreError, ThreadWrite};
use tokio::sync::{mpsc, oneshot};

use super::approvals::Requests;
use super::connection::StartError;
use super::servers::{ServerKey, Servers};
use super::threads::ThreadState;
use super::turns::{Delivery, StopWaiters};
use super::{CodexSettings, Continued, ProviderError, Sent};
use crate::hub::Hub;
use crate::store_thread::StoreThread;
use crate::usage::UsageTracker;

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
    Quiet {
        quiet: Result<bool, String>,
    },
    Attached {
        thread_id: ThreadId,
        /// The context limit the thread was loaded with (D76).
        context_limit: Option<ContextLimit>,
        access: AccessLimit,
        result: Result<Opened, RequestError>,
    },
    StartAnswered {
        thread_id: ThreadId,
        message_id: MessageId,
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
        waiters: StopWaiters,
    },
    StopExpired {
        thread_id: ThreadId,
        stop_id: uuid::Uuid,
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

pub(super) struct Completion {
    key: ServerKey,
    generation: u64,
    done: Done,
}

impl Completion {
    async fn apply(self, actor: &mut Actor) {
        if self.generation == actor.servers.generation(&self.key) {
            actor.on_done(self.key, self.done).await;
        } else {
            self.done.discard_stale().await;
        }
    }
}

pub(super) struct Frame {
    pub key: ServerKey,
    pub generation: u64,
    pub incoming: Incoming,
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
            Done::Interrupted { waiters, .. } => waiters.tell(|| Ok(())),
            Done::Connected { result: Err(_) }
            | Done::UsageRead { reply: None, .. }
            | Done::Account { .. }
            | Done::Quiet { .. }
            | Done::Attached { .. }
            | Done::StartAnswered { .. }
            | Done::DeliveryChecked { .. }
            | Done::Steered { .. }
            | Done::StopExpired { .. } => {}
        }
    }
}

pub(super) struct Opened {
    pub native_id: String,
    pub model: String,
    pub turns: Vec<types::Turn>,
    pub is_new: bool,
}

pub(crate) struct Actor {
    pub(super) settings: CodexSettings,
    pub(super) hub: Hub,
    pub(super) store: StoreThread,
    pub(super) usage: UsageTracker,
    pub(super) servers: Servers,
    pub(super) frames: mpsc::Receiver<Frame>,
    pub(super) frame_sender: mpsc::Sender<Frame>,
    /// Each model's own context window, as Codex reported it with no setting of Kratos's in force
    /// (D76).
    pub(super) windows: HashMap<String, u64>,
    /// The plan that pays for the running server's account (D24).
    pub(super) plan: Option<String>,
    pub(super) threads: HashMap<ThreadId, ThreadState>,
    pub(super) requests: Requests,
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
        let mut idle_check = tokio::time::interval(IDLE_CHECK);
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
                Some(frame) = self.frames.recv() => {
                    if frame.generation == self.servers.generation(&frame.key) {
                        self.on_incoming(frame.key, frame.incoming).await;
                    }
                }
                _ = idle_check.tick() => self.retire_idle(Instant::now()),
            }
        }
    }

    pub(super) fn spawn<F>(&self, key: &ServerKey, work: F)
    where
        F: Future<Output = Done> + Send + 'static,
    {
        let done = self.done.clone();
        let key = key.clone();
        let generation = self.servers.generation(&key);
        tokio::spawn(async move {
            // The actor holds the receiver for as long as it runs.
            let _ = done.send(Completion {
                key,
                generation,
                done: work.await,
            });
        });
    }

    // #lizard forgives: one arm per kind of completion, each a call.
    async fn on_done(&mut self, key: ServerKey, done: Done) {
        match done {
            Done::Connected { result } => self.on_connected(key, result).await,
            Done::Account { plan } => self.plan = plan,
            Done::Quiet { quiet } => self.on_quiet(key, quiet).await,
            Done::Attached {
                thread_id,
                context_limit,
                access,
                result,
            } => {
                self.on_attached(thread_id, context_limit, access, result)
                    .await
            }
            Done::StartAnswered {
                thread_id,
                message_id,
                result,
            } => self.on_start_answered(thread_id, message_id, result).await,
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
                waiters,
            } => {
                if let Err(error) = result {
                    eprintln!(
                        "Codex did not answer Stop for {thread_id:?} turn {turn_id}: {error}"
                    );
                }
                waiters.tell(|| Ok(()));
            }
            Done::StopExpired { thread_id, stop_id } => self.on_stop_expired(&thread_id, stop_id),
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
                eprintln!("cannot record Codex work, so the thread's Codex stops: {error}");
                let key = ServerKey::Thread(thread_id.clone());
                self.stop_server(
                    &key,
                    &format!("Kratos could not record Codex's work: {error}"),
                )
                .await;
                Err(ProviderError::Store(error))
            }
        }
    }

    async fn shutdown(&mut self) {
        self.close_all().await;
    }
}

const IDLE_CHECK: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests;
