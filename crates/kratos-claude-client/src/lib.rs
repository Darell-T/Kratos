//! Drives Claude Code's structured stream without linking its Agent SDK.

mod connection;
mod locate;
mod protocol;
mod transcript;

pub use connection::{
    Claude, ConnectError, Exit, Incoming, IncomingControl, Launch, MAX_LINE_BYTES, PermissionMode,
    RequestError, SettingSource, connect,
};
pub use locate::{LocateError, locate_claude};
pub use protocol::{
    Account, Assistant, CanUseTool, ControlRequest, Elicitation, HookCallback, HookMatcher,
    Initialize, Initialized, McpServer, Model, ParseError, RateLimitEvent, RateLimitInfo,
    SessionInit, SlashCommand, StreamEvent, StreamMessage, TurnOutcome, TurnResult, UnifiedWindows,
    UsageWindow,
};
pub use transcript::{
    SessionId, SessionStart, TranscriptError, delivered_message_ids, transcript_path,
};
