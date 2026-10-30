//! A client for the Kratos engine's local API, shared by the `kratos-client` command, the engine's
//! MCP server, its tests, and the live verification lanes. It talks to the TypeScript engine and the Rust engine
//! alike. [`Client::send`] is raw on purpose: lanes and tests check the engine's refusals, so a
//! foreign Host or an oversized body must reach it as written.

mod bench;
mod bench_turns;
mod follower;
mod http;
mod normalize;
mod socket;

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kratos_protocol::{Seq, ThreadId};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use bench::{BenchReport, bench_post};
pub use bench_turns::{TurnBenchReport, bench_turns};
pub use follower::{Followed, ThreadFollower};
pub use http::{ApiReply, ApiRequest};
pub use normalize::Normalizer;
pub use socket::{EventSocket, Incoming};

/// The engine at one `http://host:port` address.
///
/// Each request opens its own connection, so concurrent requests from one client run concurrently
/// and no connection outlives an engine restart.
#[derive(Clone, Debug)]
pub struct Client {
    /// `host:port`: the TCP address, and the Host header unless a request names another.
    authority: String,
    /// The file holding the engine's client token, sent as `Authorization: Bearer` unless a
    /// request names its own.
    token_file: Option<PathBuf>,
}

/// The file in the engine's data directory that holds its client token.
const TOKEN_FILE: &str = "client-token";

impl Client {
    /// `base_url` is `http://host:port`, for example `http://127.0.0.1:4200`.
    pub fn new(base_url: &str) -> Result<Client, ClientError> {
        match base_url
            .strip_prefix("http://")
            .map(|rest| rest.trim_end_matches('/'))
        {
            Some(authority) if !authority.is_empty() && !authority.contains('/') => Ok(Client {
                authority: authority.to_owned(),
                token_file: None,
            }),
            _ => Err(ClientError::Address(base_url.to_owned())),
        }
    }

    /// Sends the client token of the engine on data directory `home` with every request and
    /// socket. The file is read again for each, because every engine start issues a new token, so
    /// a client follows the engine through restarts. Without the file no token goes, as the
    /// TypeScript engine expects.
    pub fn with_token_from(mut self, home: &Path) -> Client {
        self.token_file = Some(home.join(TOKEN_FILE));
        self
    }

    pub fn get(&self, path: &str) -> Result<ApiReply, ClientError> {
        self.send(&ApiRequest::new("GET", path))
    }

    /// POSTs `body` as JSON. Tests pass `serde_json::json!` values to send requests the protocol
    /// types would refuse to build, such as a blank name.
    pub fn post<T: Serialize>(&self, path: &str, body: &T) -> Result<ApiReply, ClientError> {
        let body = serde_json::to_vec(body).expect("a request body serializes");
        let request = ApiRequest::new("POST", path)
            .header("Content-Type", "application/json")
            .body(body);
        self.send(&request)
    }

    pub fn delete(&self, path: &str) -> Result<ApiReply, ClientError> {
        self.send(&ApiRequest::new("DELETE", path))
    }

    /// PUTs `body` as JSON.
    pub fn put<T: Serialize>(&self, path: &str, body: &T) -> Result<ApiReply, ClientError> {
        let body = serde_json::to_vec(body).expect("a request body serializes");
        let request = ApiRequest::new("PUT", path)
            .header("Content-Type", "application/json")
            .body(body);
        self.send(&request)
    }

    /// Sends `request` over a new HTTP/1.1 connection and reads the whole reply. The request goes
    /// out as given, with only a missing Host, Content-Length, Authorization, and `Connection:
    /// close` added. An answer that arrives before the body is fully sent, such as a 413, is still
    /// returned.
    pub fn send(&self, request: &ApiRequest) -> Result<ApiReply, ClientError> {
        let bearer = self.bearer();
        let request = match &bearer {
            Some(bearer) if !request.names_header("Authorization") => {
                request.clone().header("Authorization", bearer)
            }
            _ => request.clone(),
        };
        self.send_raw(&request.encode(&self.authority))
    }

    fn bearer(&self) -> Option<String> {
        let token = std::fs::read_to_string(self.token_file.as_ref()?).ok()?;
        let token = token.trim();
        (!token.is_empty()).then(|| format!("Bearer {token}"))
    }

    /// Writes `bytes` to a new connection as they are and reads the reply. For a request that
    /// [`ApiRequest`] cannot express, such as one with no Host header.
    pub fn send_raw(&self, bytes: &[u8]) -> Result<ApiReply, ClientError> {
        http::exchange(&self.authority, bytes)
    }

    pub(crate) fn authority(&self) -> &str {
        &self.authority
    }

    /// Opens `/api/events`, adding `headers` (for example an Origin) to the handshake. A refused
    /// handshake is `Err(ClientError::Handshake)` with the engine's status.
    pub fn connect_events(&self, headers: &[(&str, &str)]) -> Result<EventSocket, ClientError> {
        let bearer = self.bearer();
        let mut headers = headers.to_vec();
        if let Some(bearer) = &bearer
            && !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Authorization"))
        {
            headers.push(("Authorization", bearer.as_str()));
        }
        EventSocket::connect(&self.authority, &headers)
    }

    pub fn follow(&self, thread_id: ThreadId, after: Seq) -> ThreadFollower {
        ThreadFollower::new(self.clone(), thread_id, after)
    }
}

impl ApiReply {
    /// The body as `T`, for a reply the caller expects to have succeeded. For any other status the
    /// error names the status and the body.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, ClientError> {
        if !(200..300).contains(&self.status) {
            return Err(ClientError::Unexpected {
                status: self.status,
                body: self.body.clone(),
            });
        }
        serde_json::from_value(self.body.clone()).map_err(|source| ClientError::Json {
            what: "the reply body",
            source,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("not an http://host:port address: {0}")]
    Address(String),
    #[error("cannot reach the engine at {authority}: {source}")]
    Connect {
        authority: String,
        #[source]
        source: io::Error,
    },
    #[error("HTTP exchange failed: {0}")]
    Io(#[from] io::Error),
    #[error("the engine's reply is not HTTP: {0}")]
    Reply(String),
    #[error("the engine refused the socket with status {0}")]
    Handshake(u16),
    #[error("the socket failed: {0}")]
    Socket(#[from] tungstenite::Error),
    #[error("the engine answered {status}: {body}")]
    Unexpected {
        status: u16,
        body: serde_json::Value,
    },
    #[error("the engine sent an error message: {0}")]
    Engine(String),
    #[error("cannot parse {what}: {source}")]
    Json {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("nothing arrived within {0:?}")]
    TimedOut(Duration),
    #[error("the engine at {0} did not come back within the reconnect budget")]
    EngineGone(String),
    #[error("{0}")]
    Bench(String),
}

impl ClientError {
    /// True when the connection failed, so connecting again may work. A refusal or a bad message
    /// would fail the same way again.
    pub(crate) fn is_connection_loss(&self) -> bool {
        matches!(
            self,
            ClientError::Connect { .. } | ClientError::Io(_) | ClientError::Socket(_)
        )
    }
}
