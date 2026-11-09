//! Claude's undocumented project-directory layout and durable delivery markers.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

/// A valid native Claude session UUID, safe to use as a transcript filename.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl From<Uuid> for SessionId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

impl FromStr for SessionId {
    type Err = TranscriptError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(text)
            .map(Self)
            .map_err(TranscriptError::InvalidId)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Claude refuses a reused new-session ID and an unknown resume ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStart {
    New(SessionId),
    Resume(SessionId),
}

impl SessionStart {
    /// Chooses the flag from the transcript in the user's home, rather than guessing at resume.
    ///
    /// # Errors
    /// Returns an error for an unreadable path instead of silently creating another session.
    pub fn for_session(
        claude_home: &Path,
        cwd: &Path,
        id: SessionId,
    ) -> Result<Self, TranscriptError> {
        let path = transcript_path(claude_home, cwd, id);
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => Ok(Self::Resume(id)),
            Ok(_) => Err(TranscriptError::NotAFile(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::New(id)),
            Err(source) => Err(TranscriptError::Read { path, source }),
        }
    }

    pub(crate) fn flag(self) -> String {
        match self {
            Self::New(id) => format!("--session-id={id}"),
            Self::Resume(id) => format!("--resume={id}"),
        }
    }
}

/// The pinned layout under the user's home directory, not an already appended `.claude` folder.
#[must_use]
pub fn transcript_path(claude_home: &Path, cwd: &Path, id: SessionId) -> PathBuf {
    let project: String = cwd
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    claude_home
        .join(".claude")
        .join("projects")
        .join(project)
        .join(format!("{id}.jsonl"))
}

/// What the native transcript proves about delivery.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeliveredMessages {
    /// UUIDs of messages the CLI durably accepted.
    pub ids: HashSet<String>,
    /// The last line was cut mid-write, so a message accepted just before a crash may be missing
    /// from `ids`. Treat any message not in `ids` as unknown, never as undelivered.
    pub torn_tail: bool,
}

/// Reads accepted user UUIDs, including folded messages saved as queued-command attachments.
/// A missing file means no durable evidence of delivery.
///
/// # Errors
/// Returns an error for unreadable data, a malformed row that is not the unterminated last line,
/// or a delivery marker without a UUID.
pub async fn delivered_message_ids(path: &Path) -> Result<DeliveredMessages, TranscriptError> {
    let read_error = |source| TranscriptError::Read {
        path: path.to_owned(),
        source,
    };
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DeliveredMessages::default());
        }
        Err(source) => return Err(read_error(source)),
    };
    let mut reader = BufReader::new(file);
    let mut delivered = DeliveredMessages::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(read_error)?
            == 0
        {
            return Ok(delivered);
        }
        let terminated = line.last() == Some(&b'\n');
        let row: Value = match serde_json::from_slice(&line) {
            Ok(row) => row,
            Err(_) if !terminated => {
                delivered.torn_tail = true;
                return Ok(delivered);
            }
            Err(source) => {
                return Err(TranscriptError::Json {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        if let Some(id) = delivery_id(&row).map_err(|()| TranscriptError::InvalidDelivery {
            path: path.to_owned(),
        })? {
            delivered.ids.insert(id.to_owned());
        }
    }
}

fn delivery_id(row: &Value) -> Result<Option<&str>, ()> {
    let marker = match row["type"].as_str() {
        Some("user") => &row["uuid"],
        Some("attachment") if row["attachment"]["type"] == "queued_command" => {
            &row["attachment"]["source_uuid"]
        }
        _ => return Ok(None),
    };
    marker
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .map(Some)
        .ok_or(())
}

/// Failure to inspect the native session's durable delivery evidence.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("Claude transcript {} has a delivery marker without a nonempty uuid", .path.display())]
    InvalidDelivery { path: PathBuf },
    #[error("invalid Claude session id: {0}")]
    InvalidId(#[source] uuid::Error),
    #[error("Claude transcript is not a file: {}", .0.display())]
    NotAFile(PathBuf),
    #[error("cannot read Claude transcript {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot parse Claude transcript {}: {source}", .path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}
