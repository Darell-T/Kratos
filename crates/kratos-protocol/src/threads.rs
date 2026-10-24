use std::fmt;

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

use crate::{Provider, RequiredText, Timestamp, WorkspaceId};

/// A thread's id. The engine issues UUIDs. Any string parses, because a thread that does not exist
/// answers 404.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(String);

impl ThreadId {
    pub fn new(id: String) -> ThreadId {
        ThreadId(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One task run by one agent on this machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: ThreadId,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub provider: Provider,
    pub created_at: Timestamp,
    /// The `at` of the thread's latest event, or `created_at` before its first.
    pub updated_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateThread {
    #[serde(deserialize_with = "trimmed_workspace_id")]
    pub workspace_id: WorkspaceId,
    pub title: RequiredText,
    pub provider: Provider,
}

fn trimmed_workspace_id<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<WorkspaceId, D::Error> {
    RequiredText::deserialize(deserializer).map(|id| WorkspaceId::new(id.into_string()))
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListThreads {
    pub workspace_id: WorkspaceId,
}
