//! No thread is exposed to Codex until its Windows sandbox is ready. The elevated implementation
//! supports the denied read paths that protect Kratos's client credential; unelevated does not.

use std::path::PathBuf;
use std::time::Duration;

use kratos_codex_client::{Codex, Incoming, Notification, RequestError, methods, types};

const SETUP_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) async fn prepare(
    codex: &Codex,
    incoming: &mut tokio::sync::mpsc::Receiver<Incoming>,
    cwd: PathBuf,
) -> Result<(), RequestError> {
    let readiness = codex
        .request::<methods::WindowsSandboxReadiness>(&())
        .await?;
    if readiness.status == types::WindowsSandboxReadiness::Ready {
        return Ok(());
    }
    let params = types::WindowsSandboxSetupStartParams {
        mode: types::WindowsSandboxSetupMode::Elevated,
        cwd: Some(types::AbsolutePathBuf(cwd.to_string_lossy().into_owned())),
    };
    let setup = codex
        .request::<methods::WindowsSandboxSetupStart>(&params)
        .await?;
    if !setup.started {
        return Err(setup_error("Codex did not start sandbox setup"));
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
        return Err(setup_error("setup completed but readiness is not ready"));
    }
    Ok(())
}

async fn wait_for_setup(
    incoming: &mut tokio::sync::mpsc::Receiver<Incoming>,
) -> Result<(), RequestError> {
    while let Some(message) = incoming.recv().await {
        match message {
            Incoming::Notification(Notification::WindowsSandboxSetupCompleted(completed)) => {
                if completed.mode != types::WindowsSandboxSetupMode::Elevated {
                    return Err(setup_error(
                        "Codex completed a different sandbox implementation",
                    ));
                }
                return if completed.success {
                    Ok(())
                } else {
                    Err(setup_error(
                        completed.error.as_deref().unwrap_or("no error reported"),
                    ))
                };
            }
            Incoming::Exited(exit) => return Err(RequestError::Exited(exit)),
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
    Err(setup_error(
        "Codex closed before reporting setup completion",
    ))
}

fn setup_error(error: &str) -> RequestError {
    RequestError::Rpc {
        code: kratos_codex_client::INTERNAL_ERROR,
        message: format!("Windows sandbox is unavailable: {error}. No agent commands were started"),
    }
}
