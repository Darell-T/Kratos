//! How a turn ends: its outcome, the context it used (D76), and whether it stopped at the
//! account's usage limit (D39). One write records all three, so the log never shows a turn that
//! ended without the pause that explains why nothing follows it.

use kratos_claude_client::{TurnContext, TurnOutcome as ClaudeOutcome, TurnResult};
use kratos_protocol::{ContextUsage, PauseReason, Timestamp, TurnId, TurnOutcome};
use kratos_store::{StoreError, ThreadWrite};

use super::threads::Rejection;
use crate::providers::queue::pause_if_waiting;

/// Whether the turn stopped because the account ran out of usage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Stop {
    Ordinary,
    /// The turn failed and the latest `rate_limit_event` said the server refused the included
    /// quota while no extra usage paid. Only the end of the turn decides this: an event alone
    /// changes nothing, and a turn that goes on to succeed was one the CLI waited out and resumed
    /// by itself.
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

    /// The end a result reports. `rejection` is what the latest rate limit event refused, and
    /// `answering` the model that wrote the turn's last reply, which names the context window.
    /// A turn Kratos was asked to interrupt (`stopped`) is never a limit stop, whatever its result
    /// says: the user's Stop must not become an automatic continuation.
    pub(super) fn of(
        result: &TurnResult,
        rejection: Option<&Rejection>,
        answering: Option<&str>,
        stopped: bool,
    ) -> TurnEnd {
        let outcome = match &result.outcome {
            ClaudeOutcome::Completed => TurnOutcome::Completed,
            ClaudeOutcome::Interrupted => TurnOutcome::Interrupted,
            ClaudeOutcome::Failed { error } => TurnOutcome::Failed {
                error: error.clone(),
            },
        };
        let stop = match (&outcome, rejection) {
            (TurnOutcome::Failed { .. }, Some(refused))
                if !refused.is_using_overage && !stopped =>
            {
                Stop::UsageLimit {
                    resets_at: refused.resets_at.clone(),
                }
            }
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
    let input_tokens = last
        .input_tokens
        .checked_add(last.cache_creation_input_tokens)?
        .checked_add(last.cache_read_input_tokens)?;
    Some(ContextUsage {
        input_tokens,
        cached_input_tokens: last.cache_read_input_tokens,
        output_tokens: last.output_tokens,
        reasoning_output_tokens: 0,
        total_tokens: input_tokens.checked_add(last.output_tokens)?,
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use kratos_claude_client::IterationTokens;

    use super::*;

    #[test]
    fn token_counts_that_overflow_give_no_context_and_never_a_panic() {
        // Catches a result with absurd counts killing the actor in an addition, which would stop
        // every Claude thread.
        let context = TurnContext {
            last: Some(IterationTokens {
                input_tokens: i64::MAX,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 1,
                output_tokens: 1,
            }),
            windows: BTreeMap::new(),
        };

        assert_eq!(context_usage(&context, None), None);
    }
}
