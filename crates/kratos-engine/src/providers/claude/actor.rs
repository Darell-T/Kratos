//! The actor: its inputs, its state, and the loop that serves them one at a time.
//!
//! Three inputs: commands from handles, frames from the CLI processes, and completions of the work
//! the actor started (opening a process, writing a message, a timer). Each handler runs to its
//! end before the next input, so no two handlers see a thread half changed. A handler awaits only
//! the store, never a CLI.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::time::Duration;

use kratos_claude_client::{
    Claude, ConnectError, DeliveredMessages, Incoming, Initialized, RequestError, TranscriptError,
    UsageRead,
};
use kratos_protocol::{
    AccessLimit, ApprovalDecision, ApprovalId, ApprovalOutcome, ContinuationId, Message, MessageId,
    MessageInput, MessageOrigin, QuestionId, QuestionOutcome, ThreadId, ThreadSettings,
};
use kratos_store::{StoreError, ThreadWrite};
use tokio::sync::{mpsc, oneshot};

use super::recovery::Settling;
use super::threads::{Session, SessionConfig, ThreadState, Waiter};
use super::{ClaudeSettings, Continued, ProviderError, Sent};
use crate::hub::Hub;
use crate::store_thread::StoreThread;
use crate::usage::{LimitStop, UsageTracker};

/// How many frames from the CLI processes wait for the actor. A full queue stops the readers,
/// which stops the CLIs writing, so a slow actor slows Claude instead of growing without bound.
const QUEUED_FRAMES: usize = 256;
/// How often the actor looks for sessions idle past the idle window, so one closes at most this
/// long after its window ends.
const IDLE_CHECK_EVERY: Duration = Duration::from_secs(60);

pub(super) type Reply<T> = oneshot::Sender<Result<T, ProviderError>>;

pub(super) enum Command {
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
    Continue {
        thread_id: ThreadId,
        continuation_id: ContinuationId,
        reply: Reply<Continued>,
    },
    /// Runs `waiter` once the thread's CLI is up, starting one when none runs.
    WhenOpen {
        thread_id: ThreadId,
        waiter: Waiter,
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
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// A frame from a CLI process, with the session that read it.
pub(super) struct SessionEvent {
    pub thread_id: ThreadId,
    pub epoch: u64,
    pub incoming: Incoming,
}

pub(super) struct Connected {
    pub claude: Claude,
    pub incoming: mpsc::Receiver<Incoming>,
    pub initialized: Initialized,
}

/// What a transcript was read for.
pub(super) enum TranscriptWork {
    /// A process ended with messages in flight.
    Settle(Settling),
    /// The user discards a message whose delivery is uncertain.
    Discard {
        message_id: MessageId,
        reply: Reply<Message>,
    },
}

/// Finished work that changes actor state. Work for a session that has ended carries its `epoch`
/// and is dropped.
pub(super) enum Done {
    CommandFinished {
        thread_id: ThreadId,
        epoch: u64,
        control_id: String,
    },
    Opened {
        thread_id: ThreadId,
        epoch: u64,
        config: SessionConfig,
        result: Result<Box<Connected>, ConnectError>,
    },
    Sent {
        thread_id: ThreadId,
        epoch: u64,
        message_id: MessageId,
        result: Result<(), RequestError>,
    },
    Interrupted {
        thread_id: ThreadId,
        epoch: u64,
        stop_epoch: u64,
        result: Result<(), RequestError>,
    },
    StopExpired {
        thread_id: ThreadId,
        stop_epoch: u64,
    },
    /// The wait after a CLI failure ended.
    RestartDue { thread_id: ThreadId },
    Transcript {
        thread_id: ThreadId,
        work: TranscriptWork,
        result: Result<DeliveredMessages, TranscriptError>,
    },
    /// A `get_usage` read finished. The account's usage outlives the session that read it, so the
    /// answer needs no epoch; a read of an account that has since changed is dropped by its
    /// generation.
    UsageRead {
        generation: u64,
        plan: Option<String>,
        result: Result<UsageRead, RequestError>,
        reply: Option<Reply<()>>,
    },
}

pub(crate) struct Actor {
    pub(super) requests: super::requests::Requests,
    pub(super) settings: ClaudeSettings,
    pub(super) hub: Hub,
    pub(super) store: StoreThread,
    pub(super) usage: UsageTracker,
    /// The plan of the session that initialized last, which is the account that is signed in.
    /// `None` for an account with no plan. Every read and event comes from a session that
    /// initialized, so it is never read before a session has set it.
    pub(super) signed_in: Option<String>,
    pub(super) limit_stops: mpsc::UnboundedSender<LimitStop>,
    pub(super) threads: HashMap<ThreadId, ThreadState>,
    /// Set when a write failed: no CLI runs after that, because its work would go unrecorded (L2).
    pub(super) parked: Option<String>,
    pub(super) done: mpsc::UnboundedSender<Done>,
    completions: mpsc::UnboundedReceiver<Done>,
    pub(super) frames: mpsc::Sender<SessionEvent>,
    incoming: mpsc::Receiver<SessionEvent>,
}

impl Actor {
    pub(super) fn new(
        settings: ClaudeSettings,
        hub: Hub,
        store: StoreThread,
        usage: UsageTracker,
        limit_stops: mpsc::UnboundedSender<LimitStop>,
    ) -> Actor {
        let (done, completions) = mpsc::unbounded_channel();
        let (frames, incoming) = mpsc::channel(QUEUED_FRAMES);
        Actor {
            requests: super::requests::Requests::default(),
            settings,
            hub,
            store,
            usage,
            signed_in: None,
            limit_stops,
            threads: HashMap::new(),
            parked: None,
            done,
            completions,
            frames,
            incoming,
        }
    }

    pub(super) async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let mut idle_check = tokio::time::interval(self.settings.idle_close.min(IDLE_CHECK_EVERY));
        idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                Some(event) = self.incoming.recv() => self.on_event(event).await,
                _ = idle_check.tick() => self.close_idle_sessions(),
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

    // #lizard forgives: one arm per kind of completion, each a call.
    async fn on_done(&mut self, done: Done) {
        match done {
            Done::CommandFinished {
                thread_id,
                epoch,
                control_id,
            } => {
                if let Some(thread) = self.threads.get_mut(&thread_id)
                    && thread.epoch == epoch
                {
                    thread.commands.remove(&control_id);
                }
            }
            Done::Opened {
                thread_id,
                epoch,
                config,
                result,
            } => self.on_opened(thread_id, epoch, config, result).await,
            Done::Sent {
                thread_id,
                epoch,
                message_id,
                result,
            } => self.on_sent(thread_id, epoch, message_id, result).await,
            Done::Interrupted {
                thread_id,
                epoch,
                stop_epoch,
                result,
            } => {
                self.on_interrupted(thread_id, epoch, stop_epoch, result)
                    .await
            }
            Done::StopExpired {
                thread_id,
                stop_epoch,
            } => self.on_stop_expired(&thread_id, stop_epoch),
            Done::RestartDue { thread_id } => self.on_restart_due(thread_id).await,
            Done::Transcript {
                thread_id,
                work,
                result,
            } => self.on_transcript(thread_id, work, result).await,
            Done::UsageRead {
                generation,
                plan,
                result,
                reply,
            } => self.on_usage_read(generation, plan, result, reply),
        }
    }

    /// Runs `job` as one write to the thread's records. A write that fails parks the provider
    /// rather than let Claude run unrecorded (L2), and the caller gets the error.
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
                let reason = format!("Kratos could not record Claude's work: {error}");
                eprintln!("{reason}, so Claude stops");
                self.park(reason);
                Err(ProviderError::Store(error))
            }
        }
    }

    /// Ends every CLI and refuses new work, because the records can no longer keep up with Claude.
    /// Messages in flight stay as they are; the next engine start settles them from the
    /// transcript (L1).
    fn park(&mut self, reason: String) {
        if self.parked.is_some() {
            return;
        }
        for thread in self.threads.values_mut() {
            thread.cancel_commands(None);
            thread.epoch += 1;
            thread.restarts.cancel_timer();
            if let Session::Open(open) = std::mem::replace(&mut thread.session, Session::Closed) {
                tokio::spawn(async move { open.claude.close().await });
            }
            thread.pending.clear();
            thread.run = None;
            for waiter in std::mem::take(&mut thread.waiters) {
                waiter.fail(&reason);
            }
            thread.stopping = false;
            for waiter in std::mem::take(&mut thread.stop_waiters) {
                let _ = waiter.send(Err(ProviderError::Unavailable(reason.clone())));
            }
        }
        self.parked = Some(reason);
    }

    /// The error that refuses work while the provider is parked.
    pub(super) fn running(&self) -> Result<(), ProviderError> {
        match &self.parked {
            Some(reason) => Err(ProviderError::Unavailable(reason.clone())),
            None => Ok(()),
        }
    }

    async fn shutdown(&mut self) {
        for id in self.threads.keys().cloned().collect::<Vec<_>>() {
            self.close_requests(&id, None, "Kratos is shutting down")
                .await;
        }
        let mut closing = Vec::new();
        for thread in self.threads.values_mut() {
            thread.epoch += 1;
            thread.restarts.cancel_timer();
            if let Session::Open(open) = std::mem::replace(&mut thread.session, Session::Closed) {
                closing.push(tokio::spawn(async move { open.claude.close().await }));
            }
        }
        for closed in closing {
            let _ = closed.await;
        }
    }
}
