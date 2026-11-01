//! The client token every request but the health check carries (D19).
//!
//! Each engine start writes a new 256-bit token to `<KRATOS_HOME>/client-token`, readable by the
//! user alone, and Kratos's own clients read it there. Every Codex permission profile denies agents
//! that folder, and Codex starts with none of Kratos's variables (L8), so a command an agent runs
//! cannot learn the token. Without it, the agent cannot decide its own approvals or drive other
//! threads, even with network access.

use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::ApiError;
use super::json::discard;

/// The file in the data directory that holds the token.
pub const TOKEN_FILE: &str = "client-token";

#[derive(Clone)]
pub struct ClientToken(Arc<str>);

impl ClientToken {
    /// A new random token, written to `home`'s token file in place of the last engine's.
    pub fn issue(home: &Path) -> io::Result<ClientToken> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        // Written whole beside the old one, then moved over it, so a client never reads half a
        // token; created readable by the user alone before any byte is written.
        let partial = home.join(format!("{TOKEN_FILE}.partial"));
        let _ = std::fs::remove_file(&partial);
        let mut file = private_file(&partial)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&partial, home.join(TOKEN_FILE))?;
        Ok(ClientToken(token.into()))
    }

    /// Compares in constant time, so the time a refusal takes says nothing about the token.
    fn matches(&self, presented: &[u8]) -> bool {
        let expected = self.0.as_bytes();
        presented.len() == expected.len()
            && presented
                .iter()
                .zip(expected)
                .fold(0u8, |difference, (a, b)| difference | (a ^ b))
                == 0
    }
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// On Windows the file takes the data directory's permissions, which under the user's profile
/// admit the user alone.
#[cfg(not(unix))]
fn private_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Answers 401 `{"error":"Unauthorized"}` unless the request carries `Authorization: Bearer
/// <token>`. The health check needs no token, so a client can tell a running engine from a refusal.
/// A refused body is read and thrown away first, as a 413 is, so the client sees the status.
pub(super) async fn require_client_token(
    State(token): State<ClientToken>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "));
    let is_health = request.uri().path() == "/api/health";
    if is_health || presented.is_some_and(|presented| token.matches(presented)) {
        return next.run(request).await;
    }
    discard(request.into_body()).await;
    ApiError::unauthorized().into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_token_replaces_the_file_and_only_an_exact_match_passes() {
        // Catches a token that survives a restart, which would keep a leaked token valid, and a
        // comparison that accepts a prefix or a different length.
        let home = tempfile::tempdir().unwrap();
        let first = ClientToken::issue(home.path()).unwrap();
        let second = ClientToken::issue(home.path()).unwrap();

        let stored = std::fs::read_to_string(home.path().join(TOKEN_FILE)).unwrap();
        assert_eq!(stored.len(), 64);
        assert_eq!(stored, &*second.0);
        assert_ne!(first.0, second.0);
        assert!(second.matches(stored.as_bytes()));
        assert!(!second.matches(&stored.as_bytes()[..63]));
        assert!(!second.matches(first.0.as_bytes()));
        assert!(!home.path().join(format!("{TOKEN_FILE}.partial")).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.path().join(TOKEN_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
