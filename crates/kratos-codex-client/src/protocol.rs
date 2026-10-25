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
        fn typed<T: DeserializeOwned>(
            method: &str,
            params: Value,
            wrap: fn(T) -> Notification,
        ) -> Notification {
            match serde_json::from_value(params) {
                Ok(params) => wrap(params),
                Err(error) => Notification::Malformed {
                    method: method.to_owned(),
                    error: error.to_string(),
                },
            }
        }
        match method {
            "item/agentMessage/delta" => typed(method, params, Notification::AgentMessageDelta),
            "item/started" => typed(method, params, Notification::ItemStarted),
            "item/completed" => typed(method, params, Notification::ItemCompleted),
            "turn/started" => typed(method, params, Notification::TurnStarted),
            "turn/completed" => typed(method, params, Notification::TurnCompleted),
            "error" => typed(method, params, Notification::Error),
            "serverRequest/resolved" => typed(method, params, Notification::ServerRequestResolved),
            "thread/closed" => typed(method, params, Notification::ThreadClosed),
            "thread/tokenUsage/updated" => {
                typed(method, params, Notification::ThreadTokenUsageUpdated)
            }
            "account/updated" => typed(method, params, Notification::AccountUpdated),
            "account/rateLimits/updated" => {
                typed(method, params, Notification::AccountRateLimitsUpdated)
            }
            "windowsSandbox/setupCompleted" => {
                typed(method, params, Notification::WindowsSandboxSetupCompleted)
            }
            _ => Notification::Other {
                method: method.to_owned(),
                params,
            },
        }
    }
}

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
        fn typed<T: DeserializeOwned>(
            params: Value,
            wrap: fn(T) -> ServerRequest,
        ) -> Result<ServerRequest, Unhandled> {
            serde_json::from_value(params)
                .map(wrap)
                .map_err(|error| Unhandled::BadParams(error.to_string()))
        }
        match method {
            "item/commandExecution/requestApproval" => {
                typed(params, ServerRequest::CommandApproval)
            }
            "item/fileChange/requestApproval" => typed(params, ServerRequest::FileChangeApproval),
            "item/tool/requestUserInput" => typed(params, ServerRequest::UserInput),
            "item/permissions/requestApproval" => typed(params, ServerRequest::Permissions),
            "mcpServer/elicitation/request" => typed(params, ServerRequest::McpElicitation),
            _ => Err(Unhandled::UnknownMethod),
        }
    }
}
