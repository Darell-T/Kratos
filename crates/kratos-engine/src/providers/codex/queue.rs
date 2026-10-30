//! Writes that hold or pause a thread's queue, each made inside the write of the step that needs
//! it (D11).

use kratos_protocol::PauseReason;
use kratos_store::{StoreError, ThreadWrite};

/// Pauses the queue for `reason` only when messages wait in it.
pub(super) fn pause_if_waiting(
    write: &mut ThreadWrite<'_>,
    reason: PauseReason,
) -> Result<(), StoreError> {
    if !write.pending_messages()?.is_empty() {
        write.pause(reason)?;
    }
    Ok(())
}
