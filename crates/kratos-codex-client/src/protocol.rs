//! Which generated type each method name carries.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::types;

/// A request Kratos sends: its method name, and the generated types of its parameters and result.
pub trait Method {
    const NAME: &'static str;
    type Params: Serialize;
    type Response: DeserializeOwned;
}

macro_rules! methods {
    ($($name:ident = $method:literal: $params:ty => $response:ty;)*) => {
        /// The requests Kratos sends, one zero-sized type each, for [`crate::Codex::request`].
        pub mod methods {
            use super::*;
            $(
                pub struct $name;
                impl Method for $name {
                    const NAME: &'static str = $method;
                    type Params = $params;
                    type Response = $response;
                }
            )*
        }
    };
}

methods! {
    Initialize = "initialize": types::InitializeParams => types::InitializeResponse;
    ThreadStart = "thread/start": types::ThreadStartParams => types::ThreadStartResponse;
    ThreadResume = "thread/resume": types::ThreadResumeParams => types::ThreadResumeResponse;
    ThreadRead = "thread/read": types::ThreadReadParams => types::ThreadReadResponse;
    ThreadUnsubscribe = "thread/unsubscribe": types::ThreadUnsubscribeParams => types::ThreadUnsubscribeResponse;
    TurnStart = "turn/start": types::TurnStartParams => types::TurnStartResponse;
    TurnSteer = "turn/steer": types::TurnSteerParams => types::TurnSteerResponse;
    TurnInterrupt = "turn/interrupt": types::TurnInterruptParams => types::TurnInterruptResponse;
    ModelList = "model/list": types::ModelListParams => types::ModelListResponse;
    SkillsList = "skills/list": types::SkillsListParams => types::SkillsListResponse;
    PluginInstalled = "plugin/installed": types::PluginInstalledParams => types::PluginInstalledResponse;
    AccountRead = "account/read": types::GetAccountParams => types::GetAccountResponse;
    RateLimitsRead = "account/rateLimits/read": Option<types::GetAccountRateLimitsParams> => types::GetAccountRateLimitsResponse;
    CommandExec = "command/exec": types::CommandExecParams => types::CommandExecResponse;
    WindowsSandboxReadiness = "windowsSandbox/readiness": () => types::WindowsSandboxReadinessResponse;
    WindowsSandboxSetupStart = "windowsSandbox/setupStart": types::WindowsSandboxSetupStartParams => types::WindowsSandboxSetupStartResponse;
}

/// A notification from the server, parsed into its generated type when Kratos handles its method.
#[derive(Clone, Debug, PartialEq)]
pub enum Notification {
    AgentMessageDelta(types::AgentMessageDeltaNotification),
    ItemStarted(types::ItemStartedNotification),
    ItemCompleted(types::ItemCompletedNotification),
    TurnStarted(types::TurnStartedNotification),
    TurnCompleted(types::TurnCompletedNotification),
    Error(types::ErrorNotification),
    ServerRequestResolved(types::ServerRequestResolvedNotification),
    ThreadClosed(types::ThreadClosedNotification),
    ThreadTokenUsageUpdated(types::ThreadTokenUsageUpdatedNotification),
    AccountUpdated(types::AccountUpdatedNotification),
    AccountRateLimitsUpdated(types::AccountRateLimitsUpdatedNotification),
    WindowsSandboxSetupCompleted(types::WindowsSandboxSetupCompletedNotification),
    /// A method Kratos does not handle, kept as sent.
    Other {
        method: String,
        params: Value,
    },
    /// A method Kratos handles whose parameters did not parse: the server speaks a version of the
    /// protocol this build does not.
    Malformed {
        method: String,
        error: String,
    },
}

impl Notification {
    pub fn parse(method: &str, params: Value) -> Notification {
        match NOTIFICATIONS.iter().find(|(name, _)| *name == method) {
            Some((_, parse)) => parse(params).unwrap_or_else(|error| Notification::Malformed {
                method: method.to_owned(),
                error: error.to_string(),
            }),
            None => Notification::Other {
                method: method.to_owned(),
                params,
            },
        }
    }
}

type ParseNotification = fn(Value) -> serde_json::Result<Notification>;

/// The notifications Kratos handles, by method name.
const NOTIFICATIONS: [(&str, ParseNotification); 12] = [
    ("item/agentMessage/delta", |params| {
        serde_json::from_value(params).map(Notification::AgentMessageDelta)
    }),
    ("item/started", |params| {
        serde_json::from_value(params).map(Notification::ItemStarted)
    }),
    ("item/completed", |params| {
        serde_json::from_value(params).map(Notification::ItemCompleted)
    }),
    ("turn/started", |params| {
        serde_json::from_value(params).map(Notification::TurnStarted)
    }),
    ("turn/completed", |params| {
        serde_json::from_value(params).map(Notification::TurnCompleted)
    }),
    ("error", |params| {
        serde_json::from_value(params).map(Notification::Error)
    }),
    ("serverRequest/resolved", |params| {
        serde_json::from_value(params).map(Notification::ServerRequestResolved)
    }),
    ("thread/closed", |params| {
        serde_json::from_value(params).map(Notification::ThreadClosed)
    }),
    ("thread/tokenUsage/updated", |params| {
        serde_json::from_value(params).map(Notification::ThreadTokenUsageUpdated)
    }),
    ("account/updated", |params| {
        serde_json::from_value(params).map(Notification::AccountUpdated)
    }),
    ("account/rateLimits/updated", |params| {
        serde_json::from_value(params).map(Notification::AccountRateLimitsUpdated)
    }),
    ("windowsSandbox/setupCompleted", |params| {
        serde_json::from_value(params).map(Notification::WindowsSandboxSetupCompleted)
    }),
];

/// A request from the server that waits for Kratos's answer.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerRequest {
    CommandApproval(types::CommandExecutionRequestApprovalParams),
    FileChangeApproval(types::FileChangeRequestApprovalParams),
    UserInput(types::ToolRequestUserInputParams),
    Permissions(types::PermissionsRequestApprovalParams),
    McpElicitation(types::McpServerElicitationRequestParams),
}

/// Why a server request reached no handler: its method is not one Kratos answers, or its
/// parameters did not parse.
pub(crate) enum Unhandled {
    UnknownMethod,
    BadParams(String),
}

impl ServerRequest {
    pub(crate) fn parse(method: &str, params: Value) -> Result<ServerRequest, Unhandled> {
        let (_, parse) = SERVER_REQUESTS
            .iter()
            .find(|(name, _)| *name == method)
            .ok_or(Unhandled::UnknownMethod)?;
        parse(params).map_err(|error| Unhandled::BadParams(error.to_string()))
    }
}

type ParseServerRequest = fn(Value) -> serde_json::Result<ServerRequest>;

/// The server requests Kratos answers, by method name.
const SERVER_REQUESTS: [(&str, ParseServerRequest); 5] = [
    ("item/commandExecution/requestApproval", |params| {
        serde_json::from_value(params).map(ServerRequest::CommandApproval)
    }),
    ("item/fileChange/requestApproval", |params| {
        serde_json::from_value(params).map(ServerRequest::FileChangeApproval)
    }),
    ("item/tool/requestUserInput", |params| {
        serde_json::from_value(params).map(ServerRequest::UserInput)
    }),
    ("item/permissions/requestApproval", |params| {
        serde_json::from_value(params).map(ServerRequest::Permissions)
    }),
    ("mcpServer/elicitation/request", |params| {
        serde_json::from_value(params).map(ServerRequest::McpElicitation)
    }),
];
