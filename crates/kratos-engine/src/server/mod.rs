mod json;
mod local_only;
mod paths;
mod routes;
mod socket;

use std::future::{Future, IntoFuture};
use std::io;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use kratos_protocol::ErrorBody;
use kratos_store::StoreError;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::hub::Hub;
use crate::store_thread::StoreThread;

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

/// Anything but these routes, including a known path with another method, answers 404
/// `{"error":"Not found"}`.
fn router(state: ServerState) -> Router {
    let routes = Router::new()
        .route("/api/health", get(routes::health))
        .route(
            "/api/workspaces",
            get(routes::list_workspaces).post(routes::create_workspace),
        )
        .route(
            "/api/threads",
            get(routes::list_threads).post(routes::create_thread),
        )
        .route("/api/threads/{id}/messages", post(routes::post_message))
        .route("/api/events", get(socket::events))
        .fallback(routes::not_found)
        .method_not_allowed_fallback(routes::not_found)
        .with_state(state);
    // An outer service rewrites the URI before the inner router selects a route. Router::layer
    // on the inner router would run only after that selection.
    Router::new()
        .fallback_service(routes)
        .layer(middleware::from_fn(paths::normalize_http_api_path))
        // Added last, so the Host and Origin checks run before normalization and every route.
        .layer(middleware::from_fn(local_only::refuse_other_hosts))
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
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
            StoreError::UnknownThread(_) | StoreError::UnknownWorkspace(_) => ApiError {
                status: StatusCode::NOT_FOUND,
                message: error.to_string(),
            },
            other => ApiError::internal(&other),
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
