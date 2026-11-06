//! A thread's events as the short lines `read_thread` returns: who said what, which commands ran,
//! and what the thread waits on.

use kratos_protocol::{
    AgentItem, MessageInput, MessageOrigin, PauseReason, ThreadEventBody, TurnOutcome,
};

pub const MAX_TRANSCRIPT_BYTES: usize = 16 * 1024;

pub fn line(event: &ThreadEventBody) -> Option<String> {
    match event {
        ThreadEventBody::UserMessage { text } => Some(format!("user: {text}")),
        ThreadEventBody::MessageQueued { input, origin, .. } => {
            let writer = match origin {
                MessageOrigin::Thread { thread_id, .. } => format!("thread {}", thread_id.as_str()),
                MessageOrigin::User | MessageOrigin::Continuation { .. } => "user".to_owned(),
            };
            Some(format!("{writer}: {}", input_text(input)))
        }
        ThreadEventBody::ItemCompleted { item, .. } => item_line(item),
        ThreadEventBody::TurnCompleted { outcome, .. } => {
            Some(format!("turn ended: {}", outcome_text(outcome)))
        }
        ThreadEventBody::ApprovalRequested { approval } => {
            Some(format!("waiting for approval {}", approval.id))
        }
        ThreadEventBody::QuestionAsked { question } => {
            Some(format!("waiting for an answer to question {}", question.id))
        }
        ThreadEventBody::ThreadPaused { reason } => Some(format!("paused: {}", pause_text(reason))),
        ThreadEventBody::MessageStatusChanged { .. }
        | ThreadEventBody::TurnStarted { .. }
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

pub fn newest(lines: &[String]) -> String {
    let mut used = 0;
    let kept = lines
        .iter()
        .rev()
        .take_while(|line| {
            used += line.len() + 1;
            used <= MAX_TRANSCRIPT_BYTES
        })
        .count();
    lines[lines.len() - kept..].join("\n")
}

fn item_line(item: &AgentItem) -> Option<String> {
    match item {
        AgentItem::Message { text, .. } => Some(format!("agent: {text}")),
        AgentItem::Plan { text, .. } => Some(format!("plan: {text}")),
        AgentItem::Command {
            command, exit_code, ..
        } => Some(match exit_code {
            Some(code) => format!("ran: {command} (exit {code})"),
            None => format!("ran: {command}"),
        }),
        AgentItem::FileChanges { changes, .. } => {
            let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
            Some(format!("changed: {}", paths.join(", ")))
        }
        AgentItem::Other { .. } => None,
    }
}

fn input_text(input: &[MessageInput]) -> String {
    let parts: Vec<String> = input
        .iter()
        .map(|part| match part {
            MessageInput::Text { text } => text.as_str().to_owned(),
            MessageInput::Image { path } => format!("[image {}]", path.as_str()),
            MessageInput::File { path } => format!("[file {}]", path.as_str()),
        })
        .collect();
    parts.join(" ")
}

fn outcome_text(outcome: &TurnOutcome) -> String {
    match outcome {
        TurnOutcome::Completed => "completed".to_owned(),
        TurnOutcome::Interrupted => "interrupted".to_owned(),
        TurnOutcome::Failed { error } => format!("failed, {error}"),
    }
}

fn pause_text(reason: &PauseReason) -> String {
    match reason {
        PauseReason::TurnFailed { error, .. } => format!("the last turn failed, {error}"),
        PauseReason::TimedOut { .. } => "the last turn ran past its deadline".to_owned(),
        PauseReason::Interrupted => "the last turn did not finish".to_owned(),
        PauseReason::Refused { message_id, error } => {
            format!("the provider refused message {message_id}, {error}")
        }
        PauseReason::DeliveryUncertain { message_id, error } => {
            format!("no one knows whether the provider got message {message_id}, {error}")
        }
        PauseReason::Unavailable { error } => format!("the provider is unavailable, {error}"),
        PauseReason::UsageLimit {
            resets_at: Some(resets_at),
            ..
        } => format!("usage limit until {}", resets_at.as_str()),
        PauseReason::UsageLimit {
            resets_at: None, ..
        } => "usage limit".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kratos_protocol::{Timestamp, TurnChanges, TurnId};

    #[test]
    fn a_transcript_names_how_a_turn_ended_and_why_a_thread_waits_in_words() {
        // Catches the JSON object of a tagged enum, such as {"outcome":"completed"}, printed where
        // the reading agent expects a word, and an error left out of a failed turn's line.
        let ended = |outcome| ThreadEventBody::TurnCompleted {
            turn_id: TurnId::new("turn".to_owned()),
            outcome,
            changes: TurnChanges {
                files: Vec::new(),
                edited_since_command: false,
                ran_command: false,
            },
            context: None,
        };
        assert_eq!(
            line(&ended(TurnOutcome::Completed)).as_deref(),
            Some("turn ended: completed")
        );
        assert_eq!(
            line(&ended(TurnOutcome::Failed {
                error: "boom".to_owned()
            }))
            .as_deref(),
            Some("turn ended: failed, boom")
        );
        let paused = ThreadEventBody::ThreadPaused {
            reason: PauseReason::UsageLimit {
                turn_id: TurnId::new("turn".to_owned()),
                resets_at: Some(
                    Timestamp::from_stored("2026-10-30T12:00:00.000Z".to_owned()).unwrap(),
                ),
            },
        };
        assert_eq!(
            line(&paused).as_deref(),
            Some("paused: usage limit until 2026-10-30T12:00:00.000Z")
        );
    }
}
