//! `wait_for_turn`'s wait: from a thread's details, then its events, until the thread needs
//! nothing from the waiting agent's turn to go on.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use kratos_api_client::ClientError;
use kratos_protocol::{
    Approval, PauseReason, Question, Seq, ThreadDetails, ThreadEventBody, ThreadId, TurnOutcome,
};
use serde::Serialize;

use super::{Call, transcript};

/// How often a wait checks whether Codex cancelled the call.
const CANCEL_CHECK: Duration = Duration::from_secs(1);

#[derive(Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WaitOutcome {
    Idle,
    WaitingForInput {
        approvals: Vec<Approval>,
        questions: Vec<Question>,
    },
    Paused {
        reason: PauseReason,
    },
    TurnEnded {
        outcome: TurnOutcome,
        reply: Option<String>,
    },
    TimedOut {
        last_seq: Seq,
    },
}

/// Waits from `details` until the thread settles, Codex cancels the call, or `timeout` passes.
pub fn wait(
    call: &Call,
    thread_id: ThreadId,
    details: ThreadDetails,
    timeout: Duration,
) -> Result<WaitOutcome, String> {
    let last_seq = details.last_seq;
    if let Some(outcome) = settled(details) {
        return Ok(outcome);
    }
    follow(call, thread_id, last_seq, Instant::now() + timeout)
}

/// Follows the thread's events after `after` until one ends the wait, Codex cancels the call, or
/// `deadline` passes.
fn follow(
    call: &Call,
    thread_id: ThreadId,
    after: Seq,
    deadline: Instant,
) -> Result<WaitOutcome, String> {
    let mut follower = call.caller.client.follow(thread_id, after);
    let mut last = after;
    let mut reply = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if call.cancelled.load(Ordering::Relaxed) {
            return Err("Cancelled".to_owned());
        }
        let event = match follower.next_event(left.min(CANCEL_CHECK)) {
            Ok(event) => event,
            Err(ClientError::TimedOut(_)) => continue,
            Err(error) => return Err(format!("Kratos is unreachable: {error}")),
        };
        last = event.seq;
        if let Some(text) = transcript::reply(&event.event) {
            reply = Some(text.to_owned());
        }
        if let Some(outcome) = ended(event.event, reply.as_deref()) {
            return Ok(outcome);
        }
    }
    Ok(WaitOutcome::TimedOut { last_seq: last })
}

fn settled(details: ThreadDetails) -> Option<WaitOutcome> {
    if !details.approvals.is_empty() || !details.questions.is_empty() {
        return Some(WaitOutcome::WaitingForInput {
            approvals: details.approvals,
            questions: details.questions,
        });
    }
    if let Some(reason) = details.paused {
        return Some(WaitOutcome::Paused { reason });
    }
    (details.running_turn.is_none() && details.queue.is_empty()).then_some(WaitOutcome::Idle)
}

fn ended(event: ThreadEventBody, reply: Option<&str>) -> Option<WaitOutcome> {
    match event {
        ThreadEventBody::TurnCompleted { outcome, .. } => Some(WaitOutcome::TurnEnded {
            outcome,
            reply: reply.map(str::to_owned),
        }),
        ThreadEventBody::ApprovalRequested { approval } => Some(WaitOutcome::WaitingForInput {
            approvals: vec![approval],
            questions: Vec::new(),
        }),
        ThreadEventBody::QuestionAsked { question } => Some(WaitOutcome::WaitingForInput {
            approvals: Vec::new(),
            questions: vec![question],
        }),
        ThreadEventBody::ThreadPaused { reason } => Some(WaitOutcome::Paused { reason }),
        ThreadEventBody::UserMessage { .. }
        | ThreadEventBody::MessageQueued { .. }
        | ThreadEventBody::MessageStatusChanged { .. }
        | ThreadEventBody::TurnStarted { .. }
        | ThreadEventBody::ItemCompleted { .. }
        | ThreadEventBody::ApprovalDecided { .. }
        | ThreadEventBody::ApprovalClosed { .. }
        | ThreadEventBody::QuestionAnswered { .. }
        | ThreadEventBody::QuestionClosed { .. }
        | ThreadEventBody::ThreadResumed
        | ThreadEventBody::SettingsChanged { .. }
        | ThreadEventBody::ProviderError { .. }
        | ThreadEventBody::ContinuationChanged { .. }
        | ThreadEventBody::ProviderSession { .. } => None,
    }
}
