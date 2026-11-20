//! Claude's frames in Kratos's terms: the uuids a message and a first session go by, the content
//! Claude is sent, the items its replies become, and the plan that pays for the session.

use kratos_claude_client::{Assistant, Initialized, SessionId, StreamEvent};
use kratos_protocol::{AgentItem, MessageId, MessageInput, SlashCommand, ThreadId, TurnId};
use serde_json::{Value, json};
use uuid::Uuid;

/// The namespace of the uuids Kratos derives for message ids that are not uuids themselves.
const MESSAGE_NAMESPACE: Uuid = Uuid::from_u128(0x6b72_6174_6f73_4d73_6749_6400_0000_0001);

/// The namespace of the session id each thread's first Claude session takes.
const SESSION_NAMESPACE: Uuid = Uuid::from_u128(0x6b72_6174_6f73_5365_7373_696f_6e00_0001);

/// The session a thread's first Claude process starts. It is derived from the thread id, so an
/// engine that stopped after Claude took the first message, and before Kratos saw Claude take it,
/// finds that session's transcript again instead of starting a conversation without it (L7).
pub(super) fn first_session(thread_id: &ThreadId) -> SessionId {
    SessionId::from(Uuid::new_v5(
        &SESSION_NAMESPACE,
        thread_id.as_str().as_bytes(),
    ))
}

/// The uuid Claude knows `message_id` by. Claude's transcript and replay echo hold only this uuid,
/// so reconciling a crash (D11) recomputes it from the id. A client's id that is a uuid is used as
/// it is; any other id maps to a fixed uuid derived from its text.
pub fn native_id(message_id: &MessageId) -> Uuid {
    Uuid::parse_str(message_id.as_str())
        .unwrap_or_else(|_| Uuid::new_v5(&MESSAGE_NAMESPACE, message_id.as_str().as_bytes()))
}

/// The turn that starts with `message_id`: Claude's turns have no ids of their own, so the first
/// message of a turn names it.
pub(super) fn turn_of(message_id: &MessageId) -> TurnId {
    TurnId::new(message_id.as_str().to_owned())
}

/// The content blocks of a user message. A file or an image is named by its path in a text block,
/// which Claude opens with its own tools (D86).
pub(super) fn user_content(input: &[MessageInput]) -> Value {
    let blocks: Vec<Value> = input
        .iter()
        .map(|part| {
            let text = match part {
                MessageInput::Text { text } => text.as_str().to_owned(),
                MessageInput::Image { path } => format!("Attached image: {}", path.as_str()),
                MessageInput::File { path } => format!("Attached file: {}", path.as_str()),
            };
            json!({ "type": "text", "text": text })
        })
        .collect();
    Value::Array(blocks)
}

/// The items a completed assistant message holds. Each text block is a reply, saved whole (D88);
/// a tool call is kept by its tool's name until Kratos describes it.
pub(super) fn assistant_items(assistant: &Assistant) -> Vec<AgentItem> {
    let message = &assistant.message;
    let message_id = message["id"].as_str().unwrap_or(&assistant.uuid);
    let Some(blocks) = message["content"].as_array() else {
        return Vec::new();
    };
    let mut replies = 0;
    let mut items = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        match block["type"].as_str() {
            Some("text") => {
                let id = reply_id(message_id, replies);
                replies += 1;
                let text = block["text"].as_str().unwrap_or_default().to_owned();
                items.push(AgentItem::Message { id, text });
            }
            Some("tool_use") => items.push(AgentItem::Other {
                id: block["id"]
                    .as_str()
                    .map_or_else(|| format!("{message_id}:{index}"), str::to_owned),
                provider_type: format!("tool_use:{}", block["name"].as_str().unwrap_or("unknown")),
            }),
            _ => {}
        }
    }
    items
}

/// The id of the `nth` reply of a Claude API message. The first is the message's own id, which is
/// also the id its live deltas carry, so a client replaces the streamed text with the saved one.
fn reply_id(message_id: &str, nth: usize) -> String {
    if nth == 0 {
        message_id.to_owned()
    } else {
        format!("{message_id}:{nth}")
    }
}

/// The reply text a streaming event adds, with the id of the reply it belongs to.
pub(super) fn reply_delta(stream: &StreamEvent) -> Option<(String, String)> {
    let event = &stream.event;
    if event["type"] != "content_block_delta" || event["delta"]["type"] != "text_delta" {
        return None;
    }
    let message_id = stream.metadata.get("api_message_id")?.as_str()?;
    Some((
        reply_id(message_id, 0),
        event["delta"]["text"].as_str()?.to_owned(),
    ))
}

/// The slash commands the session's `initialize` listed.
pub(super) fn slash_commands(commands: &[kratos_claude_client::SlashCommand]) -> Vec<SlashCommand> {
    commands
        .iter()
        .map(|command| SlashCommand {
            name: command.name.clone(),
            description: command.description.clone(),
            argument_hint: command.argument_hint.clone(),
            aliases: command.aliases.clone(),
            builtin: command.builtin,
        })
        .collect()
}

/// The subscription that pays for the session, never the account behind it (D24).
pub(super) fn plan(initialized: &Initialized) -> Option<String> {
    initialized
        .account
        .as_ref()
        .and_then(|account| account.subscription_type.clone())
}

#[cfg(test)]
mod tests {
    use kratos_protocol::RequiredText;

    use super::*;

    fn id(text: &str) -> MessageId {
        MessageId::new(text.to_owned())
    }

    #[test]
    fn a_message_id_maps_to_one_uuid_each_time_and_a_uuid_id_maps_to_itself() {
        // Catches a reconcile that recomputes another uuid than the one Claude was sent, which
        // would call a delivered message undelivered, and a client's own uuid being rewritten.
        let uuid = "00000000-0000-4000-8000-00000000000a";
        assert_eq!(native_id(&id(uuid)).to_string(), uuid);
        assert_eq!(native_id(&id("not a uuid")), native_id(&id("not a uuid")));
        assert_ne!(native_id(&id("one")), native_id(&id("two")));
    }

    #[test]
    fn attachments_reach_claude_as_text_naming_their_path() {
        // Catches an attachment dropped on the way to Claude, which would answer without it.
        let text = |text: &str| RequiredText::new(text).unwrap();
        let content = user_content(&[
            MessageInput::Text { text: text("look") },
            MessageInput::Image {
                path: text("/p/a.png"),
            },
            MessageInput::File {
                path: text("/p/b.rs"),
            },
        ]);
        assert_eq!(
            content,
            json!([
                { "type": "text", "text": "look" },
                { "type": "text", "text": "Attached image: /p/a.png" },
                { "type": "text", "text": "Attached file: /p/b.rs" },
            ])
        );
    }
}
