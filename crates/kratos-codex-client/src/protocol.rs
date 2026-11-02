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

macro_rules! notifications {
    ($($name:ident = $method:literal: $params:ty;)*) => {
        /// A notification from the server, parsed into its generated type when Kratos handles its
        /// method.
        #[derive(Clone, Debug, PartialEq)]
        pub enum Notification {
            $($name($params),)*
            /// A method Kratos does not handle, kept as sent.
            Other {
                method: String,
                params: Value,
            },
            /// A method Kratos handles whose parameters did not parse: the server speaks a version
            /// of the protocol this build does not.
            Malformed {
                method: String,
                error: String,
            },
        }

        impl Notification {
            pub fn parse(method: &str, params: Value) -> Notification {
                let parsed = match method {
                    $($method => serde_json::from_value(params).map(Notification::$name),)*
                    _ => {
                        return Notification::Other {
                            method: method.to_owned(),
                            params,
                        };
                    }
                };
                parsed.unwrap_or_else(|error| Notification::Malformed {
                    method: method.to_owned(),
                    error: error.to_string(),
                })
            }
        }
    };
}

notifications! {
    AgentMessageDelta = "item/agentMessage/delta": types::AgentMessageDeltaNotification;
    ItemStarted = "item/started": types::ItemStartedNotification;
    ItemCompleted = "item/completed": types::ItemCompletedNotification;
    TurnStarted = "turn/started": types::TurnStartedNotification;
    TurnCompleted = "turn/completed": types::TurnCompletedNotification;
    Error = "error": types::ErrorNotification;
    ServerRequestResolved = "serverRequest/resolved": types::ServerRequestResolvedNotification;
    ThreadClosed = "thread/closed": types::ThreadClosedNotification;
    ThreadTokenUsageUpdated = "thread/tokenUsage/updated": types::ThreadTokenUsageUpdatedNotification;
    AccountUpdated = "account/updated": types::AccountUpdatedNotification;
    AccountRateLimitsUpdated = "account/rateLimits/updated": types::AccountRateLimitsUpdatedNotification;
    WindowsSandboxSetupCompleted = "windowsSandbox/setupCompleted": types::WindowsSandboxSetupCompletedNotification;
}

/// Why a server request reached no handler: its method is not one Kratos answers, or its
/// parameters did not parse.
pub(crate) enum Unhandled {
    UnknownMethod,
    BadParams(String),
}

macro_rules! server_requests {
    ($($name:ident = $method:literal: $params:ty => $response:ty;)*) => {
        /// A request from the server that waits for Kratos's answer.
        #[derive(Clone, Debug, PartialEq)]
        pub enum ServerRequest {
            $($name($params),)*
        }

        /// Kratos's answer to a [`ServerRequest`] of the same kind.
        #[derive(Clone, Debug, PartialEq, Serialize)]
        #[serde(untagged)]
        pub enum ServerResponse {
            $($name($response),)*
        }

        impl ServerRequest {
            pub(crate) fn parse(method: &str, params: Value) -> Result<ServerRequest, Unhandled> {
                let parsed = match method {
                    $($method => serde_json::from_value(params).map(ServerRequest::$name),)*
                    _ => return Err(Unhandled::UnknownMethod),
                };
                parsed.map_err(|error| Unhandled::BadParams(error.to_string()))
            }

            pub fn method(&self) -> &'static str {
                match self {
                    $(ServerRequest::$name(_) => $method,)*
                }
            }
        }

        impl ServerResponse {
            /// The method of the request this answers.
            pub fn method(&self) -> &'static str {
                match self {
                    $(ServerResponse::$name(_) => $method,)*
                }
            }
        }
    };
}

server_requests! {
    CommandApproval = "item/commandExecution/requestApproval": types::CommandExecutionRequestApprovalParams => types::CommandExecutionRequestApprovalResponse;
    FileChangeApproval = "item/fileChange/requestApproval": types::FileChangeRequestApprovalParams => types::FileChangeRequestApprovalResponse;
    UserInput = "item/tool/requestUserInput": types::ToolRequestUserInputParams => types::ToolRequestUserInputResponse;
    Permissions = "item/permissions/requestApproval": types::PermissionsRequestApprovalParams => types::PermissionsRequestApprovalResponse;
    McpElicitation = "mcpServer/elicitation/request": types::McpServerElicitationRequestParams => types::McpServerElicitationRequestResponse;
}

impl ServerRequest {
    /// The Codex thread the request belongs to.
    pub fn thread_id(&self) -> &str {
        use types::McpServerElicitationRequestParams as Elicitation;
        match self {
            ServerRequest::CommandApproval(params) => &params.thread_id,
            ServerRequest::FileChangeApproval(params) => &params.thread_id,
            ServerRequest::UserInput(params) => &params.thread_id,
            ServerRequest::Permissions(params) => &params.thread_id,
            ServerRequest::McpElicitation(
                Elicitation::Variant0 { thread_id, .. }
                | Elicitation::Variant1 { thread_id, .. }
                | Elicitation::Variant2 { thread_id, .. }
                | Elicitation::Variant3 { thread_id, .. }
                | Elicitation::Variant4 { thread_id, .. },
            ) => thread_id,
        }
    }
}
