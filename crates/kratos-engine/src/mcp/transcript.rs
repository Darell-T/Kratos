//! A thread's events as the short lines `read_thread` returns: who said what, which commands ran,
//! and what the thread waits on.

use kratos_protocol::{AgentItem, MessageInput, MessageOrigin, ThreadEventBody};
use serde::Serialize;

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
            Some(format!("turn ended: {}", wire_name(outcome)))
        }
        ThreadEventBody::ApprovalRequested { approval } => {
            Some(format!("waiting for approval {}", approval.id))
        }
        ThreadEventBody::QuestionAsked { question } => {
            Some(format!("waiting for an answer to question {}", question.id))
        }
        ThreadEventBody::ThreadPaused { reason } => Some(format!("paused: {}", wire_name(reason))),
        _ => None,
    }
}

pub fn reply(event: &ThreadEventBody) -> Option<&str> {
    match event {
        ThreadEventBody::ItemCompleted {
            item: AgentItem::Message { text, .. },
            ..
        } => Some(text),
        _ => None,
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

fn wire_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        Ok(other) => other.to_string(),
        Err(_) => "unknown".to_owned(),
    }
}
