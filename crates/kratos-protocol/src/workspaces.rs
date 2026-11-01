use std::fmt;

use serde::de::{Deserializer, Error};
use serde::{Deserialize, Serialize};

use crate::{RequiredText, Timestamp};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// For ids the engine issued or read from its records. Ids from clients arrive through
    /// `Deserialize`.
    pub fn new(id: String) -> WorkspaceId {
        WorkspaceId(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WorkspaceId {
    /// Refuses the empty string and keeps the rest as sent. A request body that names a workspace
    /// trims it first ([`crate::CreateThread`]).
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        if id.is_empty() {
            return Err(D::Error::custom("must not be empty"));
        }
        Ok(WorkspaceId(id))
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A project folder the user registered with the engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    /// Exactly as the client sent it, trimmed: an absolute path to a directory that existed when
    /// the workspace was created. Not canonicalized.
    pub path: String,
    pub created_at: Timestamp,
}

/// The engine also requires `path` to name an existing absolute directory, which only the
/// filesystem can answer, so that check runs in the route, not here.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CreateWorkspace {
    pub name: RequiredText,
    pub path: RequiredText,
}
