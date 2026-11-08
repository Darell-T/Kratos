use kratos_protocol::{
    ApprovalId, ContinuationId, MessageId, MessageStatus, QuestionId, Seq, ThreadId, WorkspaceId,
};
use kratos_store::{Refusal, StoreError};

#[test]
fn each_store_error_has_one_request_refusal_classification() {
    // Catches the provider's failure policy and HTTP statuses drifting for a store error variant.
    let cases = [
        (
            StoreError::UnknownWorkspace(WorkspaceId::new("w".into())),
            Some(Refusal::Unknown),
        ),
        (
            StoreError::UnknownThread(ThreadId::new("t".into())),
            Some(Refusal::Unknown),
        ),
        (
            StoreError::UnknownMessage {
                thread_id: ThreadId::new("t".into()),
                message_id: MessageId::new("m".into()),
            },
            Some(Refusal::Unknown),
        ),
        (
            StoreError::UnknownApproval(ApprovalId::new("a".into())),
            Some(Refusal::Unknown),
        ),
        (
            StoreError::UnknownQuestion(QuestionId::new("q".into())),
            Some(Refusal::Unknown),
        ),
        (
            StoreError::UnknownContinuation(ContinuationId::new("c".into())),
            Some(Refusal::Unknown),
        ),
        (
            StoreError::MessageTransition {
                message_id: MessageId::new("m".into()),
                from: MessageStatus::Unknown {
                    error: "uncertain".into(),
                },
                to: MessageStatus::Queued,
            },
            Some(Refusal::Conflict),
        ),
        (
            StoreError::MessageIdTaken(MessageId::new("m".into())),
            Some(Refusal::Conflict),
        ),
        (
            StoreError::UnreadableEvent {
                thread_id: ThreadId::new("t".into()),
                seq: Seq::BEFORE_FIRST,
                source: serde_json::from_str::<serde_json::Value>("").unwrap_err(),
            },
            None,
        ),
        (StoreError::Sqlite(rusqlite::Error::InvalidQuery), None),
    ];
    for (error, expected) in cases {
        assert_eq!(error.refusal(), expected, "{error}");
        assert_eq!(error.is_refusal(), expected.is_some(), "{error}");
    }
}
