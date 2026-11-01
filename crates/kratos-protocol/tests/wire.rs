//! The wire types against literal JSON in the TypeScript engine's shapes.
//!
//! The engine and kratos-client share these types, so a wrong rename would round-trip between them
//! and pass every engine test. Only a comparison with the TypeScript text catches it.

use std::ops::RangeInclusive;

use kratos_protocol::{
    ClientMessage, CommandResult, CreateThread, ListThreads, Permission, Provider, RequiredText,
    Seq, ServerMessage, ThreadEvent, ThreadEventBody, ThreadId, Timestamp, TurnChanges,
    WorkspaceId,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap()
}

fn parse<T: DeserializeOwned>(text: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(text)
}

#[test]
fn socket_messages_match_the_typescript_json() {
    // Catches snake_case or untagged socket messages, which the TypeScript engine and the future UI
    // would not understand, and an `after` that accepts a negative or fractional cursor.
    let subscribe: ClientMessage =
        parse(r#"{"type":"subscribe","threadId":"t","after":2}"#).unwrap();
    assert_eq!(
        subscribe,
        ClientMessage::Subscribe {
            thread_id: ThreadId::new("t".into()),
            after: Seq::new(2)
        }
    );
    let unsubscribe: ClientMessage = parse(r#"{"type":"unsubscribe","threadId":"t"}"#).unwrap();
    assert_eq!(
        unsubscribe,
        ClientMessage::Unsubscribe {
            thread_id: ThreadId::new("t".into())
        }
    );
    for refused in [
        r#"{"type":"subscribe","threadId":"t","after":-1}"#,
        r#"{"type":"subscribe","threadId":"t","after":1.5}"#,
        r#"{"type":"subscribe","threadId":"t"}"#,
        r#"{"type":"resubscribe","threadId":"t","after":0}"#,
    ] {
        assert!(
            parse::<ClientMessage>(refused).is_err(),
            "{refused} should not parse"
        );
    }

    // A quote, a newline, U+2028, and an emoji: JSON.stringify escapes the first two and keeps the
    // others.
    let event = ServerMessage::Event {
        event: ThreadEvent {
            thread_id: ThreadId::new("t".into()),
            seq: Seq::new(1),
            at: Timestamp::from_stored("2030-01-01T00:00:00.000Z".into()),
            event: ThreadEventBody::UserMessage {
                text: "hi \"there\"\n\u{2028}é🚀".into(),
            },
        },
    };
    assert_eq!(
        json(&event),
        "{\"type\":\"event\",\"event\":{\"threadId\":\"t\",\"seq\":1,\"at\":\"2030-01-01T00:00:00.000Z\",\
         \"event\":{\"kind\":\"user.message\",\"text\":\"hi \\\"there\\\"\\n\u{2028}é🚀\"}}}"
    );
    let error = ServerMessage::Error {
        message: "Unknown thread nope".into(),
    };
    assert_eq!(
        json(&error),
        r#"{"type":"error","message":"Unknown thread nope"}"#
    );
}

#[test]
fn agent_types_match_the_typescript_json() {
    // Catches variant fields left in snake_case (`exit_code`), which would break the Teams rules'
    // replay of recorded TypeScript facts, and a provider name that drifts from the database's
    // CHECK constraint.
    assert_eq!(json(&Permission::ApproveForMe), r#""approveForMe""#);
    assert_eq!(
        parse::<Permission>(r#""askMe""#).unwrap(),
        Permission::AskMe
    );
    assert_eq!(
        parse::<Permission>(r#""fullAccess""#).unwrap(),
        Permission::FullAccess
    );

    let changes = TurnChanges {
        files: vec!["a.rs".into()],
        edited_since_command: true,
        ran_command: false,
    };
    let changes_json = r#"{"files":["a.rs"],"editedSinceCommand":true,"ranCommand":false}"#;
    assert_eq!(json(&changes), changes_json);
    assert_eq!(parse::<TurnChanges>(changes_json).unwrap(), changes);

    let results = [
        (
            CommandResult::Exited {
                exit_code: -1073741819,
                stdout: String::new(),
                stderr: "boom".into(),
            },
            r#"{"kind":"exited","exitCode":-1073741819,"stdout":"","stderr":"boom"}"#,
        ),
        (
            CommandResult::TimedOut {
                output: "tail".into(),
            },
            r#"{"kind":"timedOut","output":"tail"}"#,
        ),
        (
            CommandResult::Refused {
                error: "outside".into(),
            },
            r#"{"kind":"refused","error":"outside"}"#,
        ),
    ];
    for (result, result_json) in results {
        assert_eq!(json(&result), result_json);
        assert_eq!(parse::<CommandResult>(result_json).unwrap(), result);
    }

    for provider in [Provider::Codex, Provider::Claude] {
        assert_eq!(json(&provider), format!("\"{}\"", provider.as_str()));
        assert_eq!(Provider::from_name(provider.as_str()), Some(provider));
    }
    assert_eq!(Provider::from_name("gemini"), None);
}

#[test]
fn required_text_trims_exactly_what_javascript_trims() {
    // Catches `str::trim`, which keeps U+FEFF and strips U+0085, so a client could post text the
    // TypeScript engine would refuse or store differently. Scanning Node 24 over all of Unicode
    // found these characters.
    const REMOVED_BY_JAVASCRIPT: [RangeInclusive<u32>; 10] = [
        0x9..=0xd,
        0x20..=0x20,
        0xa0..=0xa0,
        0x1680..=0x1680,
        0x2000..=0x200a,
        0x2028..=0x2029,
        0x202f..=0x202f,
        0x205f..=0x205f,
        0x3000..=0x3000,
        0xfeff..=0xfeff,
    ];
    for character in (0..=char::MAX as u32).filter_map(char::from_u32) {
        let removed = REMOVED_BY_JAVASCRIPT
            .iter()
            .any(|range| range.contains(&(character as u32)));
        let is_blank = RequiredText::new(&character.to_string()).is_none();
        assert_eq!(is_blank, removed, "U+{:04X}", character as u32);
    }

    let trimmed = RequiredText::new("\u{feff} \thello  world\r\n\u{a0}").unwrap();
    assert_eq!(trimmed.as_str(), "hello  world");
}

#[test]
fn ids_follow_the_typescript_schemas() {
    // Catches ids that are trimmed or refused uniformly. In TypeScript the thread-creation body
    // trims its workspace id (`requiredText`), the listing query only refuses an empty one
    // (`z.string().min(1)`), and a socket or path thread id may be any string.
    let create: CreateThread =
        parse(r#"{"workspaceId":"  w  ","title":" chat ","provider":"codex"}"#).unwrap();
    assert_eq!(create.workspace_id, WorkspaceId::new("w".into()));
    assert_eq!(create.title.as_str(), "chat");
    for refused in [
        r#"{"workspaceId":"   ","title":"t","provider":"codex"}"#,
        r#"{"workspaceId":"w","title":" ","provider":"codex"}"#,
        r#"{"workspaceId":"w","title":"t","provider":"gemini"}"#,
        r#"{"workspaceId":"w","provider":"codex"}"#,
    ] {
        assert!(
            parse::<CreateThread>(refused).is_err(),
            "{refused} should not parse"
        );
    }

    let query: ListThreads = parse(r#"{"workspaceId":" w "}"#).unwrap();
    assert_eq!(query.workspace_id, WorkspaceId::new(" w ".into()));
    assert!(parse::<ListThreads>(r#"{"workspaceId":""}"#).is_err());

    assert_eq!(
        parse::<ThreadId>(r#""""#).unwrap(),
        ThreadId::new(String::new())
    );
}

#[test]
fn timestamps_have_the_javascript_iso_shape() {
    // Catches `YYYY-MM-DDTHH:MM:SSZ` or nanosecond precision, which would sort wrongly against the
    // rows the TypeScript engine wrote and differ from what `Date.toISOString()` produces.
    let now = Timestamp::now();
    let text = now.as_str();
    assert_eq!(text.len(), 24, "{text}");
    for (index, character) in text.bytes().enumerate() {
        match index {
            4 | 7 => assert_eq!(character, b'-', "{text}"),
            10 => assert_eq!(character, b'T', "{text}"),
            13 | 16 => assert_eq!(character, b':', "{text}"),
            19 => assert_eq!(character, b'.', "{text}"),
            23 => assert_eq!(character, b'Z', "{text}"),
            _ => assert!(character.is_ascii_digit(), "{text}"),
        }
    }
}
