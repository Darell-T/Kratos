//! The installed Claude Code release. A session starts only on a release Kratos has run, and the
//! user can install the newest one through Kratos instead of finding the CLI's updater.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use kratos_claude_client::{
    Latest, ReleaseError, Version, installed_version, latest_version, update,
};
use kratos_protocol::{CliRelease, CliUpdate};
use tokio::sync::Mutex;

use crate::providers::ProviderError;

/// The release R4 measured (`claude-cli-direct-driver.md`). An older one may lack the control
/// requests and flags Kratos sends.
pub(super) const OLDEST: Version = Version::new(2, 1, 303);

/// What the CLI's release commands need, shared by every handle to the provider.
pub(super) struct Release {
    pub program: PathBuf,
    pub env_set: Vec<(OsString, OsString)>,
    /// Where the native install's updater reads the newest release on each channel.
    pub feed: String,
    /// Held while `claude update` runs, so a second request is refused instead of racing it.
    pub updating: Mutex<()>,
}

impl Release {
    pub async fn status(&self) -> CliRelease {
        let installed = installed_version(&self.program, &self.env_set).await;
        let update = match &installed {
            Err(error) => CliUpdate::Unknown {
                reason: error.to_string(),
            },
            Ok(installed) => match latest_version(&self.program, &self.env_set, &self.feed).await {
                Ok(Latest::Release(latest)) if latest > *installed => CliUpdate::Available {
                    latest: latest.to_string(),
                },
                Ok(Latest::Release(_)) => CliUpdate::Current,
                Ok(Latest::Unknown(reason)) => CliUpdate::Unknown { reason },
                Err(error) => CliUpdate::Unknown {
                    reason: error.to_string(),
                },
            },
        };
        CliRelease {
            installed: installed.ok().map(|version| version.to_string()),
            oldest_supported: OLDEST.to_string(),
            update,
        }
    }

    /// Runs the native install's updater. Sessions already running keep the release they started
    /// on. Another manager's install is refused, since its updater replaces their files.
    pub async fn update(&self) -> Result<(), ProviderError> {
        let Ok(_updating) = self.updating.try_lock() else {
            return Err(ProviderError::Conflict(
                "Claude Code is already updating".to_owned(),
            ));
        };
        match update(&self.program, &self.env_set).await {
            Ok(_) => Ok(()),
            Err(not_native @ ReleaseError::NotNative { .. }) => {
                Err(ProviderError::Unsupported(not_native.to_string()))
            }
            Err(error) => Err(ProviderError::Unavailable(format!(
                "Claude Code did not update: {error}"
            ))),
        }
    }
}

/// Refuses to start a session on a release older than [`OLDEST`]. A version Kratos cannot read
/// does not stop the start: a newer CLI may print it differently, and a missing one fails to
/// start with its own error.
pub(super) async fn supported(
    program: &Path,
    env_set: &[(OsString, OsString)],
) -> Result<(), String> {
    match installed_version(program, env_set).await {
        Ok(installed) if installed < OLDEST => Err(format!(
            "Claude Code {installed} is older than {OLDEST}, the oldest release Kratos runs. \
             Update it from Kratos, then resume the thread"
        )),
        _ => Ok(()),
    }
}
