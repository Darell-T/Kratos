//! The R3 event kinds and request bodies against literal JSON.
//!
//! Stored events outlive the build that wrote them, so a renamed field or kind leaves older rows
//! unreadable. Round-trips between two builds of the same types would not notice; literal text does.

use kratos_protocol::{
    AgentItem, Approval, ApprovalDecision, ApprovalId, ApprovalRequest, AskReason, CappedText,
    ContextUsage, Continuation, ContinuationId, ContinuationState, FileChange, FileChangeKind,
    ItemStatus, LiveUpdate, Message, MessageId, MessageInput, MessageOrigin, MessageStatus,
    PauseReason, Permission, Provider, Question, QuestionId, QuestionItem, QuestionOption,
    ReplySoFar, RequiredText, SendMessage, ServerMessage, ThreadEventBody, ThreadId,
    ThreadSettings, Timestamp, TurnChanges, TurnId, TurnOutcome, UsageWait,
};
use std::fmt::Debug;

use serde::Serialize;
use serde::de::DeserializeOwned;

fn text(value: &str) -> RequiredText {
    RequiredText::new(value).unwrap()
}

fn turn() -> TurnId {
    TurnId::new("turn-1".into())
}

fn message() -> MessageId {
    MessageId::new("m-1".into())
}

#[track_caller]
fn assert_wire<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: T, literal: &str) {
    assert_eq!(serde_json::to_string(&value).unwrap(), literal);
    assert_eq!(serde_json::from_str::<T>(literal).unwrap(), value);
}

#[test]
fn message_events_keep_their_stored_shape() {
    assert_wire(
        ThreadEventBody::MessageQueued {
            message_id: message(),
            input: vec![
                MessageInput::Text {
                    text: text("fix it"),
                },
                MessageInput::Image {
                    path: text("C:\\shots\\bug.png"),
                },
                MessageInput::File {
                    path: text("C:\\repo\\notes.md"),
                },
            ],
            origin: MessageOrigin::User,
        },
        r#"{"kind":"message.queued","messageId":"m-1","input":[{"type":"text","text":"fix it"},{"type":"image","path":"C:\\shots\\bug.png"},{"type":"file","path":"C:\\repo\\notes.md"}],"by":"user"}"#,
    );
    assert_wire(
        ThreadEventBody::MessageQueued {
            message_id: message(),
            input: vec![MessageInput::Text {
                text: text("continue"),
            }],
            origin: MessageOrigin::Continuation {
                continuation_id: ContinuationId::new("c-1".into()),
            },
        },
        r#"{"kind":"message.queued","messageId":"m-1","input":[{"type":"text","text":"continue"}],"by":"continuation","continuationId":"c-1"}"#,
    );
    let statuses = [
        (MessageStatus::Queued, r#""status":"queued""#),
        (MessageStatus::Submitting, r#""status":"submitting""#),
        (
            MessageStatus::Accepted { turn_id: turn() },
            r#""status":"accepted","turnId":"turn-1""#,
        ),
        (
            MessageStatus::Failed {
                error: "refused".into(),
            },
            r#""status":"failed","error":"refused""#,
        ),
        (
            MessageStatus::Unknown {
                error: "Codex exited".into(),
            },
            r#""status":"unknown","error":"Codex exited""#,
        ),
        (MessageStatus::Cancelled, r#""status":"cancelled""#),
    ];
    for (status, fields) in statuses {
        assert_wire(
            ThreadEventBody::MessageStatusChanged {
                message_id: message(),
                status,
            },
            &format!(r#"{{"kind":"message.status","messageId":"m-1",{fields}}}"#),
        );
    }
}

#[test]
fn a_queued_message_reports_its_origin_and_status_beside_its_input() {
    assert_wire(
        Message {
            id: message(),
            input: vec![MessageInput::Text { text: text("go") }],
            origin: MessageOrigin::Continuation {
                continuation_id: ContinuationId::new("c-1".into()),
            },
            status: MessageStatus::Accepted { turn_id: turn() },
            queued_at: Timestamp::from_stored("2030-01-01T00:00:00.000Z".into()),
        },
        r#"{"id":"m-1","input":[{"type":"text","text":"go"}],"by":"continuation","continuationId":"c-1","status":"accepted","turnId":"turn-1","queuedAt":"2030-01-01T00:00:00.000Z"}"#,
    );
}

#[test]
fn turn_events_keep_their_stored_shape() {
    assert_wire(
        ThreadEventBody::TurnStarted { turn_id: turn() },
        r#"{"kind":"turn.started","turnId":"turn-1"}"#,
    );
    assert_wire(
        ThreadEventBody::ItemCompleted {
            turn_id: turn(),
            item: AgentItem::Command {
                id: "i-1".into(),
                command: "cargo test".into(),
                cwd: Some("C:\\repo".into()),
                status: ItemStatus::Failed,
                exit_code: Some(101),
                output: CappedText::new("error: boom", 5),
            },
        },
        r#"{"kind":"item.completed","turnId":"turn-1","item":{"type":"command","id":"i-1","command":"cargo test","cwd":"C:\\repo","status":"failed","exitCode":101,"output":{"text":"error","omittedBytes":6}}}"#,
    );
    assert_wire(
        ThreadEventBody::ItemCompleted {
            turn_id: turn(),
            item: AgentItem::FileChanges {
                id: "i-2".into(),
                status: ItemStatus::Completed,
                changes: vec![
                    FileChange {
                        path: "a.rs".into(),
                        kind: FileChangeKind::Update {
                            move_path: Some("b.rs".into()),
                        },
                        diff: CappedText::new("@@ -1 +1 @@", 64),
                    },
                    FileChange {
                        path: "c.rs".into(),
                        kind: FileChangeKind::Delete,
                        diff: CappedText::new("", 64),
                    },
                ],
            },
        },
        r#"{"kind":"item.completed","turnId":"turn-1","item":{"type":"fileChanges","id":"i-2","status":"completed","changes":[{"path":"a.rs","kind":"update","movePath":"b.rs","diff":{"text":"@@ -1 +1 @@","omittedBytes":0}},{"path":"c.rs","kind":"delete","diff":{"text":"","omittedBytes":0}}]}}"#,
    );
    assert_wire(
        ThreadEventBody::ItemCompleted {
            turn_id: turn(),
            item: AgentItem::Message {
                id: "i-3".into(),
                text: "Done.".into(),
            },
        },
        r#"{"kind":"item.completed","turnId":"turn-1","item":{"type":"message","id":"i-3","text":"Done."}}"#,
    );
    assert_wire(
        ThreadEventBody::ItemCompleted {
            turn_id: turn(),
            item: AgentItem::Other {
                id: "i-4".into(),
                provider_type: "webSearch".into(),
            },
        },
        r#"{"kind":"item.completed","turnId":"turn-1","item":{"type":"other","id":"i-4","providerType":"webSearch"}}"#,
    );
    assert_wire(
        ThreadEventBody::TurnCompleted {
            turn_id: turn(),
            outcome: TurnOutcome::Failed {
                error: "model overloaded".into(),
            },
            changes: TurnChanges {
                files: vec!["a.rs".into()],
                edited_since_command: true,
                ran_command: false,
            },
            context: Some(ContextUsage {
                input_tokens: 1200,
                cached_input_tokens: 1000,
                output_tokens: 80,
                reasoning_output_tokens: 20,
                total_tokens: 1300,
                context_window: Some(258400),
            }),
        },
        r#"{"kind":"turn.completed","turnId":"turn-1","outcome":"failed","error":"model overloaded","changes":{"files":["a.rs"],"editedSinceCommand":true,"ranCommand":false},"context":{"inputTokens":1200,"cachedInputTokens":1000,"outputTokens":80,"reasoningOutputTokens":20,"totalTokens":1300,"contextWindow":258400}}"#,
    );
    assert_wire(
        ThreadEventBody::TurnCompleted {
            turn_id: turn(),
            outcome: TurnOutcome::Interrupted,
            changes: TurnChanges {
                files: Vec::new(),
                edited_since_command: false,
                ran_command: true,
            },
            context: None,
        },
        r#"{"kind":"turn.completed","turnId":"turn-1","outcome":"interrupted","changes":{"files":[],"editedSinceCommand":false,"ranCommand":true},"context":null}"#,
    );
}

#[test]
fn approval_and_question_events_keep_their_stored_shape() {
    let approval = Approval {
        id: ApprovalId::new("a-1".into()),
        turn_id: turn(),
        request: ApprovalRequest::Command {
            command: "rm -rf target".into(),
            cwd: Some("/repo".into()),
            reason: None,
        },
        why: AskReason::Dangerous,
    };
    assert_wire(
        ThreadEventBody::ApprovalRequested { approval },
        r#"{"kind":"approval.requested","approval":{"id":"a-1","turnId":"turn-1","request":{"type":"command","command":"rm -rf target","cwd":"/repo","reason":null},"why":{"reason":"dangerous"}}}"#,
    );
    let protected = Approval {
        id: ApprovalId::new("a-2".into()),
        turn_id: turn(),
        request: ApprovalRequest::FileChanges {
            changes: vec![FileChange {
                path: "/home/u/.ssh/config".into(),
                kind: FileChangeKind::Add,
                diff: CappedText::new("Host *", 64),
            }],
            reason: Some("set up ssh".into()),
        },
        why: AskReason::ProtectedFolder {
            path: "/home/u/.ssh".into(),
        },
    };
    assert_wire(
        ThreadEventBody::ApprovalRequested {
            approval: protected,
        },
        r#"{"kind":"approval.requested","approval":{"id":"a-2","turnId":"turn-1","request":{"type":"fileChanges","changes":[{"path":"/home/u/.ssh/config","kind":"add","diff":{"text":"Host *","omittedBytes":0}}],"reason":"set up ssh"},"why":{"reason":"protectedFolder","path":"/home/u/.ssh"}}}"#,
    );
    assert_wire(
        ThreadEventBody::ApprovalDecided {
            approval_id: ApprovalId::new("a-1".into()),
            decision: ApprovalDecision::AcceptForSession,
        },
        r#"{"kind":"approval.decided","approvalId":"a-1","decision":"acceptForSession"}"#,
    );
    assert_wire(
        ThreadEventBody::ApprovalClosed {
            approval_id: ApprovalId::new("a-1".into()),
            reason: "The turn ended".into(),
        },
        r#"{"kind":"approval.closed","approvalId":"a-1","reason":"The turn ended"}"#,
    );
    let question = Question {
        id: QuestionId::new("q-1".into()),
        turn_id: turn(),
        items: vec![QuestionItem {
            id: "db".into(),
            header: "Database".into(),
            question: "Which database?".into(),
            options: vec![QuestionOption {
                label: "SQLite".into(),
                description: "One file".into(),
            }],
            allows_other: true,
            secret: false,
        }],
        blocking: true,
    };
    assert_wire(
        ThreadEventBody::QuestionAsked { question },
        r#"{"kind":"question.asked","question":{"id":"q-1","turnId":"turn-1","items":[{"id":"db","header":"Database","question":"Which database?","options":[{"label":"SQLite","description":"One file"}],"allowsOther":true,"secret":false}],"blocking":true}}"#,
    );
    assert_wire(
        ThreadEventBody::QuestionAnswered {
            question_id: QuestionId::new("q-1".into()),
        },
        r#"{"kind":"question.answered","questionId":"q-1"}"#,
    );
    assert_wire(
        ThreadEventBody::QuestionClosed {
            question_id: QuestionId::new("q-1".into()),
            reason: "The turn ended".into(),
        },
        r#"{"kind":"question.closed","questionId":"q-1","reason":"The turn ended"}"#,
    );
}

#[test]
fn thread_and_provider_events_keep_their_stored_shape() {
    let pauses = [
        (
            PauseReason::TurnFailed {
                turn_id: turn(),
                error: "boom".into(),
            },
            r#""reason":"turnFailed","turnId":"turn-1","error":"boom""#,
        ),
        (
            PauseReason::TimedOut { turn_id: turn() },
            r#""reason":"timedOut","turnId":"turn-1""#,
        ),
        (PauseReason::Interrupted, r#""reason":"interrupted""#),
        (
            PauseReason::Refused {
                message_id: message(),
                error: "bad input".into(),
            },
            r#""reason":"refused","messageId":"m-1","error":"bad input""#,
        ),
        (
            PauseReason::DeliveryUncertain {
                message_id: message(),
                error: "Codex exited".into(),
            },
            r#""reason":"deliveryUncertain","messageId":"m-1","error":"Codex exited""#,
        ),
        (
            PauseReason::Unavailable {
                error: "codex not found".into(),
            },
            r#""reason":"unavailable","error":"codex not found""#,
        ),
        (
            PauseReason::UsageLimit {
                turn_id: turn(),
                resets_at: Some(Timestamp::from_stored("2030-01-01T05:00:00.000Z".into())),
            },
            r#""reason":"usageLimit","turnId":"turn-1","resetsAt":"2030-01-01T05:00:00.000Z""#,
        ),
    ];
    for (reason, fields) in pauses {
        assert_wire(
            ThreadEventBody::ThreadPaused { reason },
            &format!(r#"{{"kind":"thread.paused",{fields}}}"#),
        );
    }
    assert_wire(
        ThreadEventBody::ThreadResumed,
        r#"{"kind":"thread.resumed"}"#,
    );
    assert_wire(
        ThreadEventBody::SettingsChanged {
            settings: ThreadSettings {
                model: Some("gpt-5.4-mini".into()),
                permission: Permission::ApproveForMe,
                plan_mode: false,
                context_limit_tokens: Some(200000),
            },
        },
        r#"{"kind":"settings.changed","settings":{"model":"gpt-5.4-mini","permission":"approveForMe","planMode":false,"contextLimitTokens":200000}}"#,
    );
    assert_wire(
        ThreadEventBody::ProviderError {
            turn_id: turn(),
            message: "Reconnecting".into(),
            will_retry: true,
        },
        r#"{"kind":"provider.error","turnId":"turn-1","message":"Reconnecting","willRetry":true}"#,
    );
    assert_wire(
        ThreadEventBody::ContinuationChanged {
            continuation: Continuation {
                id: ContinuationId::new("c-1".into()),
                turn_id: turn(),
                waits_on: UsageWait {
                    provider: Provider::Codex,
                    account: Some("acct-1".into()),
                    limit_ids: vec!["codex".into()],
                },
                resets_at: None,
                state: ContinuationState::AwaitingUser {
                    why: "The reset time is unknown".into(),
                },
            },
        },
        r#"{"kind":"continuation.changed","continuation":{"id":"c-1","turnId":"turn-1","waitsOn":{"provider":"codex","account":"acct-1","limitIds":["codex"]},"resetsAt":null,"state":"awaitingUser","why":"The reset time is unknown"}}"#,
    );
    assert_wire(
        ThreadEventBody::ProviderSession {
            provider: Provider::Codex,
            plan: Some("plus".into()),
        },
        r#"{"kind":"provider.session","provider":"codex","plan":"plus"}"#,
    );
}

#[test]
fn live_updates_have_their_socket_shape() {
    let thread_id = ThreadId::new("t".into());
    assert_wire(
        ServerMessage::Live {
            thread_id: thread_id.clone(),
            update: LiveUpdate::ReplyDelta {
                turn_id: turn(),
                item_id: "i-1".into(),
                delta: "Hel".into(),
            },
        },
        r#"{"type":"live","threadId":"t","update":{"kind":"replyDelta","turnId":"turn-1","itemId":"i-1","delta":"Hel"}}"#,
    );
    assert_wire(
        ServerMessage::Live {
            thread_id,
            update: LiveUpdate::Snapshot {
                turn_id: Some(turn()),
                replies: vec![ReplySoFar {
                    item_id: "i-1".into(),
                    text: "Hello".into(),
                }],
            },
        },
        r#"{"type":"live","threadId":"t","update":{"kind":"snapshot","turnId":"turn-1","replies":[{"itemId":"i-1","text":"Hello"}]}}"#,
    );
}

#[test]
fn a_message_body_is_text_or_parts_with_an_optional_id() {
    // Catches a body that accepts both forms at once, an empty part list, or an id the store would
    // have to truncate, each of which would queue a message the client did not mean.
    let parse = |body: &str| serde_json::from_str::<SendMessage>(body).map(SendMessage::into_parts);

    let (id, input) = parse(r#"{"text":"  hi  "}"#).unwrap().unwrap();
    assert_eq!(id, None);
    assert_eq!(input, [MessageInput::Text { text: text("hi") }]);
    let (id, input) = parse(r#"{"messageId":"m-9","input":[{"type":"image","path":"C:\\a.png"}]}"#)
        .unwrap()
        .unwrap();
    assert_eq!(id, Some(MessageId::new("m-9".into())));
    assert_eq!(
        input,
        [MessageInput::Image {
            path: text("C:\\a.png")
        }]
    );

    assert!(
        parse(r#"{"text":"hi","input":[{"type":"text","text":"hi"}]}"#)
            .unwrap()
            .is_err()
    );
    assert!(parse(r#"{"input":[]}"#).unwrap().is_err());
    assert!(parse(r#"{"messageId":"m"}"#).unwrap().is_err());
    let longest = "x".repeat(128);
    assert!(parse(&format!(r#"{{"messageId":"{longest}","text":"hi"}}"#)).is_ok());
    for refused in [
        format!(r#"{{"messageId":"{longest}y","text":"hi"}}"#),
        r#"{"messageId":"","text":"hi"}"#.to_owned(),
        r#"{"text":"   "}"#.to_owned(),
        r#"{"text":"hi","priority":1}"#.to_owned(),
        r#"{"input":[{"type":"video","path":"a.mp4"}]}"#.to_owned(),
    ] {
        assert!(parse(&refused).is_err(), "{refused} should not parse");
    }
}

#[test]
fn thread_settings_refuse_unknown_fields() {
    // Catches a misspelled setting that would otherwise be dropped while the client believes it
    // applied, which D76 forbids.
    let settings: ThreadSettings = serde_json::from_str(
        r#"{"model":null,"permission":"fullAccess","planMode":true,"contextLimitTokens":null}"#,
    )
    .unwrap();
    assert_eq!(settings.permission, Permission::FullAccess);
    assert!(settings.plan_mode);
    assert!(
        serde_json::from_str::<ThreadSettings>(
            r#"{"permission":"askMe","planMode":false,"contextWindow":1}"#
        )
        .is_err()
    );
}
