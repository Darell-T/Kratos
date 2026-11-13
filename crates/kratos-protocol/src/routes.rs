//! The engine's HTTP routes: each path as the router declares it, with `{…}` for an id, and a
//! builder per route that fills its ids in for a client, percent-encoding each one.

use crate::{ApprovalId, ContinuationId, MessageId, QuestionId, Seq, ThreadId, WorkspaceId};

pub const HEALTH: &str = "/api/health";
pub const EVENTS: &str = "/api/events";
pub const USAGE: &str = "/api/usage";
pub const CLAUDE_CLI: &str = "/api/agents/claude/cli";
pub const CLAUDE_CLI_UPDATE: &str = "/api/agents/claude/cli/update";
pub const WORKSPACES: &str = "/api/workspaces";
pub const THREADS: &str = "/api/threads";
pub const THREAD: &str = "/api/threads/{id}";
pub const THREAD_EVENTS: &str = "/api/threads/{id}/events";
pub const THREAD_MESSAGES: &str = "/api/threads/{id}/messages";
pub const THREAD_STOP: &str = "/api/threads/{id}/stop";
pub const THREAD_RESUME: &str = "/api/threads/{id}/resume";
pub const THREAD_SETTINGS: &str = "/api/threads/{id}/settings";
pub const THREAD_SKILLS: &str = "/api/threads/{id}/skills";
pub const MESSAGE: &str = "/api/threads/{id}/messages/{message_id}";
pub const MESSAGE_STEER: &str = "/api/threads/{id}/messages/{message_id}/steer";
pub const MESSAGE_RETRY: &str = "/api/threads/{id}/messages/{message_id}/retry";
pub const MESSAGE_DISCARD: &str = "/api/threads/{id}/messages/{message_id}/discard";
pub const APPROVAL: &str = "/api/approvals/{id}";
pub const QUESTION: &str = "/api/questions/{id}";
pub const CONTINUATION_SCHEDULE: &str = "/api/continuations/{id}/schedule";

pub fn threads_in(workspace_id: &WorkspaceId) -> String {
    format!("{THREADS}?workspaceId={}", encode(workspace_id.as_str()))
}

pub fn thread(id: &ThreadId) -> String {
    fill(THREAD, &[id.as_str()])
}

pub fn thread_events_after(id: &ThreadId, after: Seq) -> String {
    format!(
        "{}?after={}",
        fill(THREAD_EVENTS, &[id.as_str()]),
        after.get()
    )
}

pub fn thread_messages(id: &ThreadId) -> String {
    fill(THREAD_MESSAGES, &[id.as_str()])
}

pub fn thread_stop(id: &ThreadId) -> String {
    fill(THREAD_STOP, &[id.as_str()])
}

pub fn thread_resume(id: &ThreadId) -> String {
    fill(THREAD_RESUME, &[id.as_str()])
}

pub fn thread_settings(id: &ThreadId) -> String {
    fill(THREAD_SETTINGS, &[id.as_str()])
}

pub fn thread_skills(id: &ThreadId) -> String {
    fill(THREAD_SKILLS, &[id.as_str()])
}

pub fn message(thread_id: &ThreadId, message_id: &MessageId) -> String {
    fill(MESSAGE, &[thread_id.as_str(), message_id.as_str()])
}

pub fn message_steer(thread_id: &ThreadId, message_id: &MessageId) -> String {
    fill(MESSAGE_STEER, &[thread_id.as_str(), message_id.as_str()])
}

pub fn message_retry(thread_id: &ThreadId, message_id: &MessageId) -> String {
    fill(MESSAGE_RETRY, &[thread_id.as_str(), message_id.as_str()])
}

pub fn message_discard(thread_id: &ThreadId, message_id: &MessageId) -> String {
    fill(MESSAGE_DISCARD, &[thread_id.as_str(), message_id.as_str()])
}

pub fn approval(id: &ApprovalId) -> String {
    fill(APPROVAL, &[id.as_str()])
}

pub fn question(id: &QuestionId) -> String {
    fill(QUESTION, &[id.as_str()])
}

pub fn continuation_schedule(id: &ContinuationId) -> String {
    fill(CONTINUATION_SCHEDULE, &[id.as_str()])
}

/// `pattern` with its `{…}` placeholders replaced, in order, by `ids`, each percent-encoded.
fn fill(pattern: &str, ids: &[&str]) -> String {
    let mut path = String::new();
    let mut rest = pattern;
    for id in ids {
        let (before, placeholder) = rest.split_once('{').expect("a placeholder per id");
        let (_, after) = placeholder.split_once('}').expect("a closed placeholder");
        path.push_str(before);
        path.push_str(&encode(id));
        rest = after;
    }
    assert!(
        !rest.contains('{'),
        "an id for every placeholder in {pattern}"
    );
    path.push_str(rest);
    path
}

/// Every byte but the unreserved characters (`A-Z a-z 0-9 - . _ ~`), as `%XX`.
fn encode(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builder_fills_each_id_into_its_route_encoded() {
        // Catches an id with a reserved character that splits the path or the query, and a
        // builder that fills the ids into the wrong placeholders.
        let thread_id = ThreadId::new("a/b c".to_owned());
        let message_id = MessageId::new("m?1".to_owned());
        assert_eq!(
            message_steer(&thread_id, &message_id),
            "/api/threads/a%2Fb%20c/messages/m%3F1/steer"
        );
        assert_eq!(
            thread_events_after(&thread_id, Seq::new(7)),
            "/api/threads/a%2Fb%20c/events?after=7"
        );
        assert_eq!(
            threads_in(&WorkspaceId::new("w&x=1".to_owned())),
            "/api/threads?workspaceId=w%26x%3D1"
        );
        assert_eq!(
            approval(&ApprovalId::new("é".to_owned())),
            "/api/approvals/%C3%A9"
        );
    }

    #[test]
    fn every_builder_names_as_many_ids_as_its_route_has_placeholders() {
        // Catches a builder paired with a route of another shape, which would panic on its first
        // call instead of in a test.
        let (thread_id, message_id) = (
            ThreadId::new("t".to_owned()),
            MessageId::new("m".to_owned()),
        );
        let built = [
            (THREAD, thread(&thread_id)),
            (THREAD_MESSAGES, thread_messages(&thread_id)),
            (THREAD_STOP, thread_stop(&thread_id)),
            (THREAD_RESUME, thread_resume(&thread_id)),
            (THREAD_SETTINGS, thread_settings(&thread_id)),
            (THREAD_SKILLS, thread_skills(&thread_id)),
            (MESSAGE, message(&thread_id, &message_id)),
            (MESSAGE_STEER, message_steer(&thread_id, &message_id)),
            (MESSAGE_RETRY, message_retry(&thread_id, &message_id)),
            (MESSAGE_DISCARD, message_discard(&thread_id, &message_id)),
            (APPROVAL, approval(&ApprovalId::new("t".to_owned()))),
            (QUESTION, question(&QuestionId::new("t".to_owned()))),
            (
                CONTINUATION_SCHEDULE,
                continuation_schedule(&ContinuationId::new("t".to_owned())),
            ),
        ];
        for (route, path) in built {
            let expected = route.replace("{id}", "t").replace("{message_id}", "m");
            assert_eq!(path, expected);
        }
    }
}
