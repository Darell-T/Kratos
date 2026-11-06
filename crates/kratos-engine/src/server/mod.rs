mod acting;
mod agents;
pub mod auth;
mod json;
mod local_only;
mod socket;
mod threads;
mod workspaces;

use std::future::{Future, IntoFuture};
use std::io;
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request};
use axum::http::header::CONTENT_TYPE;
use axum::http::{StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use kratos_protocol::routes::{
    APPROVAL, CONTINUATION_SCHEDULE, EVENTS, HEALTH, MESSAGE, MESSAGE_DISCARD, MESSAGE_RETRY,
    MESSAGE_STEER, QUESTION, THREAD, THREAD_EVENTS, THREAD_MESSAGES, THREAD_RESUME,
    THREAD_SETTINGS, THREAD_SKILLS, THREAD_STOP, THREADS, USAGE, WORKSPACES,
};
use kratos_protocol::{ErrorBody, Health, MessageId, ThreadId};
use kratos_store::StoreError;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::hub::Hub;
use crate::providers::codex::{CodexProvider, ProviderError};
use crate::scheduler::{SchedulerError, SchedulerHandle};
use crate::store_thread::StoreThread;
use crate::usage::UsageTracker;

pub use auth::ClientToken;

/// The largest request body a route reads. One byte more answers 413.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// The largest message a client may send on the socket. One byte more closes that socket.
pub const MAX_SOCKET_MESSAGE_BYTES: usize = 64 * 1024;
/// Time for in-flight HTTP requests to finish after shutdown. A stalled upload cannot hold the
/// process open beyond this drain period.
const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct ServerState {
    pub store: StoreThread,
    pub hub: Hub,
    /// Codex and its scheduler, or why Codex cannot run, such as no Codex installed.
    pub codex: Result<Codex, String>,
    pub usage: UsageTracker,
    pub token: ClientToken,
}

#[derive(Clone)]
pub struct Codex {
    pub provider: CodexProvider,
    /// The usage scheduler. `None` only where a test drives Codex without one.
    pub scheduler: Option<SchedulerHandle>,
}

impl ServerState {
    fn codex(&self) -> Result<&CodexProvider, ApiError> {
        self.codex
            .as_ref()
            .map(|codex| &codex.provider)
            .map_err(|missing| ApiError::unavailable(missing.clone()))
    }
}

type ThreadPath = Result<Path<ThreadId>, PathRejection>;
type MessagePath = Result<Path<(ThreadId, MessageId)>, PathRejection>;

fn path<T>(path: Result<Path<T>, PathRejection>) -> Result<T, ApiError> {
    path.map(|Path(value)| value)
        .map_err(|_| ApiError::bad_request("Malformed URL path"))
}

/// Serves the API on `listener` until `shutdown` completes, then stops accepting and lets the HTTP
/// requests in flight finish for at most [`HTTP_DRAIN_TIMEOUT`]. The caller drops the runtime
/// after this returns, cancelling remaining requests and sockets before the store closes.
pub async fn serve(
    listener: TcpListener,
    state: ServerState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let (draining, drain_started) = oneshot::channel();
    let serving = axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown.await;
            let _ = draining.send(());
        })
        .into_future();
    tokio::pin!(serving);
    tokio::select! {
        result = &mut serving => result,
        _ = drain_started => match tokio::time::timeout(HTTP_DRAIN_TIMEOUT, serving).await {
            Ok(result) => result,
            Err(_) => Ok(()),
        },
    }
}

/// Anything but the routes below, including a known path with another method, answers 404
/// `{"error":"Not found"}`.
fn router(state: ServerState) -> Router {
    let token = state.token.clone();
    let routes = agent_routes()
        .merge(user_routes().route_layer(middleware::from_fn(acting::refuse_agents)))
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .with_state(state);
    // An outer service rewrites the URI before the inner router selects a route. Router::layer
    // on the inner router would run only after that selection.
    Router::new()
        .fallback_service(routes)
        .layer(middleware::from_fn_with_state(
            token,
            auth::require_client_token,
        ))
        .layer(middleware::from_fn(normalize_http_api_path))
        // Added last, so the Host and Origin checks run before normalization and every route.
        .layer(middleware::from_fn(local_only::refuse_other_hosts))
}

/// TypeScript HTTP routes ignore empty segments after checking the `/api/` prefix. Its socket
/// upgrade handler requires exactly `/api/events`, so that endpoint keeps its original spelling.
async fn normalize_http_api_path(mut request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if path.starts_with("/api/") && (path.ends_with('/') || path.contains("//")) {
        let normalized = format!(
            "/{}",
            path.split('/')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("/")
        );
        if normalized != EVENTS {
            let path_and_query = match request.uri().query() {
                Some(query) => format!("{normalized}?{query}"),
                None => normalized,
            };
            let mut parts = request.uri().clone().into_parts();
            parts.path_and_query = Some(
                path_and_query
                    .parse()
                    .expect("removing path separators preserves a valid URI"),
            );
            *request.uri_mut() =
                Uri::from_parts(parts).expect("the URI's scheme and authority are unchanged");
        }
    }
    next.run(request).await
}

/// The routes Kratos's tools call for a thread's agent, and the only ones a request naming a
/// thread reaches. Opening a route to agents means moving it here, and its handler must then apply
/// `Acting`'s rules. The reads need none, since an agent may read every thread.
fn agent_routes() -> Router<ServerState> {
    Router::new()
        .route(WORKSPACES, get(workspaces::list_workspaces))
        .route(
            THREADS,
            get(threads::list_threads).post(threads::create_thread),
        )
        .route(THREAD, get(threads::thread_details))
        .route(THREAD_EVENTS, get(threads::thread_events))
        .route(THREAD_MESSAGES, post(agents::post_message))
        .route(THREAD_STOP, post(agents::stop))
        .route(APPROVAL, post(agents::decide))
        .route(QUESTION, post(agents::answer))
}

/// Every other route. `router` refuses a request naming a thread on all of them, so a new route
/// stays closed to agents until it moves to [`agent_routes`].
fn user_routes() -> Router<ServerState> {
    Router::new()
        .route(HEALTH, get(health))
        .route(WORKSPACES, post(workspaces::create_workspace))
        .route(MESSAGE, delete(agents::remove_message))
        .route(MESSAGE_STEER, post(agents::steer))
        .route(MESSAGE_RETRY, post(agents::retry_message))
        .route(MESSAGE_DISCARD, post(agents::discard_message))
        .route(THREAD_RESUME, post(agents::resume))
        .route(THREAD_SETTINGS, put(agents::put_settings))
        .route(THREAD_SKILLS, get(agents::skills))
        .route(CONTINUATION_SCHEDULE, post(agents::schedule))
        .route(USAGE, get(agents::usage))
        .route(EVENTS, get(socket::events))
}

async fn health() -> Response {
    json_reply(StatusCode::OK, &Health { ok: true })
}

async fn not_found() -> ApiError {
    ApiError::not_found()
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl ToString) -> ApiError {
        ApiError {
            status,
            message: message.to_string(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub fn not_found() -> ApiError {
        ApiError {
            status: StatusCode::NOT_FOUND,
            message: "Not found".into(),
        }
    }

    pub fn unauthorized() -> ApiError {
        ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "Unauthorized".into(),
        }
    }

    /// The agent cannot serve the request now, for example because Codex is not installed.
    pub fn unavailable(message: String) -> ApiError {
        ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message,
        }
    }

    pub fn forbidden() -> ApiError {
        ApiError {
            status: StatusCode::FORBIDDEN,
            message: "Forbidden".into(),
        }
    }

    pub fn body_too_large() -> ApiError {
        ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "Request body is too large".into(),
        }
    }

    /// Logs `error` to stderr and answers 500 "Internal server error", so internals never reach a
    /// client.
    pub fn internal(error: &dyn std::error::Error) -> ApiError {
        eprintln!("internal error: {error}");
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "Internal server error".into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> ApiError {
        match error {
            StoreError::UnknownThread(_)
            | StoreError::UnknownWorkspace(_)
            | StoreError::UnknownMessage { .. }
            | StoreError::UnknownApproval(_)
            | StoreError::UnknownQuestion(_)
            | StoreError::UnknownContinuation(_) => ApiError::new(StatusCode::NOT_FOUND, error),
            StoreError::MessageIdTaken(_) | StoreError::MessageTransition { .. } => {
                ApiError::new(StatusCode::CONFLICT, error)
            }
            StoreError::UnreadableEvent { .. } | StoreError::Sqlite(_) => {
                ApiError::internal(&error)
            }
        }
    }
}

impl From<ProviderError> for ApiError {
    fn from(error: ProviderError) -> ApiError {
        match error {
            ProviderError::UnknownThread(_)
            | ProviderError::UnknownMessage(_)
            | ProviderError::UnknownApproval(_)
            | ProviderError::UnknownQuestion(_) => ApiError::new(StatusCode::NOT_FOUND, error),
            ProviderError::Conflict(_) => ApiError::new(StatusCode::CONFLICT, error),
            ProviderError::Forbidden(_) => ApiError::new(StatusCode::FORBIDDEN, error),
            ProviderError::Unsupported(_) => ApiError::bad_request(error.to_string()),
            ProviderError::Unavailable(_) | ProviderError::Stopped => {
                ApiError::unavailable(error.to_string())
            }
            ProviderError::Store(error) => error.into(),
        }
    }
}

impl From<SchedulerError> for ApiError {
    fn from(error: SchedulerError) -> ApiError {
        match error {
            SchedulerError::UnknownContinuation(_) => ApiError::new(StatusCode::NOT_FOUND, error),
            SchedulerError::Conflict(_) => ApiError::new(StatusCode::CONFLICT, error),
            SchedulerError::Stopped => ApiError::unavailable(error.to_string()),
            SchedulerError::Store(error) => error.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        json_reply(
            self.status,
            &ErrorBody {
                error: self.message,
            },
        )
    }
}

/// `body` as JSON with `Content-Type: application/json; charset=utf-8`.
pub fn json_reply<T: Serialize>(status: StatusCode, body: &T) -> Response {
    let json = serde_json::to_vec(body).expect("wire types serialize");
    (
        status,
        [(CONTENT_TYPE, "application/json; charset=utf-8")],
        json,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use kratos_protocol::{ApprovalId, ContinuationId, MessageStatus, QuestionId};

    use super::*;

    fn status_of(error: StoreError) -> StatusCode {
        ApiError::from(error).status
    }

    #[test]
    fn a_store_refusal_answers_the_status_of_what_the_caller_got_wrong() {
        // Catches a caller's unknown id or forbidden queue move answering 500 as if the store had
        // failed.
        let message_id = MessageId::new("message".to_owned());
        let unknown = [
            StoreError::UnknownMessage {
                thread_id: ThreadId::new("thread".to_owned()),
                message_id: message_id.clone(),
            },
            StoreError::UnknownApproval(ApprovalId::new("approval".to_owned())),
            StoreError::UnknownQuestion(QuestionId::new("question".to_owned())),
            StoreError::UnknownContinuation(ContinuationId::new("continuation".to_owned())),
        ];
        for error in unknown {
            let text = error.to_string();
            assert_eq!(status_of(error), StatusCode::NOT_FOUND, "{text}");
        }
        let forbidden_move = StoreError::MessageTransition {
            message_id,
            from: MessageStatus::Submitting,
            to: MessageStatus::Queued,
        };
        assert_eq!(status_of(forbidden_move), StatusCode::CONFLICT);
    }
}
