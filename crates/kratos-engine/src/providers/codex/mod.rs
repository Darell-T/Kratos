//! Codex threads.
//!
//! One actor task owns the `codex app-server` connection and every Codex thread's turn state.
//! Handles send it commands and wait for the reply. A request to Codex runs in a task of its own
//! and comes back to the actor as a completion, so the actor keeps reading Codex's notifications
//! while a request is out, and a completion that arrives after its thread moved on is recognized
//! and dropped. Nothing outside the actor changes a Codex thread's state.
//!
//! The two delivery rules of the TypeScript provider hold here (D11). A queued message starts only
//! once the turn before it is confirmed finished, and a message whose delivery is uncertain is
//! never sent again: only the provider's history or the user settles it.

mod actor;
mod commands;
mod connection;
mod incoming;
pub mod policy;
pub mod profiles;
mod recovery;
mod requests;
#[cfg(windows)]
mod sandbox;
mod threads;
mod turns;
pub mod wire;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use kratos_command_safety::DangerousCommandPlatform;
use kratos_protocol::{
    ApprovalDecision, ApprovalId, ApprovalOutcome, CommandResult, ContinuationId, Message,
    MessageId, MessageInput, QuestionId, QuestionOutcome, Skill, ThreadId, ThreadSettings,
};
use kratos_store::StoreError;
use tokio::sync::{mpsc, oneshot};

use crate::hub::Hub;
use crate::store_thread::StoreThread;
use crate::usage::UsageTracker;
use crate::usage::scheduler::LimitStop;

pub use profiles::ProtectedFolders;

/// The most undelivered messages one thread holds (D85).
pub const MAX_PENDING_MESSAGES: usize = 10;

/// How the provider starts Codex and how long it waits on it.
#[derive(Clone, Debug)]
pub struct CodexSettings {
    /// The Codex binary, `codex.exe` on Windows, never the npm shim (D2).
    pub program: PathBuf,
    /// Arguments placed before Kratos's own, such as a test's `-c` model provider settings.
    pub extra_args: Vec<std::ffi::OsString>,
    /// Variables set for the app-server, such as `CODEX_HOME` in tests.
    pub env_set: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// The app-server's own working folder, empty and Kratos's, so a legacy policy that makes the
    /// server's folder writable exposes nothing.
    pub server_folder: PathBuf,
    pub protected: ProtectedFolders,
    /// A turn that runs this long without waiting on the user is stopped (TypeScript: 10 minutes).
    pub turn_timeout: Duration,
    /// How long a stopped turn may take to end before Kratos closes the app-server.
    pub stop_timeout: Duration,
    /// The deadline of an ordinary request to Codex.
    pub request_timeout: Duration,
    pub platform: DangerousCommandPlatform,
}

/// What became of a continuation the scheduler sent (D39).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Continued {
    Queued,
    /// The continuation was cancelled or sent before; there is nothing to send.
    NotOpen,
    /// The thread cannot continue on its own, and the text says why: the user decides.
    NeedsUser(String),
}

/// What became of a message sent to a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sent {
    Queued(Message),
    /// The client sent an id the thread already has; this is that message.
    Existing(Message),
    /// The thread's queue is full; nothing was stored.
    Full {
        pending: usize,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Unknown thread {0}")]
    UnknownThread(ThreadId),
    #[error("Unknown message {0}")]
    UnknownMessage(MessageId),
    #[error("Unknown approval {0}")]
    UnknownApproval(ApprovalId),
    #[error("Unknown question {0}")]
    UnknownQuestion(QuestionId),
    #[error("Unknown continuation {0}")]
    UnknownContinuation(ContinuationId),
    /// The request does not fit the thread's state, for example a retry of a message that did not
    /// fail. The text says what would.
    #[error("{0}")]
    Conflict(String),
    /// The setting is not one the pinned Codex supports, or is out of its reported bounds (D76).
    #[error("{0}")]
    Unsupported(String),
    /// Codex cannot run: it is missing, failed to start, or keeps exiting.
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("The Codex provider has stopped")]
    Stopped,
}

/// The handle to the Codex actor. Clones share it.
#[derive(Clone)]
pub struct CodexProvider {
    commands: mpsc::Sender<actor::Command>,
}

/// Commands that may wait for the actor. A burst of requests waits for room instead of growing
/// without bound.
const QUEUED_COMMANDS: usize = 64;

/// Starts the actor. It first settles what an earlier engine left unfinished (L1): turns shown
/// running end as interrupted, messages being sent become uncertain, and waiting approvals and
/// questions close. Codex itself starts only when a thread needs it (L5), or in the background to
/// let its history settle a message whose delivery the restart left uncertain.
///
/// The receiver hands the scheduler every turn that stops at a usage limit.
pub async fn start(
    settings: CodexSettings,
    hub: Hub,
    store: StoreThread,
    usage: UsageTracker,
) -> Result<(CodexProvider, mpsc::UnboundedReceiver<LimitStop>), StoreError> {
    let (commands, inbox) = mpsc::channel(QUEUED_COMMANDS);
    // Unbounded so the actor never waits on the scheduler, which waits on the actor. A turn stops
    // at a limit at most once, so the channel holds at most one entry per turn.
    let (limit_stops, stops) = mpsc::unbounded_channel();
    let (actor, uncertain) =
        actor::Actor::recover(settings, hub, store, usage, limit_stops).await?;
    tokio::spawn(actor.run(inbox, uncertain));
    Ok((CodexProvider { commands }, stops))
}

impl CodexProvider {
    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, ProviderError>>) -> actor::Command,
    ) -> Result<T, ProviderError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| ProviderError::Stopped)?;
        result.await.map_err(|_| ProviderError::Stopped)?
    }

    /// Queues a message, then starts it when the thread is free (D85). A message id the client
    /// already sent returns that message.
    pub async fn send(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
        input: Vec<MessageInput>,
    ) -> Result<Sent, ProviderError> {
        self.ask(|reply| actor::Command::Send {
            thread_id,
            message_id,
            input,
            reply,
        })
        .await
    }

    /// Adds a queued message to the running turn instead of waiting for it to end.
    pub async fn steer(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        self.ask(|reply| actor::Command::Steer {
            thread_id,
            message_id,
            reply,
        })
        .await
    }

    /// Removes a queued or failed message.
    pub async fn remove(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        self.ask(|reply| actor::Command::Remove {
            thread_id,
            message_id,
            reply,
        })
        .await
    }

    /// Queues a message Codex refused before acting on it again.
    pub async fn retry(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        self.ask(|reply| actor::Command::Retry {
            thread_id,
            message_id,
            reply,
        })
        .await
    }

    /// Settles a message whose delivery is uncertain: accepted when Codex's history has it,
    /// cancelled when it does not. Never sends it again (D11).
    pub async fn discard(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
    ) -> Result<Message, ProviderError> {
        self.ask(|reply| actor::Command::Discard {
            thread_id,
            message_id,
            reply,
        })
        .await
    }

    /// Stops the running turn. Returns once Codex accepted the stop; the turn's end arrives as an
    /// event.
    pub async fn stop(&self, thread_id: ThreadId) -> Result<(), ProviderError> {
        self.ask(|reply| actor::Command::Stop { thread_id, reply })
            .await
    }

    /// Lets a paused queue run again. Refused while a message failed or its delivery is uncertain.
    pub async fn resume(&self, thread_id: ThreadId) -> Result<(), ProviderError> {
        self.ask(|reply| actor::Command::Resume { thread_id, reply })
            .await
    }

    /// Replaces the thread's settings from its next turn (D87).
    pub async fn set_settings(
        &self,
        thread_id: ThreadId,
        settings: ThreadSettings,
    ) -> Result<(), ProviderError> {
        self.ask(|reply| actor::Command::SetSettings {
            thread_id,
            settings,
            reply,
        })
        .await
    }

    /// Records the first decision on an approval and sends it to Codex. A later decision gets the
    /// recorded outcome (D83).
    pub async fn decide(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<ApprovalOutcome, ProviderError> {
        self.ask(|reply| actor::Command::Decide {
            approval_id,
            decision,
            reply,
        })
        .await
    }

    pub async fn answer(
        &self,
        question_id: QuestionId,
        answers: BTreeMap<String, Vec<String>>,
    ) -> Result<QuestionOutcome, ProviderError> {
        self.ask(|reply| actor::Command::Answer {
            question_id,
            answers,
            reply,
        })
        .await
    }

    /// The skills Codex reports for the thread's folder (D97, L11).
    pub async fn skills(&self, thread_id: ThreadId) -> Result<Vec<Skill>, ProviderError> {
        self.ask(|reply| actor::Command::Skills { thread_id, reply })
            .await
    }

    /// Runs `command` in the thread's folder inside the sandbox of the thread's permission, through
    /// Codex's command runner. Used where Kratos itself, not an agent, runs a command (R4, R8).
    pub async fn run_command(
        &self,
        thread_id: ThreadId,
        command: Vec<String>,
        timeout: Duration,
    ) -> Result<CommandResult, ProviderError> {
        self.ask(|reply| actor::Command::Run {
            thread_id,
            command,
            timeout,
            reply,
        })
        .await
    }

    /// Asks Codex for the account's usage now, for the scheduler (D38).
    pub async fn refresh_usage(&self) -> Result<(), ProviderError> {
        self.ask(|reply| actor::Command::RefreshUsage { reply })
            .await
    }

    /// Queues a continuation's message ahead of the thread's waiting messages and lets the queue
    /// run (D39). Called by the usage scheduler only.
    pub async fn continue_thread(
        &self,
        thread_id: ThreadId,
        continuation_id: ContinuationId,
    ) -> Result<Continued, ProviderError> {
        self.ask(|reply| actor::Command::Continue {
            thread_id,
            continuation_id,
            reply,
        })
        .await
    }

    /// Closes Codex and ends the actor. Waiting requests close.
    pub async fn shutdown(&self) {
        let (reply, done) = oneshot::channel();
        if self
            .commands
            .send(actor::Command::Shutdown { reply })
            .await
            .is_ok()
        {
            let _ = done.await;
        }
    }
}
