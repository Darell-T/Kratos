use serde::{Deserialize, Serialize};

/// The variable naming the engine's data folder. Empty counts as unset.
pub const HOME_VAR: &str = "KRATOS_HOME";
/// The data folder, in the user's home folder, when [`HOME_VAR`] is unset.
pub const DEFAULT_HOME_FOLDER: &str = ".kratos";
/// The port the engine listens on unless told another.
pub const DEFAULT_PORT: u16 = 4200;
/// The file in the data folder that holds the engine's client token.
pub const CLIENT_TOKEN_FILE: &str = "client-token";

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
