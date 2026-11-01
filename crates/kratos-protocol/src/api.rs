use serde::{Deserialize, Serialize};

/// The body of every failed HTTP request, for example `{"error":"Unknown thread 42"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

/// The body of `GET /api/health`: `{"ok":true}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub ok: bool,
}
