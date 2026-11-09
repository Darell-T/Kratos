//! What Kratos answers Claude's requests before its own permission rules exist. Every answer
//! refuses, so nothing runs that no rule allowed.

use kratos_claude_client::{ControlRequest, IncomingControl};
use serde_json::json;

const DENIED: &str =
    "Kratos's permission rules are not built yet, so it declines every tool request";

/// Answers a request Claude made of Kratos, always with a refusal: a tool use is denied, an
/// elicitation is declined, and any other request is an error. Kratos registers no hooks and
/// serves no MCP server yet, so Claude has no other request to make.
pub(super) fn answer(control: IncomingControl) {
    let queued = match &control.request {
        ControlRequest::CanUseTool(_) => control.answer(json!({
            "behavior": "deny",
            "message": DENIED,
        })),
        ControlRequest::Elicitation(_) => control.answer(json!({ "action": "decline" })),
        ControlRequest::HookCallback(_) => control.fail("Kratos registers no hooks"),
        ControlRequest::McpMessage { .. } => control.fail("Kratos serves no MCP servers yet"),
        ControlRequest::Unsupported { .. } => control.fail("Kratos does not answer this request"),
    };
    if !queued {
        eprintln!("claude: the request was withdrawn before Kratos refused it");
    }
}
