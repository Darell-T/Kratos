//! The approval and question contracts ported from the TypeScript Claude provider.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use kratos_protocol::{
    Approval, ApprovalDecision, ApprovalOutcome, Question, QuestionOutcome, Seq, ThreadEventBody,
};
use serde_json::{Value, json};

use crate::harness::{A, B, Harness, Launch, Options, WAIT};

pub async fn send(harness: &Harness, launch: &Launch, id: &str, tool: &str, input: Value) {
    harness
        .call(
            launch,
            "test/ask",
            json!({ "requestId": id, "request": {
                "subtype": "can_use_tool", "tool_name": tool, "input": input, "tool_use_id": id
            }}),
        )
        .await;
}

pub async fn response(harness: &Harness, launch: &Launch, id: &str) -> Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(frame) = harness.received(launch).await.into_iter().find(|frame| {
            frame["type"] == "control_response" && frame["response"]["request_id"] == id
        }) {
            return frame["response"]["response"].clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{id} was never answered"
        );
        tokio::task::yield_now().await;
    }
}

pub async fn approval(harness: &mut Harness) -> Approval {
    let ThreadEventBody::ApprovalRequested { approval } = harness
        .wait_for(|event| matches!(event, ThreadEventBody::ApprovalRequested { .. }))
        .await
    else {
        unreachable!()
    };
    approval
}

async fn question(harness: &mut Harness) -> Question {
    let ThreadEventBody::QuestionAsked { question } = harness
        .wait_for(|event| matches!(event, ThreadEventBody::QuestionAsked { .. }))
        .await
    else {
        unreachable!()
    };
    question
}

#[tokio::test]
async fn routes_tool_approvals_to_kratos_and_answers_each_request_exactly_once() {
    // Catches repeated execution decisions, session rules that bypass Kratos, and requests left open on withdrawal or result.
    let mut harness = Harness::start(Options {
        tools: true,
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    let mut client_one = harness
        .hub
        .follow(harness.thread_id.clone(), Seq::BEFORE_FIRST)
        .await
        .unwrap();
    let mut client_two = harness
        .hub
        .follow(harness.thread_id.clone(), Seq::BEFORE_FIRST)
        .await
        .unwrap();
    send(
        &harness,
        &launch,
        "write",
        "Write",
        json!({ "file_path": "notes.rs", "content": "hello" }),
    )
    .await;
    let first = approval(&mut harness).await;
    let decided = harness
        .provider
        .decide(first.id.clone(), ApprovalDecision::AcceptForSession)
        .await
        .unwrap();
    assert_eq!(
        decided,
        ApprovalOutcome::Decided {
            decision: ApprovalDecision::AcceptForSession
        }
    );
    assert_eq!(
        harness
            .provider
            .decide(first.id.clone(), ApprovalDecision::Decline)
            .await
            .unwrap(),
        decided
    );
    let answer = response(&harness, &launch, "write").await;
    assert_eq!(answer["behavior"], "allow");
    assert!(
        answer.get("updatedPermissions").is_none(),
        "session rules would bypass Kratos"
    );
    for client in [&mut client_one, &mut client_two] {
        let mut seen = Vec::new();
        while !seen.iter().any(|event: &kratos_protocol::ThreadEvent| {
            matches!(event.event, ThreadEventBody::ApprovalDecided { .. })
        }) {
            seen.extend(
                tokio::time::timeout(WAIT, client.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(seen.iter().any(|event| matches!(&event.event, ThreadEventBody::ApprovalRequested { approval } if approval.id == first.id)));
    }
    send(
        &harness,
        &launch,
        "write-again",
        "Write",
        json!({ "file_path": "notes.rs" }),
    )
    .await;
    let again = approval(&mut harness).await;
    harness
        .provider
        .decide(again.id, ApprovalDecision::Decline)
        .await
        .unwrap();
    assert_eq!(
        response(&harness, &launch, "write-again").await["behavior"],
        "deny"
    );
    send(
        &harness,
        &launch,
        "withdrawn",
        "Read",
        json!({ "file_path": "notes.rs" }),
    )
    .await;
    let withdrawn = approval(&mut harness).await;
    harness
        .emit(
            &launch,
            json!({ "type": "control_cancel_request", "request_id": "withdrawn" }),
        )
        .await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::ApprovalClosed { .. }))
        .await;
    assert!(matches!(
        harness
            .provider
            .decide(withdrawn.id, ApprovalDecision::Accept)
            .await
            .unwrap(),
        ApprovalOutcome::Closed { .. }
    ));
    send(
        &harness,
        &launch,
        "ends",
        "Read",
        json!({ "file_path": "notes.rs" }),
    )
    .await;
    let ended = approval(&mut harness).await;
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(matches!(
        harness
            .provider
            .decide(ended.id, ApprovalDecision::Accept)
            .await
            .unwrap(),
        ApprovalOutcome::Closed { .. }
    ));
    assert_eq!(
        response(&harness, &launch, "ends").await["behavior"],
        "deny"
    );
    assert!(harness.details().await.approvals.is_empty());
    let replies = harness.received(&launch).await;
    assert_eq!(
        replies
            .iter()
            .filter(|frame| frame["response"]["request_id"] == "write")
            .count(),
        1
    );
    assert!(
        !replies
            .iter()
            .any(|frame| frame["response"]["request_id"] == "withdrawn")
    );
}

#[tokio::test]
async fn answers_questions_and_elicitations_and_never_stops_the_turn_while_any_are_pending() {
    // Catches Kratos interrupting a user who is answering, duplicate answers, or an answer lost before reaching Claude.
    let mut harness = Harness::start(Options {
        tools: true,
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    harness.send(B, B).await;
    let questions = json!([{ "question": "Which database?", "header": "Database", "options": [{ "label": "SQLite", "description": "File" }], "multiSelect": false }]);
    send(
        &harness,
        &launch,
        "question",
        "AskUserQuestion",
        json!({ "questions": questions }),
    )
    .await;
    let asked = question(&mut harness).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(harness.controls(&launch, "interrupt").await, 0);
    assert!(harness.details().await.approvals.is_empty());
    let answers = BTreeMap::from([("Which database?".into(), vec!["SQLite".into()])]);
    assert_eq!(
        harness
            .provider
            .answer(asked.id.clone(), answers, None)
            .await
            .unwrap(),
        QuestionOutcome::Answered
    );
    assert_eq!(
        response(&harness, &launch, "question").await,
        json!({ "behavior": "allow", "updatedInput": { "questions": questions, "answers": { "Which database?": "SQLite" } } })
    );
    assert_eq!(
        harness
            .provider
            .answer(asked.id, BTreeMap::new(), None)
            .await
            .unwrap(),
        QuestionOutcome::Answered
    );
    harness.call(&launch, "test/ask", json!({ "requestId": "sign-in", "request": { "subtype": "elicitation", "mcp_server_name": "github", "message": "Sign in", "mode": "url", "url": "https://example.invalid" } })).await;
    let elicited = question(&mut harness).await;
    assert_eq!(elicited.items[0].header, "github");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(harness.controls(&launch, "interrupt").await, 0);
    harness
        .provider
        .answer(elicited.id, BTreeMap::new(), None)
        .await
        .unwrap();
    assert_eq!(
        response(&harness, &launch, "sign-in").await,
        json!({ "action": "cancel" })
    );
    harness.call(&launch, "test/ask", json!({ "requestId": "form", "request": { "subtype": "elicitation", "mcp_server_name": "jira", "message": "Project key", "mode": "form", "requested_schema": {} } })).await;
    let form = question(&mut harness).await;
    harness
        .emit(
            &launch,
            json!({ "type": "control_cancel_request", "request_id": "form" }),
        )
        .await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::QuestionClosed { .. }))
        .await;
    assert!(matches!(
        harness
            .provider
            .answer(form.id, BTreeMap::new(), None)
            .await
            .unwrap(),
        QuestionOutcome::Closed { .. }
    ));
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(harness.controls(&launch, "interrupt").await, 0);
    assert_eq!(harness.details().await.paused, None);
    harness.finish(&launch, &[A], json!({})).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    harness.wait_written(&launch, &[A, B]).await;
}

#[tokio::test]
async fn a_cli_exit_closes_both_kinds_of_user_request_and_later_answers_return_that_outcome() {
    // Catches disconnected clients still showing a request whose CLI can never receive its answer.
    let mut harness = Harness::start(Options {
        tools: true,
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    send(
        &harness,
        &launch,
        "read",
        "Read",
        json!({ "file_path": "notes.txt" }),
    )
    .await;
    let asked = approval(&mut harness).await;
    send(&harness, &launch, "question-exit", "AskUserQuestion", json!({ "questions": [{ "question": "Which folder?", "header": "Folder", "options": [] }] })).await;
    let question = question(&mut harness).await;
    harness.exit(&launch, 3).await;
    harness
        .wait_for(|event| matches!(event, ThreadEventBody::TurnCompleted { .. }))
        .await;
    assert!(harness.details().await.approvals.is_empty());
    assert!(harness.details().await.questions.is_empty());
    assert!(matches!(
        harness
            .provider
            .decide(asked.id, ApprovalDecision::Accept)
            .await
            .unwrap(),
        ApprovalOutcome::Closed { .. }
    ));
    assert!(matches!(
        harness
            .provider
            .answer(question.id, BTreeMap::new(), None)
            .await
            .unwrap(),
        QuestionOutcome::Closed { .. }
    ));
}

#[tokio::test]
async fn accepting_an_approval_after_stop_cannot_restart_the_stopped_action() {
    // Catches a late user decision allowing a file tool or command after the user has stopped its turn.
    let mut harness = Harness::start(Options {
        tools: true,
        ..Options::default()
    })
    .await;
    let launch = harness.begin(A, 1).await;
    send(
        &harness,
        &launch,
        "before-stop",
        "Write",
        json!({ "file_path": "notes.txt" }),
    )
    .await;
    let asked = approval(&mut harness).await;
    harness
        .provider
        .stop(harness.thread_id.clone())
        .await
        .unwrap();
    assert!(matches!(
        harness
            .provider
            .decide(asked.id, ApprovalDecision::Accept)
            .await
            .unwrap(),
        ApprovalOutcome::Closed { .. }
    ));
    assert_eq!(
        response(&harness, &launch, "before-stop").await["behavior"],
        "deny"
    );
}
