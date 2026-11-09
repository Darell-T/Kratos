//! Claude threads.
//!
//! One actor task owns every Claude thread's state, as the Codex actor does, but each thread has a
//! `claude` process of its own instead of sharing one server. Handles send the actor commands and
//! wait for the reply. Work that waits on a process, such as starting it or writing a message,
//! runs in a task of its own and comes back to the actor as a completion, so the actor keeps
//! reading frames while it is out, and a completion for a process that has ended is recognized and
//! dropped. Nothing outside the actor changes a Claude thread's state.
//!
//! The two delivery rules hold here (D11). A queued message is written to Claude only once every
//! message before it has a result, and a message whose delivery is uncertain is never sent again:
//! only Claude's transcript or the user settles it. Claude accepts a message only when it echoes
//! it back, and a turn ends only on its result.
//!
//! Kratos decides each tool request and records approvals and questions through the shared Hub.
//! Every command uses the Codex runner. No thread launches in a mode that skips the ask.

mod actor;
mod commands;
mod hooks;
mod incoming;
mod paths;
mod permissions;
mod policy;
mod queue;
mod recovery;
mod requests;
mod session;
mod settings;
mod threads;
mod tools;
mod turns;
mod wire;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use kratos_protocol::{
    AccessLimit, ApprovalDecision, ApprovalId, ApprovalOutcome, ContinuationId, Message, MessageId,
    MessageInput, MessageOrigin, QuestionId, QuestionOutcome, ThreadId, ThreadSettings,
};
use kratos_store::StoreError;
use tokio::sync::{mpsc, oneshot};

use crate::hub::Hub;
use crate::providers::{Continued, ProviderError, Sent};
use crate::store_thread::StoreThread;

use crate::providers::MAX_PENDING_MESSAGES;

pub use wire::native_id;

/// How the provider starts Claude and how long it waits on it.
#[derive(Clone, Debug)]
pub struct ClaudeSettings {
    /// The command and permission boundary. Without it, every tool request is refused.
    pub tools: Option<ClaudeTools>,
    /// The native `claude` executable, found as the user's shell finds it (L8).
    pub program: PathBuf,
    /// The user's home folder, under which Claude keeps each session's transcript.
    pub claude_home: PathBuf,
    /// Variables set for the CLI after Kratos's own and the provider's are removed.
    pub env_set: Vec<(OsString, OsString)>,
    /// A turn that runs this long is stopped (TypeScript: 10 minutes).
    pub turn_timeout: Duration,
    /// How long a stopped turn may take to end before Kratos closes the CLI.
    pub stop_timeout: Duration,
    /// The deadline of a control request to the CLI, such as an interrupt.
    pub control_timeout: Duration,
    /// The wait after a CLI's first failure to run. Each failure in a row doubles it (L4).
    pub restart_wait: Duration,
    /// Failures in a row after which only the user may start the CLI again (L4).
    pub max_restarts: u32,
}

/// Dependencies that let Claude execute tools under Kratos's policy.
#[derive(Clone)]
pub struct ClaudeTools {
    pub runner: super::codex::CodexProvider,
    pub protected: super::codex::ProtectedFolders,
}

impl std::fmt::Debug for ClaudeTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClaudeTools")
            .field("protected", &self.protected)
            .finish_non_exhaustive()
    }
}

/// Commands that may wait for the actor. A burst of requests waits for room instead of growing
/// without bound.
const QUEUED_COMMANDS: usize = 64;

/// The handle to the Claude actor. Clones share it.
#[derive(Clone)]
pub struct ClaudeProvider {
    commands: mpsc::Sender<actor::Command>,
}

/// Starts the actor. It first settles what an earlier engine left unfinished (L1): turns shown
/// running end as interrupted, and messages being sent are settled from Claude's transcripts.
/// A CLI starts only when a thread's message needs one (L5).
pub async fn start(
    settings: ClaudeSettings,
    hub: Hub,
    store: StoreThread,
) -> Result<ClaudeProvider, StoreError> {
    let (commands, inbox) = mpsc::channel(QUEUED_COMMANDS);
    let actor = actor::Actor::recover(settings, hub, store).await?;
    tokio::spawn(actor.run(inbox));
    Ok(ClaudeProvider { commands })
}

impl ClaudeProvider {
    /// Records the first decision and returns the recorded outcome on every later attempt (D83).
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

    /// Answers a question once, subject to the acting thread's access ceiling.
    pub async fn answer(
        &self,
        question_id: QuestionId,
        answers: BTreeMap<String, Vec<String>>,
        limit: Option<AccessLimit>,
    ) -> Result<QuestionOutcome, ProviderError> {
        self.ask(|reply| actor::Command::Answer {
            question_id,
            answers,
            limit,
            reply,
        })
        .await
    }
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

    /// Queues a message, then writes it to Claude when the thread is free (D85). A message id the
    /// client already sent returns that message.
    pub async fn send(
        &self,
        thread_id: ThreadId,
        message_id: MessageId,
        input: Vec<MessageInput>,
        origin: MessageOrigin,
    ) -> Result<Sent, ProviderError> {
        self.ask(|reply| actor::Command::Send {
            thread_id,
            message_id,
            input,
            origin,
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

    /// Queues a message Claude never received before acting on it again.
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

    /// Settles a message whose delivery is uncertain: accepted when Claude's transcript has it,
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

    /// Stops the running turn. Returns once Claude acknowledged the interrupt or the stopped turn
    /// ended; the turn's end arrives as an event.
    pub async fn stop(&self, thread_id: ThreadId) -> Result<(), ProviderError> {
        self.ask(|reply| actor::Command::Stop { thread_id, reply })
            .await
    }

    /// Lets a paused queue run again, and a CLI that kept failing start once more. Refused while a
    /// message failed or its delivery is uncertain.
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

    /// Refuses settings a new thread could not run with, by the checks [`Self::set_settings`]
    /// applies.
    pub(crate) async fn check_settings(
        &self,
        settings: ThreadSettings,
    ) -> Result<(), ProviderError> {
        self.ask(|reply| actor::Command::CheckSettings { settings, reply })
            .await
    }

    /// Queues a continuation's message ahead of the thread's waiting messages and lets the queue
    /// run (D39). Called by the usage scheduler only.
    pub(crate) async fn continue_thread(
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

    /// Reads the account's usage through a session of the thread.
    pub(crate) async fn refresh_usage(&self, thread_id: ThreadId) -> Result<(), ProviderError> {
        let _ = thread_id;
        Err(ProviderError::Unavailable(
            "Kratos does not read Claude's usage yet".into(),
        ))
    }

    /// The access the thread's settings give its agent.
    pub async fn access(&self, thread_id: ThreadId) -> Result<AccessLimit, ProviderError> {
        self.ask(|reply| actor::Command::Access { thread_id, reply })
            .await
    }

    /// Closes every CLI and ends the actor.
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
