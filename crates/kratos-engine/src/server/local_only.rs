//! The Host and Origin check every request passes before routing, sockets included.
//!
//! Any web page can make a browser send requests to 127.0.0.1. Requiring a local Host name stops
//! DNS rebinding, and requiring a local Origin when one is present (browsers always send one across
//! sites) stops other pages from driving the engine. Clients that send no Origin, such as
//! kratos-client, pass.

use axum::extract::Request;
use axum::http::HeaderMap;
use axum::http::header::{HOST, ORIGIN};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::ApiError;

const LOCAL_HOSTNAMES: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Answers 403 `{"error":"Forbidden"}` unless the request comes from this machine's own pages or
/// clients.
pub(super) async fn refuse_other_hosts(request: Request, next: Next) -> Response {
    if !is_local_request(request.headers()) {
        return ApiError::forbidden().into_response();
    }
    next.run(request).await
}

fn is_local_request(headers: &HeaderMap) -> bool {
    let host_is_local = headers
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .is_some_and(is_local_authority);
    let origin_is_local = headers.get(ORIGIN).is_none_or(|origin| {
        origin
            .to_str()
            .ok()
            .and_then(origin_authority)
            .is_some_and(is_local_authority)
    });
    host_is_local && origin_is_local
}

/// Stricter than the WHATWG URL parsing the TypeScript engine relies on: spellings such as `127.1`
/// or `localhost.` are refused. A stricter check can only refuse a local client, never admit a
/// remote one.
fn is_local_authority(authority: &str) -> bool {
    let authority = authority.to_ascii_lowercase();
    // The last colon starts a port only when digits follow it. `[::1]` has colons that are not one.
    let host = match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            host
        }
        _ => &authority,
    };
    LOCAL_HOSTNAMES.contains(&host)
}

/// `None` for `null` and for anything without `://`, which therefore counts as a foreign origin.
fn origin_authority(origin: &str) -> Option<&str> {
    origin
        .split_once("://")
        .map(|(_scheme, authority)| authority)
}
