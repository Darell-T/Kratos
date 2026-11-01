//! The Kratos engine's wire types.
//!
//! Every type here serializes to the JSON the TypeScript engine sends and accepts: camelCase
//! fields, `kind` tags on thread events, and `type` tags on socket messages. Parsing is the
//! validation: blank text fails to parse, so a handler never sees it.

mod agents;
mod api;
mod events;
mod text;
mod threads;
mod time;
mod workspaces;

pub use agents::{CommandResult, Permission, Provider, TurnChanges};
pub use api::{ErrorBody, Health};
pub use events::{ClientMessage, PostMessage, Seq, ServerMessage, ThreadEvent, ThreadEventBody};
pub use text::RequiredText;
pub use threads::{CreateThread, ListThreads, Thread, ThreadId};
pub use time::Timestamp;
pub use workspaces::{CreateWorkspace, Workspace, WorkspaceId};
