//! The installed Claude Code release. A session starts only on a release Kratos has run, and the
//! user can install the newest one through Kratos instead of finding the CLI's updater.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kratos_claude_client::{
    Latest, ReleaseError, UPDATE_LIMIT, Version, check_native, installed_version, latest_version,
    update,
};
use kratos_protocol::{CliRelease, CliUpdate};
use tokio::sync::watch;
use tokio::task::{AbortHandle, JoinHandle};

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
    pub updating: Mutex<Updating>,
}

#[derive(Default)]
pub(super) enum Updating {
    #[default]
    Idle,
    Preparing {
        task: AbortHandle,
        ended: watch::Receiver<()>,
    },
    /// The updater or the version check after it runs. Stopping it midway can leave a broken CLI,
    /// so shutdown waits for it.
    Installing { ended: watch::Receiver<()> },
    ShuttingDown,
}

struct Running {
    release: Arc<Release>,
    _sender: watch::Sender<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.release.finished();
    }
}

impl Release {
    fn updating(&self) -> MutexGuard<'_, Updating> {
        self.updating.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn start<T: Send + 'static>(
        self: &Arc<Self>,
        update: impl Future<Output = T> + Send + 'static,
    ) -> Result<JoinHandle<T>, ProviderError> {
        let mut updating = self.updating();
        match *updating {
            Updating::Idle => {}
            Updating::ShuttingDown => return Err(ProviderError::Stopped),
            _ => {
                return Err(ProviderError::Conflict(
                    "Claude Code is already updating".to_owned(),
                ));
            }
        }
        let (sender, ended) = watch::channel(());
        let running = Running {
            release: Arc::clone(self),
            _sender: sender,
        };
        let task = tokio::spawn(async move {
            let _running = running;
            update.await
        });
        *updating = Updating::Preparing {
            task: task.abort_handle(),
            ended,
        };
        Ok(task)
    }

    pub fn begin_install(&self) -> bool {
        let mut updating = self.updating();
        let Updating::Preparing { ended, .. } = &*updating else {
            return false;
        };
        let ended = ended.clone();
        *updating = Updating::Installing { ended };
        true
    }

    fn finished(&self) {
        let mut updating = self.updating();
        if !matches!(*updating, Updating::ShuttingDown) {
            *updating = Updating::Idle;
        }
    }

    pub async fn settle_for_shutdown(&self) {
        let taken = std::mem::replace(&mut *self.updating(), Updating::ShuttingDown);
        match taken {
            Updating::Preparing { task, mut ended } => {
                task.abort();
                let _ = ended.changed().await;
            }
            Updating::Installing { mut ended } => {
                eprintln!("Kratos waits for the Claude Code update to finish before it stops");
                if tokio::time::timeout(UPDATE_LIMIT, ended.changed())
                    .await
                    .is_err()
                {
                    eprintln!("the Claude Code update ran past {UPDATE_LIMIT:?}, and Kratos stops");
                }
            }
            Updating::Idle | Updating::ShuttingDown => {}
        }
    }

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

    /// Refuses an install another manager owns, whose updater Kratos does not run.
    pub async fn check_native(&self) -> Result<(), ProviderError> {
        check_native(&self.program, &self.env_set)
            .await
            .map_err(update_error)
    }

    /// Runs the native install's updater. The caller closes every session first.
    pub async fn update(&self) -> Result<(), ProviderError> {
        update(&self.program, &self.env_set)
            .await
            .map(|_| ())
            .map_err(update_error)
    }
}

fn update_error(error: ReleaseError) -> ProviderError {
    match error {
        not_native @ ReleaseError::NotNative { .. } => {
            ProviderError::Unsupported(not_native.to_string())
        }
        error => ProviderError::Unavailable(format!("Claude Code did not update: {error}")),
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
