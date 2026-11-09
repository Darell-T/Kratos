//! How a turn ends: its outcome, the context it used (D76), and whether it stopped at the
//! account's usage limit (D39). One write records all three, so the log never shows a turn that
//! ended without the pause that explains why nothing follows it.

use kratos_claude_client::{TurnContext, TurnOutcome as ClaudeOutcome, TurnResult};
use kratos_protocol::{ContextUsage, PauseReason, Timestamp, TurnId, TurnOutcome};
use kratos_store::{StoreError, ThreadWrite};

use super::threads::SeenLimit;
use crate::providers::queue::pause_if_waiting;

/// Whether the turn stopped because the account ran out of usage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Stop {
    Ordinary,
    /// The turn failed and the latest `rate_limit_event` of the turn said rejected. Only the end of
    /// the turn decides this: an event alone changes nothing, and a turn that goes on to succeed
    /// was one the CLI waited out and resumed by itself.
    UsageLimit {
        resets_at: Option<Timestamp>,
    },
}

pub(super) struct TurnEnd {
    pub outcome: TurnOutcome,
    pub context: Option<ContextUsage>,
    pub stop: Stop,
}

impl TurnEnd {
    /// A turn that ended because its process did.
    pub(super) fn failed(error: String) -> TurnEnd {
        TurnEnd {
            outcome: TurnOutcome::Failed { error },
            context: None,
            stop: Stop::Ordinary,
        }
    }

    /// The end a result reports. `limit` is the latest rate limit event of the turn, and
    /// `answering` the model that wrote its last reply, which names the context window.
    pub(super) fn of(
        result: &TurnResult,
        limit: Option<&SeenLimit>,
        answering: Option<&str>,
    ) -> TurnEnd {
        let outcome = match &result.outcome {
            ClaudeOutcome::Completed => TurnOutcome::Completed,
            ClaudeOutcome::Interrupted => TurnOutcome::Interrupted,
            ClaudeOutcome::Failed { error } => TurnOutcome::Failed {
                error: error.clone(),
            },
        };
        let stop = match (&outcome, limit) {
            (TurnOutcome::Failed { .. }, Some(seen)) if seen.rejected => Stop::UsageLimit {
                resets_at: seen.resets_at.clone(),
            },
            _ => Stop::Ordinary,
        };
        TurnEnd {
            outcome,
            context: context_usage(&result.context, answering),
            stop,
        }
    }
}

/// The context the turn's last request filled, the same meaning as Codex's `last` counts (D76):
/// input includes what the cache holds, and the total is input plus output. Claude counts thinking
/// inside its output and reports no split, so the reasoning share is zero. A result that lists no
/// request has no context, never a context of zeros.
pub(super) fn context_usage(
    context: &TurnContext,
    answering: Option<&str>,
) -> Option<ContextUsage> {
    let last = context.last?;
    let input_tokens =
        last.input_tokens + last.cache_creation_input_tokens + last.cache_read_input_tokens;
    Some(ContextUsage {
        input_tokens,
        cached_input_tokens: last.cache_read_input_tokens,
        output_tokens: last.output_tokens,
        reasoning_output_tokens: 0,
        total_tokens: input_tokens + last.output_tokens,
        context_window: context.window_of(answering),
    })
}

/// Ends `turn_id` with `end`, in the caller's write. A usage-limit stop pauses the thread whatever
/// waits behind it, because the scheduler continues from that pause. Any other failure pauses the
/// queue behind it when messages wait, because they may no longer make sense.
pub(super) fn end_turn(
    write: &mut ThreadWrite<'_>,
    turn_id: TurnId,
    end: TurnEnd,
    changes: kratos_protocol::TurnChanges,
) -> Result<(), StoreError> {
    let TurnEnd {
        outcome,
        context,
        stop,
    } = end;
    let failure = match &outcome {
        TurnOutcome::Failed { error } => Some(error.clone()),
        TurnOutcome::Completed | TurnOutcome::Interrupted => None,
    };
    write.complete_turn(turn_id.clone(), outcome, changes, context)?;
    match (stop, failure) {
        (Stop::UsageLimit { resets_at }, _) => {
            write.pause(PauseReason::UsageLimit { turn_id, resets_at })?;
            Ok(())
        }
        (Stop::Ordinary, Some(error)) => {
            pause_if_waiting(write, PauseReason::TurnFailed { turn_id, error })
        }
        (Stop::Ordinary, None) => Ok(()),
    }
}
