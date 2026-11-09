//! The queue rules every provider shares. Each is one step of a write to a thread's records:
//! which message holds the queue, what pauses it, and what lets it run again (D11, D39, D85).

use kratos_protocol::{ContinuationState, Message, MessageId, MessageStatus, PauseReason, TurnId};
use kratos_store::{StoreError, ThreadWrite};

/// Whether a message holds the queue until the user settles it: the provider refused it, or its delivery
/// is uncertain (D11).
pub(crate) fn holds_queue(status: &MessageStatus) -> bool {
    matches!(
        status,
        MessageStatus::Failed { .. } | MessageStatus::Unknown { .. }
    )
}

pub(crate) fn settling_action(message: &Message) -> Option<String> {
    let message_id = &message.id;
    match message.status {
        MessageStatus::Submitting => {
            Some(format!("Wait for message {message_id} to finish sending"))
        }
        MessageStatus::Failed { .. } => Some(format!("Retry or remove message {message_id}")),
        MessageStatus::Unknown { .. } => Some(format!("Discard message {message_id}")),
        MessageStatus::Queued | MessageStatus::Accepted { .. } | MessageStatus::Cancelled => None,
    }
}

pub(crate) fn held_message(write: &ThreadWrite<'_>) -> Result<Option<Message>, StoreError> {
    Ok(write
        .pending_messages()?
        .into_iter()
        .find(|message| holds_queue(&message.status)))
}

pub(crate) fn pause_if_waiting(
    write: &mut ThreadWrite<'_>,
    reason: PauseReason,
) -> Result<(), StoreError> {
    if !write.pending_messages()?.is_empty() {
        write.pause(reason)?;
    }
    Ok(())
}

/// Marks a message the provider refused before acting on it, and pauses the queue on it until the user
/// retries or removes it.
pub(crate) fn mark_refused(
    write: &mut ThreadWrite<'_>,
    message_id: MessageId,
    error: String,
) -> Result<(), StoreError> {
    write.set_message_status(
        &message_id,
        MessageStatus::Failed {
            error: error.clone(),
        },
    )?;
    write.pause(PauseReason::Refused { message_id, error })?;
    Ok(())
}

/// Marks a message whose delivery is uncertain, and pauses the queue on it until the provider's history or
/// the user settles it.
pub(crate) fn mark_uncertain(
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

pub(crate) fn resume_when_settled(write: &mut ThreadWrite<'_>) -> Result<(), StoreError> {
    let held = held_message(write)?.is_some();
    let paused_by_message = matches!(
        write.provider_thread()?.paused,
        Some(PauseReason::Refused { .. } | PauseReason::DeliveryUncertain { .. })
    );
    if paused_by_message && !held {
        write.resume()?;
    }
    Ok(())
}

/// Records each found message as accepted by its turn, then lifts a delivery pause once nothing
/// uncertain is left, or turns it into an interrupted pause while messages wait.
pub(crate) fn settle_from_history(
    write: &mut ThreadWrite<'_>,
    found: Vec<(kratos_protocol::MessageId, TurnId)>,
) -> Result<(), StoreError> {
    for (message_id, turn_id) in found {
        write.set_message_status(&message_id, MessageStatus::Accepted { turn_id })?;
    }
    let pending = write.pending_messages()?;
    let held = pending.iter().any(|message| holds_queue(&message.status));
    // Only a delivery pause lifts here; resume_when_settled also lifts a refusal's.
    let paused_for_delivery = matches!(
        write.provider_thread()?.paused,
        Some(PauseReason::DeliveryUncertain { .. })
    );
    match (paused_for_delivery && !held, pending.is_empty()) {
        (false, _) => {}
        (true, true) => {
            write.resume()?;
        }
        (true, false) => {
            write.pause(PauseReason::Interrupted)?;
        }
    }
    Ok(())
}

/// Lets a paused queue run again, in the caller's write. Refused with the action the user must
/// take while a message is still being sent, which the user can only wait out, and while a message
/// holds the queue, because only the user may settle it (D11). An open continuation is cancelled:
/// the user took over (D39).
pub(crate) fn resume_queue(write: &mut ThreadWrite<'_>) -> Result<Result<(), String>, StoreError> {
    // Unlike held_message, a message still being sent refuses the resume too.
    let refusal = write.pending_messages()?.iter().find_map(settling_action);
    if let Some(refusal) = refusal {
        return Ok(Err(format!("{refusal} before resuming")));
    }
    if let Some(open) = write.open_continuation()? {
        write.update_continuation(
            &open.id,
            open.resets_at,
            ContinuationState::Cancelled {
                reason: "The user resumed the thread".into(),
            },
        )?;
    }
    write.resume()?;
    Ok(Ok(()))
}
