//! How the native CLI is launched: its flags, its environment, and its process tree.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use kratos_process::OwnedChild;
use tokio::process::Command;

use crate::{ConnectError, SessionStart};

/// The CLI's permission setting. Approval policy itself belongs to the engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionMode {
    #[default]
    Default,
    Auto,
    Plan,
    BypassPermissions,
}

impl PermissionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

/// Settings sources are explicit so project settings are never loaded accidentally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingSource {
    User,
    Project,
    Local,
}

impl SettingSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

/// The complete native CLI launch, with intentional overrides applied after inherited filtering.
#[derive(Clone, Debug)]
pub struct Launch {
    pub program: PathBuf,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub permission_mode: PermissionMode,
    pub setting_sources: Vec<SettingSource>,
    pub session: SessionStart,
    pub disallowed_tools: Vec<String>,
    pub env_set: Vec<(OsString, OsString)>,
    pub control_timeout: Duration,
    pub close_timeout: Duration,
}

impl Launch {
    /// Starts with no settings sources and the CLI's default model and permissions.
    #[must_use]
    pub fn new(program: PathBuf, cwd: PathBuf, session: SessionStart) -> Self {
        Self {
            program,
            cwd,
            session,
            model: None,
            permission_mode: PermissionMode::Default,
            setting_sources: Vec::new(),
            disallowed_tools: Vec::new(),
            env_set: Vec::new(),
            control_timeout: Duration::from_secs(30),
            close_timeout: Duration::from_secs(5),
        }
    }
}

pub(crate) fn spawn(launch: &Launch) -> Result<OwnedChild, ConnectError> {
    let mut command = Command::new(&launch.program);
    command
        .args([
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--permission-prompt-tool=stdio",
            "--include-partial-messages",
            "--replay-user-messages",
        ])
        .arg(launch.session.flag())
        .arg(format!(
            "--permission-mode={}",
            launch.permission_mode.as_str()
        ))
        .arg(format!(
            "--setting-sources={}",
            launch
                .setting_sources
                .iter()
                .map(|source| source.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ))
        .current_dir(&launch.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(model) = &launch.model {
        command.arg(format!("--model={model}"));
    }
    if launch.permission_mode == PermissionMode::BypassPermissions {
        command.arg("--allow-dangerously-skip-permissions");
    }
    if !launch.disallowed_tools.is_empty() {
        command
            .arg("--disallowedTools")
            .arg(launch.disallowed_tools.join(","));
    }
    // A session never updates the CLI in the background: the updater would replace files running
    // sessions load, and the user decides when to update (Kratos's update route).
    command.env("DISABLE_AUTOUPDATER", "1");
    set_environment(&mut command, &launch.env_set);
    OwnedChild::spawn(command).map_err(|source| ConnectError::Spawn {
        program: launch.program.clone(),
        source,
    })
}

/// Removes every inherited `CLAUDE*`, `ANTHROPIC*`, and `KRATOS*` variable, so the CLI uses the
/// subscription login and none of Kratos's secrets, then sets `env_set`.
pub(crate) fn set_environment(command: &mut Command, env_set: &[(OsString, OsString)]) {
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("CLAUDE")
            || upper.starts_with("ANTHROPIC")
            || upper.starts_with("KRATOS")
        {
            command.env_remove(name);
        }
    }
    for (name, value) in env_set {
        command.env(name, value);
    }
}
