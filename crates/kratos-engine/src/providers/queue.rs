//! The queue rules every provider shares. Each is one step of a write to a thread's records:
//! which message holds the queue, what pauses it, and what lets it run again (D11, D39, D85).

use kratos_protocol::{
    ContinuationId, ContinuationState, Message, MessageId, MessageInput, MessageStatus,
    PauseReason, RequiredText, TurnId,
};
use kratos_store::{Enqueued, StoreError, ThreadWrite};
use uuid::Uuid;

use super::{Continued, MAX_PENDING_MESSAGES};

/// The text of the message Kratos sends to carry on a turn that stopped at a usage limit (D39).
const CONTINUATION_TEXT: &str = "Continue the task you were working on. You stopped earlier because the usage limit was reached.";

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

/// The write that queues a continuation's message ahead of the waiting ones, marks the
/// continuation sent, and lets the queue run, in one write (D39). A continuation that is no longer
/// open sends nothing. One that would run past a message waiting on the user, or past a pause the
/// limit did not cause, or that finds the queue full (D85), goes back to the user. Retrying with
/// the same `continuation_id` queues nothing twice, because the store keeps one message per
/// continuation.
pub(crate) fn continuation_write(
    continuation_id: ContinuationId,
) -> impl FnOnce(&mut ThreadWrite<'_>) -> Result<Continued, StoreError> + Send + 'static {
    let message_id = MessageId::new(Uuid::new_v4().to_string());
    let input = vec![MessageInput::Text {
        text: RequiredText::new(CONTINUATION_TEXT).expect("the continuation text is not blank"),
    }];
    move |write| {
        if let Some(why) = blocks_continuation(write)? {
            return Ok(Continued::NeedsUser(why));
        }
        let sent =
            write.send_continuation(&continuation_id, &message_id, input, MAX_PENDING_MESSAGES)?;
        Ok(match sent {
            Some(Enqueued::Added(_) | Enqueued::Existing(_)) => {
                write.resume()?;
                Continued::Queued
            }
            Some(Enqueued::Full { pending }) => Continued::NeedsUser(format!(
                "The thread's queue is full with {pending} messages: remove one, then let Kratos continue"
            )),
            None => Continued::NotOpen,
        })
    }
}

/// Why the thread cannot take a continuation on its own, if it cannot: a message waits on the user,
/// or the queue is paused for something the reset does not fix.
fn blocks_continuation(write: &mut ThreadWrite<'_>) -> Result<Option<String>, StoreError> {
    if let Some(held) = held_message(write)? {
        return Ok(settling_action(&held));
    }
    Ok(match write.provider_thread()?.paused {
        None | Some(PauseReason::UsageLimit { .. } | PauseReason::Unavailable { .. }) => None,
        Some(_) => Some("The thread is paused for another reason; resume it yourself".to_owned()),
    })
}
