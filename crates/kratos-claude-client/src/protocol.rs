//! Claude's wire shapes and the normalization that hides its replay and result conventions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage::{ApiRetry, RateLimitStatus, RateLimitType, TurnContext};

/// Startup configuration for SDK-owned MCP servers, hooks, and structured output.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Initialize {
    #[serde(
        rename = "sdkMcpServers",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub sdk_mcp_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, Vec<HookMatcher>>,
    #[serde(rename = "jsonSchema", skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<Value>,
}

/// Hook callbacks are registered by identifiers, rather than executable commands.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HookMatcher {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    #[serde(rename = "hookCallbackIds")]
    pub hook_callback_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
}

/// A model advertised by the CLI, including the native model it resolves to.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub value: String,
    pub display_name: String,
    pub description: String,
    pub resolved_model: Option<String>,
}

/// A slash command advertised during initialization.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    /// Other names that run the command.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// True for the CLI's own commands. The CLI leaves it out for a command a user, project, plugin
    /// or MCP server defines.
    #[serde(default)]
    pub builtin: bool,
}

/// Login capabilities without an account email or organization identifier.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub subscription_type: Option<String>,
    pub api_provider: Option<String>,
    pub token_source: Option<String>,
    pub api_key_source: Option<String>,
}

/// The initialize control response, distinct from a later streamed session-init frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Initialized {
    pub models: Vec<Model>,
    pub commands: Vec<SlashCommand>,
    pub account: Option<Account>,
    /// Account email and organization fields are removed before this value is exposed.
    pub raw: Value,
}

impl Initialized {
    /// Parses advertised capabilities and removes account identity from the retained response.
    ///
    /// # Errors
    /// Returns an error when a required capability field has an unreadable shape.
    pub fn parse(mut raw: Value) -> Result<Self, ParseError> {
        #[derive(Deserialize)]
        struct Wire {
            models: Vec<Model>,
            commands: Vec<SlashCommand>,
            account: Option<Account>,
        }
        let wire: Wire = serde_json::from_value(raw.clone())?;
        if let Some(account) = raw.get_mut("account").and_then(Value::as_object_mut) {
            account.remove("email");
            account.remove("organization");
            account.remove("accountUuid");
            account.remove("organizationUuid");
        }
        Ok(Self {
            models: wire.models,
            commands: wire.commands,
            account: wire.account,
            raw,
        })
    }
}

/// The session's current model and protocol capabilities, emitted when a turn starts.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct SessionInit {
    pub model: String,
    pub session_id: String,
    pub claude_code_version: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

/// Assistant output whose correlation list may be absent after a folded message.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Assistant {
    pub message: Value,
    pub uuid: String,
    pub session_id: String,
    pub user_message_uuids: Option<Vec<String>>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

/// A streaming Anthropic event, retained without reproducing its content-block union.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct StreamEvent {
    pub event: Value,
    pub uuid: Option<String>,
    pub session_id: String,
    pub user_message_uuids: Option<Vec<String>>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

/// Terminal meaning independent of Claude's error-shaped interrupt result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Interrupted,
    Failed { error: String },
}

/// The complete set of submitted messages consumed by one native turn.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnResult {
    pub message_ids: Vec<String>,
    pub outcome: TurnOutcome,
    pub structured_output: Option<Value>,
    /// The context the turn used, from the result's token counts.
    pub context: TurnContext,
    pub raw: Value,
}

impl TurnResult {
    /// Normalizes the terminal result using the existing TypeScript provider's precedence.
    ///
    /// # Errors
    /// Returns an error when required result fields are absent or malformed.
    pub fn parse(raw: Value) -> Result<Self, ParseError> {
        #[derive(Deserialize)]
        struct Wire {
            subtype: String,
            is_error: bool,
            result: Option<String>,
            errors: Option<Vec<String>>,
            user_message_uuid: Option<String>,
            user_message_uuids: Option<Vec<String>>,
            terminal_reason: Option<String>,
            structured_output: Option<Value>,
        }
        let wire: Wire = serde_json::from_value(raw.clone())?;
        let message_ids = wire.user_message_uuids.unwrap_or_else(|| {
            wire.user_message_uuid
                .filter(|id| !id.is_empty())
                .into_iter()
                .collect()
        });
        let outcome = if matches!(
            wire.terminal_reason.as_deref(),
            Some("aborted_streaming" | "aborted_tools")
        ) {
            TurnOutcome::Interrupted
        } else if wire.subtype != "success" || wire.is_error {
            let failure = if wire.subtype == "success" {
                wire.result
            } else {
                wire.errors.map(|errors| errors.join("\n"))
            };
            TurnOutcome::Failed {
                error: failure
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| "Claude turn failed".to_owned()),
            }
        } else {
            TurnOutcome::Completed
        };
        Ok(Self {
            message_ids,
            outcome,
            structured_output: wire.structured_output,
            context: TurnContext::of(&raw),
            raw,
        })
    }
}

/// One subscription allowance window. Reset timestamps are Unix seconds.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub utilization: Option<f64>,
    pub resets_at: Option<u64>,
}

/// The two measured allowance windows, independently optional.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct UnifiedWindows {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
}

/// Subscription limits as reported by the CLI, without inferring scheduler behavior.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitInfo {
    pub status: RateLimitStatus,
    pub rate_limit_type: Option<RateLimitType>,
    pub resets_at: Option<u64>,
    /// The reported limit's utilization as a fraction, when the event gives one.
    pub utilization: Option<f64>,
    pub unified_windows: Option<UnifiedWindows>,
    pub is_using_overage: Option<bool>,
    pub overage_status: Option<String>,
    pub overage_disabled_reason: Option<String>,
}

/// A subscription-limit notification and its original protocol fields.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RateLimitEvent {
    pub rate_limit_info: RateLimitInfo,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

/// Messages forwarded to the engine. Unknown kinds remain available as their original JSON.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamMessage {
    Init(SessionInit),
    Replay {
        uuid: String,
    },
    Assistant(Assistant),
    StreamEvent(StreamEvent),
    Result(TurnResult),
    RateLimit(RateLimitEvent),
    /// The CLI's request failed and the CLI retries by itself.
    ApiRetry(ApiRetry),
    Unrecognized {
        kind: String,
        raw: Value,
    },
}

impl StreamMessage {
    /// Parses a non-control frame, tolerating unknown fields and future message types.
    ///
    /// # Errors
    /// Returns an error for malformed known messages or an absent message type.
    pub fn parse(raw: Value) -> Result<Self, ParseError> {
        let kind = raw
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ParseError::MissingType)?
            .to_owned();
        match kind.as_str() {
            "system" => Self::system(raw),
            "user" => Self::user(raw),
            "result" => Ok(Self::Result(TurnResult::parse(raw)?)),
            _ => Self::ordinary(kind, raw),
        }
    }

    fn ordinary(kind: String, raw: Value) -> Result<Self, ParseError> {
        match kind.as_str() {
            "assistant" => Ok(Self::Assistant(serde_json::from_value(raw)?)),
            "stream_event" => Ok(Self::StreamEvent(serde_json::from_value(raw)?)),
            "rate_limit_event" => Ok(Self::RateLimit(serde_json::from_value(raw)?)),
            _ => Ok(Self::Unrecognized { kind, raw }),
        }
    }

    fn system(raw: Value) -> Result<Self, ParseError> {
        if raw["subtype"] == "api_retry" {
            return Ok(Self::ApiRetry(serde_json::from_value(raw)?));
        }
        if raw["subtype"] != "init" {
            return Ok(Self::Unrecognized {
                kind: "system".to_owned(),
                raw,
            });
        }
        let init: SessionInit = serde_json::from_value(raw)?;
        if init.model.is_empty() {
            return Err(ParseError::EmptyModel);
        }
        Ok(Self::Init(init))
    }

    fn user(raw: Value) -> Result<Self, ParseError> {
        if raw["isReplay"] != true {
            return Ok(Self::Unrecognized {
                kind: "user".to_owned(),
                raw,
            });
        }
        let uuid = raw
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|uuid| !uuid.is_empty())
            .ok_or(ParseError::MissingReplayUuid)?;
        Ok(Self::Replay {
            uuid: uuid.to_owned(),
        })
    }
}

/// A permission callback, with policy details retained for the permission owner.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct CanUseTool {
    pub tool_name: String,
    pub input: Value,
    pub tool_use_id: String,
    #[serde(default)]
    pub permission_suggestions: Vec<Value>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub mcp_server: Option<McpServer>,
    #[serde(flatten)]
    pub details: BTreeMap<String, Value>,
}

/// The source distinguishes SDK-owned tools from configured external servers.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub source: String,
}

/// A registered hook invocation whose answer belongs to the hook owner.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct HookCallback {
    pub callback_id: String,
    pub input: Value,
    pub tool_use_id: Option<String>,
}

/// An MCP server's request for user input. Recordings do not exercise this declared shape.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Elicitation {
    pub mcp_server_name: String,
    pub message: String,
    pub mode: Option<String>,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
    pub requested_schema: Option<Value>,
}

/// Reverse controls retain their application-owned request bodies.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlRequest {
    CanUseTool(CanUseTool),
    McpMessage { server_name: String, message: Value },
    HookCallback(HookCallback),
    Elicitation(Elicitation),
    Unsupported { subtype: String },
}

impl ControlRequest {
    pub(crate) fn parse(raw: Value) -> Result<Self, ParseError> {
        let subtype = raw
            .get("subtype")
            .and_then(Value::as_str)
            .ok_or(ParseError::MissingSubtype)?
            .to_owned();
        match subtype.as_str() {
            "can_use_tool" => Ok(Self::CanUseTool(serde_json::from_value(raw)?)),
            "mcp_message" => {
                #[derive(Deserialize)]
                struct Wire {
                    server_name: String,
                    message: Value,
                }
                let wire: Wire = serde_json::from_value(raw)?;
                Ok(Self::McpMessage {
                    server_name: wire.server_name,
                    message: wire.message,
                })
            }
            "hook_callback" => Ok(Self::HookCallback(serde_json::from_value(raw)?)),
            "elicitation" => Ok(Self::Elicitation(serde_json::from_value(raw)?)),
            _ => Ok(Self::Unsupported { subtype }),
        }
    }
}

/// Stdout data or a control request that the crate cannot turn into a typed message.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Claude control request has no string request_id")]
    MissingRequestId,
    #[error("Claude session init has an empty model")]
    EmptyModel,
    #[error("Claude message has no string type")]
    MissingType,
    #[error("Claude control request has no string subtype")]
    MissingSubtype,
    #[error("Claude replay has no nonempty uuid")]
    MissingReplayUuid,
    #[error("cannot read Claude message: {0}")]
    Json(#[from] serde_json::Error),
}
