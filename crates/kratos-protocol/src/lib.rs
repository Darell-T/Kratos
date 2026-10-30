//! The Kratos engine's wire types.
//!
//! Every type here serializes to the JSON the TypeScript engine sends and accepts: camelCase
//! fields, `kind` tags on thread events, and `type` tags on socket messages. Parsing is the
//! validation: blank text fails to parse, so a handler never sees it.

/// Declares an id that is a string on the wire, with `new`, `as_str`, and `Display`. Each id gives
/// its own doc comment and derives, and an id that refuses some strings writes its own
/// `Deserialize`.
macro_rules! string_id {
    ($(#[$attribute:meta])* $name:ident) => {
        $(#[$attribute])*
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// For an id that did not come from a client. Ids from clients arrive through
            /// `Deserialize`.
            pub fn new(id: String) -> $name {
                $name(id)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

mod api;
mod approvals;
mod events;
mod messages;
pub mod routes;
mod skills;
mod socket;
mod text;
mod threads;
mod time;
mod turns;
mod usage;
mod workspaces;

pub use api::{CLIENT_TOKEN_FILE, DEFAULT_HOME_FOLDER, DEFAULT_PORT, ErrorBody, HOME_VAR, Health};
pub use approvals::{
    AnswerQuestion, Approval, ApprovalDecision, ApprovalId, ApprovalOutcome, ApprovalRequest,
    AskReason, DecideApproval, ExtraAccess, NetworkAccess, PathAccess, PathAccessMode, Question,
    QuestionId, QuestionItem, QuestionOption, QuestionOutcome,
};
pub use events::{PauseReason, Seq, ThreadEvent, ThreadEventBody};
pub use messages::{
    MAX_MESSAGE_ID_CHARS, Message, MessageId, MessageInput, MessageOrigin, MessageStatus,
    PostMessage, QueueFull, SendMessage,
};
pub use skills::{Skill, ThreadSkills};
pub use socket::{ClientMessage, LiveUpdate, ReplySoFar, ServerMessage};
pub use text::RequiredText;
pub use threads::{
    AccessLimit, CreateThread, ListEvents, ListThreads, OutsideWrites, Permission, Provider,
    Thread, ThreadDetails, ThreadId, ThreadSandbox, ThreadSettings,
};
pub use time::Timestamp;
pub use turns::{
    AgentItem, CappedText, CommandResult, ContextUsage, FileChange, FileChangeKind, ItemStatus,
    TurnChanges, TurnId, TurnOutcome,
};
pub use usage::{
    AccountUsage, Allowance, Continuation, ContinuationId, ContinuationState, UsageBucket,
    UsageReport, UsageWait, UsageWindow,
};
pub use workspaces::{CreateWorkspace, Workspace, WorkspaceId};
