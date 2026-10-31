//! No thread is exposed to Codex until its Windows sandbox is ready. The elevated implementation
//! supports the denied read paths that protect Kratos's client credential; unelevated does not.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use kratos_codex_client::{Codex, Incoming, Notification, RequestError, methods, types};

const SETUP_TIMEOUT: Duration = Duration::from_secs(120);
const MODE: types::WindowsSandboxSetupMode = types::WindowsSandboxSetupMode::Elevated;

pub(super) fn launch_arguments() -> [OsString; 2] {
    ["-c".into(), format!("windows.sandbox=\"{MODE}\"").into()]
}

#[derive(Debug, thiserror::Error)]
pub(super) enum SandboxError {
    #[error(transparent)]
    Request(#[from] RequestError),
    #[error("Windows sandbox is unavailable: {0}. No agent commands were started")]
    Unavailable(String),
}

pub(super) async fn prepare(
    codex: &Codex,
    incoming: &mut tokio::sync::mpsc::Receiver<Incoming>,
    cwd: PathBuf,
) -> Result<(), SandboxError> {
    let readiness = codex
        .request::<methods::WindowsSandboxReadiness>(&())
        .await?;
    if readiness.status == types::WindowsSandboxReadiness::Ready {
        return Ok(());
    }
    let params = types::WindowsSandboxSetupStartParams {
        mode: MODE,
        cwd: Some(types::AbsolutePathBuf(cwd.to_string_lossy().into_owned())),
    };
    let setup = codex
        .request::<methods::WindowsSandboxSetupStart>(&params)
        .await?;
    if !setup.started {
        return Err(unavailable("Codex did not start sandbox setup"));
    }
    tokio::time::timeout(SETUP_TIMEOUT, wait_for_setup(incoming))
        .await
        .map_err(|_| RequestError::Timeout {
            method: "Windows sandbox setupCompleted".to_owned(),
            after: SETUP_TIMEOUT,
        })??;
    let readiness = codex
        .request::<methods::WindowsSandboxReadiness>(&())
        .await?;
    if readiness.status != types::WindowsSandboxReadiness::Ready {
        return Err(unavailable("setup completed but readiness is not ready"));
    }
    Ok(())
}

async fn wait_for_setup(
    incoming: &mut tokio::sync::mpsc::Receiver<Incoming>,
) -> Result<(), SandboxError> {
    while let Some(message) = incoming.recv().await {
        match message {
            Incoming::Notification(Notification::WindowsSandboxSetupCompleted(completed)) => {
                if completed.mode != MODE {
                    return Err(unavailable(
                        "Codex completed a different sandbox implementation",
                    ));
                }
                return if completed.success {
                    Ok(())
                } else {
                    Err(unavailable(
                        completed.error.as_deref().unwrap_or("no error reported"),
                    ))
                };
            }
            Incoming::Exited(exit) => return Err(RequestError::Exited(exit).into()),
            Incoming::Request(request) => {
                request.fail(
                    kratos_codex_client::INTERNAL_ERROR,
                    "Windows sandbox setup is still running",
                );
            }
            // No threads exist yet. Startup warnings cannot change the readiness result.
            Incoming::Notification(_) => {}
        }
    }
    Err(unavailable(
        "Codex closed before reporting setup completion",
    ))
}

fn unavailable(error: &str) -> SandboxError {
    SandboxError::Unavailable(error.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_server_starts_with_the_sandbox_that_setup_prepares() {
        // Catches a launch argument that names another implementation, or spells the elevated
        // one differently from the text Codex's config reads.
        assert_eq!(
            launch_arguments(),
            [
                OsString::from("-c"),
                OsString::from("windows.sandbox=\"elevated\"")
            ]
        );
    }
}
