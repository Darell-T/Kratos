//! Everything the engine reads from its environment, read once at start.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use kratos_protocol::{DEFAULT_PORT, HOME_VAR, ThreadId, engine_home};

/// The variable naming the port the engine listens on. Empty counts as unset.
pub const PORT_VAR: &str = "KRATOS_PORT";
/// The variable naming the thread whose agent the `mcp` subcommand serves.
pub const THREAD_ID_VAR: &str = "KRATOS_THREAD_ID";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Absolute.
    pub home: PathBuf,
    /// 0 asks the system for a free port; the listening line then names the real one.
    pub port: u16,
    /// The user's home folder, when the system knows it.
    pub user_home: Option<PathBuf>,
    /// `CODEX_EXE`, the Codex to run instead of the one on `PATH` (D2).
    pub codex_exe: Option<OsString>,
    /// `PATH`, searched for Codex.
    pub path: Option<OsString>,
    /// Codex's data folder: `CODEX_HOME`, else `.codex` in the user's home folder, made absolute.
    pub codex_home: Option<PathBuf>,
    /// The thread the `mcp` subcommand serves.
    pub mcp_thread: Option<ThreadId>,
}

impl Config {
    /// Reads `KRATOS_HOME` (default `.kratos` in the user's home folder, made absolute against the
    /// current directory), `KRATOS_PORT` (default 4200), Codex's variables, and the `mcp`
    /// subcommand's thread. An empty value counts as unset.
    pub fn from_env() -> Result<Config, ConfigError> {
        let user_home = std::env::home_dir();
        Ok(Config {
            home: home_from_env(user_home.as_deref())?,
            port: port_from_env()?,
            codex_exe: std::env::var_os("CODEX_EXE"),
            path: std::env::var_os("PATH"),
            codex_home: codex_home_from_env(user_home.as_deref()),
            mcp_thread: std::env::var(THREAD_ID_VAR)
                .ok()
                .filter(|id| !id.is_empty())
                .map(ThreadId::new),
            user_home,
        })
    }
}

fn home_from_env(user_home: Option<&Path>) -> Result<PathBuf, ConfigError> {
    let requested =
        engine_home(std::env::var_os(HOME_VAR), user_home).ok_or(ConfigError::NoHomeFolder)?;
    std::path::absolute(&requested).map_err(|source| ConfigError::Home {
        path: requested,
        source,
    })
}

fn codex_home_from_env(user_home: Option<&Path>) -> Option<PathBuf> {
    non_empty_var("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| user_home.map(|home| home.join(".codex")))
        .and_then(|codex_home| std::path::absolute(codex_home).ok())
}

fn port_from_env() -> Result<u16, ConfigError> {
    let Some(raw) = non_empty_var(PORT_VAR) else {
        return Ok(DEFAULT_PORT);
    };
    let text = raw.to_string_lossy().into_owned();
    text.parse().map_err(|_| ConfigError::Port(text))
}

fn non_empty_var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("KRATOS_PORT must be an integer between 0 and 65535, got \"{0}\"")]
    Port(String),
    #[error("cannot find the user's home folder; set KRATOS_HOME")]
    NoHomeFolder,
    #[error("cannot make KRATOS_HOME {} absolute: {source}", path.display())]
    Home {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}
