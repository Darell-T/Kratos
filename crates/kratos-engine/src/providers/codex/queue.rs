//! Writes that hold or pause a thread's queue, each made inside the write of the step that needs
//! it (D11).

use kratos_protocol::{MessageId, MessageStatus, PauseReason};
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

/// Marks a message whose delivery is uncertain, and pauses the queue on it until Codex's history or
/// the user settles it.
pub(super) fn mark_uncertain(
    write: &mut ThreadWrite<'_>,
    message_id: MessageId,
    error: String,
) -> Result<(), StoreError> {
    write.set_message_status(
        &message_id,
        MessageStatus::Unknown {
            error: error.clone(),
        },
    )?;
    write.pause(PauseReason::DeliveryUncertain { message_id, error })?;
    Ok(())
}
