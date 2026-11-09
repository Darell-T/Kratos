//! Compatibility facts pinned to the recorded native CLI rather than inferred from SDK types.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use kratos_claude_client::{
    Initialized, SessionId, SessionStart, StreamMessage, TurnOutcome, TurnResult,
    delivered_message_ids, locate_claude, transcript_path,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Row {
    dir: String,
    msg: Value,
}

fn recordings() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/transcripts")
}
fn rows(scenario: &str) -> Vec<Row> {
    std::fs::read_to_string(recordings().join(scenario).join("wire.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn results(scenario: &str) -> Vec<TurnResult> {
    rows(scenario)
        .into_iter()
        .filter(|row| row.dir == "in" && row.msg["type"] == "result")
        .map(|row| TurnResult::parse(row.msg).unwrap())
        .collect()
}

#[test]
fn initialize_accounts_preserve_pro_and_signed_out_shapes_without_identity() {
    // Catches requiring tokenSource for a subscription login or exposing account identity in raw data.
    let pro = rows("s0-protocol-smoke/a-logged-in")
        .into_iter()
        .find(|row| row.dir == "in" && row.msg["type"] == "control_response")
        .unwrap();
    let initialized = Initialized::parse(pro.msg["response"]["response"].clone()).unwrap();
    let account = initialized.account.unwrap();
    assert_eq!(account.subscription_type.as_deref(), Some("Claude Pro"));
    assert_eq!(account.token_source, None);
    assert_eq!(initialized.raw["account"].get("email"), None);
    assert_eq!(initialized.raw["account"].get("organization"), None);
    let signed_out = rows("s0-protocol-smoke/b-empty-config-dir")
        .into_iter()
        .find(|row| row.dir == "in" && row.msg["type"] == "control_response")
        .unwrap();
    let account = Initialized::parse(signed_out.msg["response"]["response"].clone())
        .unwrap()
        .account
        .unwrap();
    assert_eq!(account.token_source.as_deref(), Some("none"));
    assert_eq!(account.api_provider.as_deref(), Some("firstParty"));
}

#[test]
fn only_dequeued_replay_is_acceptance_and_folded_result_keeps_both_messages() {
    // Catches acknowledging queued messages or losing the second message in a folded native turn.
    let rows = rows("s2-message-acknowledgement/b-fold");
    let sent: Vec<String> = rows
        .iter()
        .filter(|row| row.dir == "out" && row.msg["type"] == "user")
        .map(|row| row.msg["uuid"].as_str().unwrap().to_owned())
        .collect();
    let mut replay_positions = Vec::new();
    let mut queued_second = None;
    let mut assistants = Vec::new();
    for (position, row) in rows
        .into_iter()
        .enumerate()
        .filter(|(_, row)| row.dir == "in")
    {
        if row.msg["type"] == "command_lifecycle"
            && row.msg["state"] == "queued"
            && row.msg["command_uuid"] == sent[1]
        {
            queued_second = Some(position);
        }
        let parsed = StreamMessage::parse(row.msg).unwrap();
        match parsed {
            StreamMessage::Replay { uuid } => replay_positions.push((position, uuid)),
            StreamMessage::Assistant(assistant) => assistants.push(assistant),
            StreamMessage::Result(result) => {
                assert_eq!(result.message_ids, sent);
                assert_eq!(result.outcome, TurnOutcome::Completed);
            }
            _ => {}
        }
    }
    assert_eq!(replay_positions.len(), 2);
    assert!(queued_second.unwrap() < replay_positions[1].0);
    assert_eq!(replay_positions[1].1, sent[1]);
    assert!(assistants[0].user_message_uuids.is_some());
    assert!(
        assistants
            .iter()
            .skip(1)
            .all(|assistant| assistant.user_message_uuids.is_none())
    );
}

#[test]
fn sequential_turns_repeat_init_and_lifecycle_frames_remain_available() {
    // Catches treating session init as a one-shot frame or discarding undeclared command lifecycle data.
    let parsed: Vec<_> = rows("s2-message-acknowledgement/a-sequential")
        .into_iter()
        .filter(|row| row.dir == "in" && !row.msg["type"].as_str().unwrap().starts_with("control_"))
        .map(|row| StreamMessage::parse(row.msg).unwrap())
        .collect();
    assert_eq!(
        parsed
            .iter()
            .filter(|message| matches!(message, StreamMessage::Init(_)))
            .count(),
        2
    );
    assert!(parsed.iter().any(|message| matches!(message, StreamMessage::Unrecognized { kind, raw } if kind == "command_lifecycle" && raw["state"] == "queued")));
}

#[test]
fn tools_and_streaming_interrupts_end_as_interrupted_before_a_queued_message_runs() {
    // Catches mapping Claude's error-shaped interrupt to failure or attaching the next message to it.
    let tools = results("s4-interrupt/a-running-tool");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].outcome, TurnOutcome::Interrupted);
    assert_eq!(tools[0].raw["terminal_reason"], "aborted_tools");
    assert_eq!(tools[1].outcome, TurnOutcome::Completed);
    assert_ne!(tools[0].message_ids, tools[1].message_ids);
    let streaming = results("s4c-interrupt-while-streaming");
    assert_eq!(streaming[0].outcome, TurnOutcome::Interrupted);
    assert_eq!(streaming[0].raw["terminal_reason"], "aborted_streaming");
}

#[test]
fn real_rate_limit_event_keeps_two_windows_and_unix_second_resets() {
    // Catches reading utilization from the wrong nesting or interpreting reset seconds as milliseconds.
    let row = rows("s1-launch-and-stream")
        .into_iter()
        .find(|row| row.msg["type"] == "rate_limit_event")
        .unwrap();
    let StreamMessage::RateLimit(event) = StreamMessage::parse(row.msg).unwrap() else {
        panic!("rate limit event");
    };
    let info = event.rate_limit_info;
    assert_eq!(info.status, "allowed");
    assert_eq!(info.rate_limit_type.as_deref(), Some("five_hour"));
    assert_eq!(info.resets_at, Some(1_792_531_800));
    let windows = info.unified_windows.unwrap();
    let five = windows.five_hour.unwrap();
    let seven = windows.seven_day.unwrap();
    assert!((five.utilization.unwrap() - 0.53).abs() < 1e-10);
    assert!((seven.utilization.unwrap() - 0.17).abs() < 1e-10);
    assert_eq!(five.resets_at, Some(1_792_531_800));
    assert_eq!(seven.resets_at, Some(1_792_638_000));
}

#[test]
fn structured_output_and_failure_text_follow_the_typescript_result_contract() {
    // Catches dropping reports, overriding an empty plural list, or losing a failed turn's error text.
    let report = results("s7-forced-report-format/c-initialize-only");
    assert_eq!(
        report.last().unwrap().structured_output,
        Some(json!({"answer":"4"}))
    );
    let failure = TurnResult::parse(json!({"subtype":"error_during_execution", "is_error":true,"errors":["first", "second"], "user_message_uuids":[], "user_message_uuid":"singular"})).unwrap();
    assert!(failure.message_ids.is_empty());
    assert_eq!(
        failure.outcome,
        TurnOutcome::Failed {
            error: "first\nsecond".to_owned()
        }
    );
    let failure =
        TurnResult::parse(json!({"subtype":"success", "is_error":true, "result":"failed success"}))
            .unwrap();
    assert_eq!(
        failure.outcome,
        TurnOutcome::Failed {
            error: "failed success".to_owned()
        }
    );
    let failure = TurnResult::parse(json!({"subtype":"error", "is_error":true})).unwrap();
    assert_eq!(
        failure.outcome,
        TurnOutcome::Failed {
            error: "Claude turn failed".to_owned()
        }
    );
    let empty =
        TurnResult::parse(json!({"subtype":"success", "is_error":false,"user_message_uuid":""}))
            .unwrap();
    assert!(empty.message_ids.is_empty());
}

#[test]
fn plan_write_and_bypass_question_still_request_permission_in_the_recordings() {
    // Catches transport assumptions that suppress plan writes or AskUserQuestion callbacks.
    let plan = rows("s9-plan-mode-and-ask-user-question/a-plan");
    let requests: Vec<_> = plan
        .iter()
        .filter(|row| row.dir == "in" && row.msg["request"]["subtype"] == "can_use_tool")
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].msg["request"]["tool_name"], "Write");
    let writes: Vec<_> = plan
        .iter()
        .filter(|row| row.dir == "in" && row.msg["type"] == "assistant")
        .flat_map(|row| row.msg["message"]["content"].as_array().unwrap())
        .filter(|block| block["type"] == "tool_use" && block["name"] == "Write")
        .collect();
    assert_eq!(writes.len(), 2);
    assert!(
        writes[0]["input"]["file_path"]
            .as_str()
            .unwrap()
            .contains("/.claude/plans/")
    );
    assert_eq!(
        requests[0].msg["request"]["input"]["file_path"],
        writes[1]["input"]["file_path"]
    );
    let question = rows("s9-plan-mode-and-ask-user-question/b-ask-user-question");
    assert!(
        question
            .iter()
            .any(|row| row.dir == "in" && row.msg["request"]["tool_name"] == "AskUserQuestion")
    );
    assert!(question.iter().any(|row| {
        row.msg
            .to_string()
            .contains("The user did not answer the questions")
    }));
}

#[test]
fn native_session_id_and_subscription_login_survive_a_recorded_resume() {
    // Catches changing native identity on resume or mistaking subscription login for an API key.
    let first: Vec<_> = rows("s5-resume/run1")
        .into_iter()
        .filter_map(|row| match StreamMessage::parse(row.msg) {
            Ok(StreamMessage::Init(init)) => Some(init),
            _ => None,
        })
        .collect();
    let resumed: Vec<_> = rows("s5-resume/run2")
        .into_iter()
        .filter_map(|row| match StreamMessage::parse(row.msg) {
            Ok(StreamMessage::Init(init)) => Some(init),
            _ => None,
        })
        .collect();
    assert_eq!(first[0].session_id, resumed[0].session_id);
    for (scenario, flag) in [
        ("s5-resume/run1", "--session-id="),
        ("s5-resume/run2", "--resume="),
    ] {
        let launch: Value = serde_json::from_str(
            &std::fs::read_to_string(recordings().join(scenario).join("launch.json")).unwrap(),
        )
        .unwrap();
        assert!(
            launch["argv"]
                .as_array()
                .unwrap()
                .contains(&json!(format!("{flag}{}", first[0].session_id)))
        );
    }
    let init = rows("s1-launch-and-stream")
        .into_iter()
        .find_map(|row| match StreamMessage::parse(row.msg) {
            Ok(StreamMessage::Init(init)) => Some(init),
            _ => None,
        })
        .unwrap();
    assert_eq!(init.metadata["apiKeySource"], "none");
}

#[test]
fn a_cancelled_queued_message_is_not_in_the_completed_turn() {
    // Catches attaching a cancelled asynchronous message to a completed native turn.
    let rows = rows("s2-message-acknowledgement/c-cancel");
    let cancelled = rows
        .iter()
        .find(|row| {
            row.dir == "in"
                && row.msg["type"] == "command_lifecycle"
                && row.msg["state"] == "cancelled"
        })
        .unwrap();
    let id = cancelled.msg["command_uuid"].as_str().unwrap();
    let parsed = StreamMessage::parse(cancelled.msg.clone()).unwrap();
    assert!(
        matches!(parsed, StreamMessage::Unrecognized { kind, .. } if kind == "command_lifecycle")
    );
    let result = results("s2-message-acknowledgement/c-cancel");
    assert!(
        result
            .iter()
            .all(|result| !result.message_ids.iter().any(|consumed| consumed == id))
    );
}

#[tokio::test]
async fn native_transcript_reconciliation_includes_user_rows_and_folded_attachments() {
    // Catches losing a folded message after restart because it was stored as an attachment.
    let delivered = delivered_message_ids(&recordings().join("native-fold.jsonl"))
        .await
        .unwrap();
    assert!(!delivered.torn_tail);
    let ids = delivered.ids;
    assert_eq!(ids.len(), 3);
    assert!(ids.contains("00000000-0000-4000-8000-00000000005f"));
    assert!(ids.contains("00000000-0000-4000-8000-000000000075"));
    assert!(ids.contains("00000000-0000-4000-8000-000000000071"));
}

#[tokio::test]
async fn a_torn_last_line_keeps_the_earlier_ids_and_says_delivery_may_be_unknown() {
    // Catches failing the whole read after a crash mid-write, when the earlier rows still prove delivery.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"uuid\":\"first\"}
{\"type\":\"user\",\"uuid\":\"second\",\"mess",
    )
    .unwrap();
    let delivered = delivered_message_ids(&path).await.unwrap();
    assert!(delivered.torn_tail);
    assert_eq!(delivered.ids, HashSet::from(["first".to_owned()]));

    std::fs::write(
        &path,
        "{\"type\":\"user\",\"uuid\":\"first\"}
",
    )
    .unwrap();
    assert!(!delivered_message_ids(&path).await.unwrap().torn_tail);

    std::fs::write(&path, "{\"type\":\"user\",\"uuid\":\"first\"}").unwrap();
    let unterminated = delivered_message_ids(&path).await.unwrap();
    assert!(!unterminated.torn_tail, "a complete row needs no newline");
    assert_eq!(unterminated.ids.len(), 1);
}

#[tokio::test]
async fn a_malformed_line_before_the_tail_still_fails() {
    // Catches treating corruption in the middle of a transcript as a harmless torn tail.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"uuid\":\"first\"}
{\"type\":\"user\",\"uu
{\"type\":\"user\",\"uuid\":\"third\"}
",
    )
    .unwrap();
    assert!(delivered_message_ids(&path).await.is_err());
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"uu
",
    )
    .unwrap();
    assert!(
        delivered_message_ids(&path).await.is_err(),
        "a newline-terminated bad line is not a torn tail"
    );
}

#[test]
fn session_flags_follow_transcript_existence_and_pin_directory_encoding() {
    // Catches choosing the mutually exclusive native flags incorrectly or changing the measured layout.
    let home = tempfile::tempdir().unwrap();
    let id: SessionId = "00000000-0000-4000-8000-000000000001".parse().unwrap();
    let cwd = Path::new("C:\\project one\\a.b");
    let path = transcript_path(home.path(), cwd, id);
    assert_eq!(
        path,
        home.path()
            .join(".claude/projects/C--project-one-a-b/00000000-0000-4000-8000-000000000001.jsonl")
    );
    assert_eq!(
        SessionStart::for_session(home.path(), cwd, id).unwrap(),
        SessionStart::New(id)
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "").unwrap();
    assert_eq!(
        SessionStart::for_session(home.path(), cwd, id).unwrap(),
        SessionStart::Resume(id)
    );
}

#[test]
fn locator_reports_override_and_actual_path_search() {
    // Catches ignoring an explicit invalid override or returning a later CLI after an earlier launcher.
    let directory = tempfile::tempdir().unwrap();
    let invalid = directory.path().join("missing.exe");
    let error = locate_claude(Some(invalid.as_os_str()), None)
        .unwrap_err()
        .to_string();
    assert!(error.contains(&invalid.display().to_string()));
    let path = std::env::join_paths([directory.path()]).unwrap();
    assert!(
        locate_claude(None, Some(&path))
            .unwrap_err()
            .to_string()
            .contains(&directory.path().display().to_string())
    );
    #[cfg(windows)]
    {
        std::fs::write(directory.path().join("claude.cmd"), "@exit /b 0").unwrap();
        assert!(
            locate_claude(None, Some(&path))
                .unwrap_err()
                .to_string()
                .contains("claude.cmd")
        );
    }
}

#[test]
fn future_message_types_survive_but_known_malformed_messages_are_errors() {
    // Catches dropping self-updated protocol frames or disguising malformed results as unknown messages.
    let raw = json!({"type":"future_notification", "new_field":42});
    assert_eq!(
        StreamMessage::parse(raw.clone()).unwrap(),
        StreamMessage::Unrecognized {
            kind: "future_notification".to_owned(),
            raw
        }
    );
    assert!(StreamMessage::parse(json!({"type":"result", "subtype":"success"})).is_err());
    assert!(
        StreamMessage::parse(
            json!({"type":"system", "subtype":"init", "session_id":"synthetic", "model":""})
        )
        .is_err()
    );
}

#[tokio::test]
async fn malformed_delivery_markers_fail_instead_of_reporting_non_delivery() {
    // Catches resending accepted work because a malformed native marker was silently skipped.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    for row in [
        json!({"type":"user"}),
        json!({"type":"user", "uuid":42}),
        json!({"type":"user", "uuid":" "}),
        json!({"type":"attachment", "attachment":{"type":"queued_command"}}),
    ] {
        std::fs::write(&path, format!("{row}\n")).unwrap();
        assert!(
            delivered_message_ids(&path).await.is_err(),
            "invalid delivery marker: {row}"
        );
    }
}
