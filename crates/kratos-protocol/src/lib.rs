//! The Kratos engine's wire types.
//!
//! Every type here serializes to the JSON the TypeScript engine sends and accepts: camelCase
//! fields, `kind` tags on thread events, and `type` tags on socket messages. Parsing is the
//! validation: blank text fails to parse, so a handler never sees it.

mod agents;
mod api;
mod approvals;
mod events;
mod messages;
mod skills;
mod text;
mod threads;
mod time;
mod turns;
mod usage;
mod workspaces;

pub use agents::{CommandResult, OutsideWrites, Permission, Provider, ThreadSandbox, TurnChanges};
pub use api::{ErrorBody, Health};
pub use approvals::{
    AnswerQuestion, Approval, ApprovalDecision, ApprovalId, ApprovalOutcome, ApprovalRequest,
    AskReason, DecideApproval, ExtraAccess, NetworkAccess, PathAccess, PathAccessMode, Question,
    QuestionId, QuestionItem, QuestionOption, QuestionOutcome,
};
pub use events::{
    ClientMessage, LiveUpdate, PauseReason, PostMessage, ReplySoFar, Seq, ServerMessage,
    ThreadEvent, ThreadEventBody,
};
pub use messages::{
    MAX_MESSAGE_ID_CHARS, Message, MessageId, MessageInput, MessageOrigin, MessageStatus,
    QueueFull, SendMessage,
};
pub use skills::{Skill, ThreadSkills};
pub use text::RequiredText;
pub use threads::{CreateThread, ListThreads, Thread, ThreadDetails, ThreadId, ThreadSettings};
pub use time::Timestamp;
pub use turns::{
    AgentItem, CappedText, ContextUsage, FileChange, FileChangeKind, ItemStatus, TurnId,
    TurnOutcome,
};
pub use usage::{
    AccountUsage, Allowance, Continuation, ContinuationId, ContinuationState, UsageBucket,
    UsageReport, UsageWait, UsageWindow,
};
pub use workspaces::{CreateWorkspace, Workspace, WorkspaceId};
