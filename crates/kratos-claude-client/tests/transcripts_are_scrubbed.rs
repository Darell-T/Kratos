//! Privacy checks over every committed recording, including native transcript samples and launches.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

const LOCAL_MARKERS: [&str; 2] = ["chatgpt", "kratos-claude-cli-spike"];

fn check_string(text: &str) -> Result<(), String> {
    if text.contains('@') {
        return Err("email or account marker in transcript".to_owned());
    }
    let lowered = text.to_ascii_lowercase();
    if let Some(marker) = LOCAL_MARKERS
        .iter()
        .find(|marker| lowered.contains(**marker))
    {
        return Err(format!("local marker {marker:?} in transcript"));
    }
    let mut normalized = text.to_ascii_lowercase().replace('\\', "/");
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    if normalized.contains(":/users/") {
        return Err("Windows user path in transcript".to_owned());
    }
    for prefix in ["--session-id=", "--resume="] {
        if let Some(id) = text.strip_prefix(prefix) {
            check_session_id(id)?;
        }
    }
    Ok(())
}

fn check_session_id(id: &str) -> Result<(), String> {
    if id.len() == 36
        && id.starts_with("00000000-0000-4000-8000-")
        && id[24..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(format!("non-synthetic session id: {id}"))
    }
}

fn check_value(value: &Value) -> Result<(), String> {
    match value {
        Value::String(text) => check_string(text),
        Value::Array(values) => {
            for value in values {
                check_value(value)?;
            }
            Ok(())
        }
        Value::Object(values) => {
            for (key, value) in values {
                check_string(key)?;
                check_session_field(key, value)?;
                check_value(value)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn check_session_field(key: &str, value: &Value) -> Result<(), String> {
    if matches!(key, "session_id" | "sessionId") {
        check_session_id(value.as_str().ok_or("session id is not a string")?)?;
    }
    Ok(())
}

fn content_block_texts(rows: &[Value]) -> Vec<String> {
    let mut finished = Vec::new();
    let mut open: BTreeMap<(String, u64), String> = BTreeMap::new();
    for row in rows {
        let message = &row["msg"];
        if message["type"] != "stream_event" {
            continue;
        }
        let event = &message["event"];
        let Some(index) = event["index"].as_u64() else {
            continue;
        };
        let key = (
            message["api_message_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            index,
        );
        match event["type"].as_str() {
            Some("content_block_start") => {
                finished.extend(open.insert(key, String::new()));
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                let piece = ["text", "partial_json", "thinking"]
                    .iter()
                    .find_map(|field| delta[*field].as_str());
                open.entry(key).or_default().push_str(piece.unwrap_or(""));
            }
            _ => {}
        }
    }
    finished.extend(open.into_values());
    finished
}

fn check_file(path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    if path
        .extension()
        .is_some_and(|extension| extension == "jsonl")
    {
        let mut rows = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let value: Value = serde_json::from_str(line)
                .map_err(|error| format!("line {}: {error}", index + 1))?;
            check_value(&value)?;
            rows.push(value);
        }
        content_block_texts(&rows)
            .iter()
            .try_for_each(|block| check_string(block))
    } else {
        check_value(&serde_json::from_str::<Value>(&text).map_err(|error| error.to_string())?)
    }
}

fn check_directory(path: &Path) {
    for entry in std::fs::read_dir(path).expect("recording directory is readable") {
        let path = entry.expect("recording entry is readable").path();
        if path.is_dir() {
            check_directory(&path);
        } else {
            check_file(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        }
    }
}

#[test]
fn recorded_transcripts_contain_no_account_identity_or_local_user_paths() {
    // Catches publishing an unsanitized recording or native session sample.
    check_directory(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/transcripts"));
}

#[test]
fn checker_rejects_a_planted_email_path_and_real_session_id() {
    // Catches a privacy checker that passes because its fixture list is already clean.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("planted.jsonl");
    for planted in [
        serde_json::json!({"account":{"email":"planted@example.invalid"}}),
        serde_json::json!({"cwd":"c:/uSeRs/synthetic/project"}),
        serde_json::json!({"cwd":"C:\\Users\\synthetic\\project"}),
        serde_json::json!({"sessionId":"11111111-1111-4111-8111-111111111111"}),
        serde_json::json!({"session_id":42}),
        serde_json::json!({"argv":["--resume=11111111-1111-4111-8111-111111111111"]}),
    ] {
        std::fs::write(&path, format!("{planted}\n")).unwrap();
        assert!(
            check_file(&path).is_err(),
            "planted identity must be rejected: {planted}"
        );
    }
}

#[test]
fn checker_rejects_a_path_split_across_two_streamed_deltas() {
    // Catches a checker that inspects one delta at a time, so a path cut between two deltas passes.
    let delta = |text: &str| {
        serde_json::json!({"dir":"in","msg":{"type":"stream_event","api_message_id":"msg_1",
            "event":{"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":text}}}})
    };
    let start = serde_json::json!({"dir":"in","msg":{"type":"stream_event","api_message_id":"msg_1",
        "event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}});
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("split.jsonl");
    for (first, second) in [
        (r"see C:\Us", r"ers\someone\file.txt"),
        ("see Chat", "GPT/file.txt"),
        ("see kratos-claude-", "cli-spike/file.txt"),
    ] {
        let rows = [start.clone(), delta(first), delta(second)];
        let text: String = rows
            .iter()
            .map(|row| {
                format!(
                    "{row}
"
                )
            })
            .collect();
        std::fs::write(&path, text).unwrap();
        assert!(
            check_file(&path).is_err(),
            "split path must be rejected: {first}{second}"
        );
    }
}
