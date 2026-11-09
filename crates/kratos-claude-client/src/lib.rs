//! Drives Claude Code's structured stream without linking its Agent SDK.

mod connection;
mod launch;
mod locate;
mod protocol;
mod transcript;

pub use connection::{
    Claude, ConnectError, Control, Exit, ExitCause, Incoming, IncomingControl, MAX_LINE_BYTES,
    RequestError, connect,
};
pub use launch::{Launch, PermissionMode, SettingSource};
pub use locate::{LocateError, locate_claude};
pub use protocol::{
    Account, Assistant, CanUseTool, ControlRequest, Elicitation, HookCallback, HookMatcher,
    Initialize, Initialized, McpServer, Model, ParseError, RateLimitEvent, RateLimitInfo,
    SessionInit, SlashCommand, StreamEvent, StreamMessage, TurnOutcome, TurnResult, UnifiedWindows,
    UsageWindow,
};
pub use transcript::{
    DeliveredMessages, SessionId, SessionStart, TranscriptError, delivered_message_ids,
    transcript_path,
};
