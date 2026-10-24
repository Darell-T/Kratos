use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 4200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Absolute.
    pub home: PathBuf,
    /// 0 asks the system for a free port; the listening line then names the real one.
    pub port: u16,
}

impl Config {
    /// Reads `KRATOS_HOME` (default `.kratos` in the user's home folder, made absolute against the
    /// current directory) and `KRATOS_PORT` (default 4200). An empty value counts as unset.
    pub fn from_env() -> Result<Config, ConfigError> {
        Ok(Config {
            home: home_from_env()?,
            port: port_from_env()?,
        })
    }
}

fn home_from_env() -> Result<PathBuf, ConfigError> {
    let requested = match non_empty_var("KRATOS_HOME") {
        Some(home) => PathBuf::from(home),
        None => std::env::home_dir()
            .ok_or(ConfigError::NoHomeFolder)?
            .join(".kratos"),
    };
    std::path::absolute(&requested).map_err(|source| ConfigError::Home {
        path: requested,
        source,
    })
}

fn port_from_env() -> Result<u16, ConfigError> {
    let Some(raw) = non_empty_var("KRATOS_PORT") else {
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
