use serde::{Deserialize, Serialize};

use crate::{
    AgentItem, Approval, ApprovalDecision, ApprovalId, ContextUsage, Continuation, MessageId,
    MessageInput, MessageOrigin, MessageStatus, Provider, Question, QuestionId, RequiredText,
    ThreadId, ThreadSettings, Timestamp, TurnChanges, TurnId, TurnOutcome,
};

/// A position in one thread's event log.
///
/// A thread's events count up from 1 with no gaps, and a number, once committed, never names
/// another event. A client that has seen event `n` resumes with `after: n`. The cursor 0 means
/// "before the first event", so `after: 0` replays the whole log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(u64);

impl Seq {
    pub const BEFORE_FIRST: Seq = Seq(0);

    pub fn new(position: u64) -> Seq {
        Seq(position)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// One thing that happened in a thread, as the thread's log keeps it. The `kind` names are part of
/// the stored format: renaming one leaves older rows unreadable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ThreadEventBody {
    /// A message to a thread whose provider the engine does not drive yet. It reaches no agent.
    #[serde(rename = "user.message")]
    UserMessage { text: String },
    #[serde(rename = "message.queued", rename_all = "camelCase")]
    MessageQueued {
        message_id: MessageId,
        input: Vec<MessageInput>,
        #[serde(flatten)]
        origin: MessageOrigin,
    },
    #[serde(rename = "message.status", rename_all = "camelCase")]
    MessageStatusChanged {
        message_id: MessageId,
        #[serde(flatten)]
        status: MessageStatus,
    },
    #[serde(rename = "turn.started", rename_all = "camelCase")]
    TurnStarted { turn_id: TurnId },
    #[serde(rename = "item.completed", rename_all = "camelCase")]
    ItemCompleted { turn_id: TurnId, item: AgentItem },
    #[serde(rename = "turn.completed", rename_all = "camelCase")]
    TurnCompleted {
        turn_id: TurnId,
        #[serde(flatten)]
        outcome: TurnOutcome,
        changes: TurnChanges,
        context: Option<ContextUsage>,
    },
    #[serde(rename = "approval.requested")]
    ApprovalRequested { approval: Approval },
    #[serde(rename = "approval.decided", rename_all = "camelCase")]
    ApprovalDecided {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    },
    /// The request ended without a decision, for example because its turn ended.
    #[serde(rename = "approval.closed", rename_all = "camelCase")]
    ApprovalClosed {
        approval_id: ApprovalId,
        reason: String,
    },
    #[serde(rename = "question.asked")]
    QuestionAsked { question: Question },
    #[serde(rename = "question.answered", rename_all = "camelCase")]
    QuestionAnswered { question_id: QuestionId },
    #[serde(rename = "question.closed", rename_all = "camelCase")]
    QuestionClosed {
        question_id: QuestionId,
        reason: String,
    },
    /// The thread's queue waits for the user: nothing more starts until they act.
    #[serde(rename = "thread.paused")]
    ThreadPaused {
        #[serde(flatten)]
        reason: PauseReason,
    },
    #[serde(rename = "thread.resumed")]
    ThreadResumed,
    #[serde(rename = "settings.changed")]
    SettingsChanged { settings: ThreadSettings },
    /// The provider reported trouble in a turn. `will_retry` is the provider retrying on its own.
    #[serde(rename = "provider.error", rename_all = "camelCase")]
    ProviderError {
        turn_id: TurnId,
        message: String,
        will_retry: bool,
    },
    /// The scheduler planned, changed, or dropped the continuation of a turn that stopped at a usage
    /// limit (D39).
    #[serde(rename = "continuation.changed")]
    ContinuationChanged { continuation: Continuation },
    /// A provider session began serving the thread, and the plan that pays for it (D24).
    #[serde(rename = "provider.session")]
    ProviderSession {
        provider: Provider,
        plan: Option<String>,
    },
}

/// Why a thread's queue waits for the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "reason",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PauseReason {
    /// The last turn failed, so the messages behind it may no longer make sense.
    TurnFailed { turn_id: TurnId, error: String },
    /// Kratos stopped a turn at its deadline.
    TimedOut { turn_id: TurnId },
    /// The last turn did not finish before Kratos or the provider stopped.
    Interrupted,
    /// The provider refused a message before acting on it. The user may send it again.
    Refused {
        message_id: MessageId,
        error: String,
    },
    /// Kratos cannot tell whether the provider acted on a message. Only the user can discard it.
    DeliveryUncertain {
        message_id: MessageId,
        error: String,
    },
    /// The provider's session cannot serve the thread, for example after a crash.
    Unavailable { error: String },
    /// The turn stopped at the account's usage limit. Kratos's scheduler resumes the work once the
    /// provider says the account may run again (D39); `resets_at` is when it will ask.
    UsageLimit {
        turn_id: TurnId,
        resets_at: Option<Timestamp>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadEvent {
    pub thread_id: ThreadId,
    pub seq: Seq,
    /// When the engine committed the event. Read inside the append's transaction, so `at` never
    /// decreases along `seq` unless the wall clock steps back.
    pub at: Timestamp,
    pub event: ThreadEventBody,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PostMessage {
    pub text: RequiredText,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    /// Sends the thread's events after `after`, in order, then each new one as it is appended.
    /// Subscribing again to the same thread on the same socket replaces the earlier subscription.
    Subscribe {
        thread_id: ThreadId,
        after: Seq,
    },
    Unsubscribe {
        thread_id: ThreadId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    Event {
        event: ThreadEvent,
    },
    Error {
        message: String,
    },
    /// Progress the log does not keep, such as a reply as it is written. A client that misses some
    /// gets a `snapshot` instead, and the finished item arrives as an `item.completed` event.
    Live {
        thread_id: ThreadId,
        update: LiveUpdate,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LiveUpdate {
    /// More text of a reply the agent is writing.
    ReplyDelta {
        turn_id: TurnId,
        item_id: String,
        delta: String,
    },
    /// Every reply of the running turn so far, sent when a client subscribes or falls behind.
    Snapshot {
        turn_id: Option<TurnId>,
        replies: Vec<ReplySoFar>,
    },
    /// An item the agent started, such as a command now running.
    ItemStarted { turn_id: TurnId, item: AgentItem },
    Context {
        turn_id: TurnId,
        usage: ContextUsage,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplySoFar {
    pub item_id: String,
    pub text: String,
}
