use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The variable naming the engine's data folder. Empty counts as unset.
pub const HOME_VAR: &str = "KRATOS_HOME";
/// The port the engine listens on unless told another.
pub const DEFAULT_PORT: u16 = 4200;
/// The file in the data folder that holds the engine's client token.
pub const CLIENT_TOKEN_FILE: &str = "client-token";
/// The header naming the thread whose agent makes a request through Kratos's tools. A request
/// without it comes from the user.
pub const ACTING_THREAD_HEADER: &str = "Kratos-Thread";

/// The engine's data folder: `home_var`, the value of [`HOME_VAR`], unless it is empty, else
/// `.kratos` in `user_home`. `None` when neither names one.
pub fn engine_home(home_var: Option<OsString>, user_home: Option<&Path>) -> Option<PathBuf> {
    home_var
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| user_home.map(|home| home.join(".kratos")))
}

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
