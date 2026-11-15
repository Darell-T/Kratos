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
use tokio::task::AbortHandle;

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
    /// The update in flight, as engine shutdown sees it.
    pub updating: Mutex<Updating>,
}

/// Where an update stands, which decides what engine shutdown does with it.
#[derive(Default)]
pub(super) enum Updating {
    #[default]
    Idle,
    /// Checking the install or closing sessions. Nothing is replaced yet, so shutdown cancels it.
    /// `task` is missing for the moment between recording the update and starting its task.
    Preparing {
        task: Option<AbortHandle>,
        ended: watch::Receiver<()>,
    },
    /// The updater or the version check after it runs. Stopping it midway can leave a broken CLI,
    /// so shutdown waits for it.
    Installing { ended: watch::Receiver<()> },
    /// The engine is stopping, and no update may start installing.
    ShuttingDown,
}

/// An update in flight. Dropping it ends the update, then wakes a shutdown waiting for it.
pub(super) struct Running {
    release: Arc<Release>,
    ended: watch::Receiver<()>,
    _sender: watch::Sender<()>,
}

impl Running {
    /// What tells this update apart from any later one.
    pub fn id(&self) -> watch::Receiver<()> {
        self.ended.clone()
    }
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

    /// Records an update about to start. The update's task owns the returned [`Running`], so the
    /// update ends however the task does, even when nobody awaits it.
    ///
    /// # Errors
    /// Refuses while another update runs or the engine stops.
    pub fn prepare(self: &Arc<Self>) -> Result<Running, ProviderError> {
        let mut updating = self.updating();
        match *updating {
            Updating::Idle => {
                let (sender, ended) = watch::channel(());
                *updating = Updating::Preparing {
                    task: None,
                    ended: ended.clone(),
                };
                Ok(Running {
                    release: Arc::clone(self),
                    ended,
                    _sender: sender,
                })
            }
            Updating::ShuttingDown => Err(ProviderError::Stopped),
            _ => Err(ProviderError::Conflict(
                "Claude Code is already updating".to_owned(),
            )),
        }
    }

    /// Records the task that runs `update`, so shutdown can cancel it while it prepares. Does
    /// nothing when that update already ended, and another may have started since.
    pub fn started(&self, update: &watch::Receiver<()>, task: AbortHandle) {
        if let Updating::Preparing { task: slot, ended } = &mut *self.updating()
            && ended.same_channel(update)
        {
            *slot = Some(task);
        }
    }

    /// Moves a preparing update to installing. False when the engine is stopping, and the update
    /// must not begin replacing files.
    pub fn begin_install(&self) -> bool {
        let mut updating = self.updating();
        let Updating::Preparing { ended, .. } = &*updating else {
            return false;
        };
        let ended = ended.clone();
        *updating = Updating::Installing { ended };
        true
    }

    /// The update ended, unless shutdown already took it over.
    fn finished(&self) {
        let mut updating = self.updating();
        if !matches!(*updating, Updating::ShuttingDown) {
            *updating = Updating::Idle;
        }
    }

    /// At engine shutdown: cancels an update that has not begun installing, and waits for one that
    /// has, at most [`UPDATE_LIMIT`], the longest the updater and its checks may run before they
    /// end on their own.
    pub async fn settle_for_shutdown(&self) {
        let taken = std::mem::replace(&mut *self.updating(), Updating::ShuttingDown);
        match taken {
            Updating::Preparing { task, mut ended } => {
                if let Some(task) = task {
                    task.abort();
                }
                // A task that had not started yet stops itself at `begin_install`.
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
