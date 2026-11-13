//! Observable client behavior against a child process, including recorded protocol ordering.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use kratos_claude_client::{
    Claude, ConnectError, Control, ControlRequest, ExitCause, Incoming, IncomingControl,
    Initialize, Initialized, Launch, MAX_LINE_BYTES, PermissionMode, RequestError, SessionId,
    SessionStart, SettingSource, StreamMessage, TurnOutcome, connect,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

const A: &str = "10000000-0000-4000-8000-000000000001";

fn launch() -> Launch {
    #[cfg(windows)]
    kratos_process::contain_current_process().unwrap();
    let id: SessionId = "00000000-0000-4000-8000-000000000001".parse().unwrap();
    Launch::new(
        fake_agents::executable("fake-claude"),
        std::env::temp_dir(),
        SessionStart::New(id),
    )
}

async fn fake() -> (Claude, mpsc::Receiver<Incoming>, Initialized) {
    connect(launch(), Initialize::default(), drop)
        .await
        .unwrap()
}

async fn next(incoming: &mut mpsc::Receiver<Incoming>) -> Incoming {
    tokio::time::timeout(Duration::from_secs(10), incoming.recv())
        .await
        .expect("fake produces an event")
        .expect("stream remains open")
}

async fn command(claude: &Claude, subtype: &str, fields: Value) -> Value {
    let mut request = fields;
    request["subtype"] = json!(subtype);
    claude.control(Control::Raw(request)).await.unwrap()
}

async fn emit(claude: &Claude, message: Value) {
    command(claude, "test/emit", json!({"message":message})).await;
}

#[derive(Clone, Deserialize)]
struct Row {
    dir: String,
    msg: Value,
}

fn rows(scenario: &str) -> Vec<Row> {
    std::fs::read_to_string(recording_path(scenario))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn recording_path(scenario: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts")
        .join(scenario)
        .join("wire.jsonl")
}

fn answer_recorded(
    control: IncomingControl,
    answers: &HashMap<String, Value>,
    observed: &Mutex<Vec<ControlRequest>>,
) {
    observed.lock().unwrap().push(control.request.clone());
    let response = answers
        .get(&control.id)
        .expect("recorded reverse control has an answer");
    if response["subtype"] == "error" {
        assert!(control.fail(response["error"].as_str().unwrap()));
    } else {
        assert!(control.answer(response["response"].clone()));
    }
}

async fn play(
    scenario: &str,
) -> (
    Initialized,
    Vec<StreamMessage>,
    Vec<ControlRequest>,
    Vec<String>,
) {
    let rows = rows(scenario);
    let answers: Arc<HashMap<_, _>> = Arc::new(
        rows.iter()
            .filter(|row| row.dir == "out" && row.msg["type"] == "control_response")
            .map(|row| {
                (
                    row.msg["response"]["request_id"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    row.msg["response"].clone(),
                )
            })
            .collect(),
    );
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut launch = launch();
    launch.env_set.push((
        "FAKE_CLAUDE_TRANSCRIPT".into(),
        recording_path(scenario).into_os_string(),
    ));
    let initialize: Initialize = serde_json::from_value(rows[0].msg["request"].clone()).unwrap();
    let startup_answers = answers.clone();
    let startup_observed = observed.clone();
    let (claude, mut incoming, initialized) = connect(launch, initialize, move |control| {
        answer_recorded(control, &startup_answers, &startup_observed)
    })
    .await
    .unwrap();
    let initialized_at = rows
        .iter()
        .position(|row| {
            row.dir == "in" && row.msg["response"]["request_id"] == rows[0].msg["request_id"]
        })
        .unwrap();
    let mut sent_ids = Vec::new();
    let mut pending = HashMap::new();
    let mut messages = Vec::new();
    for row in rows.into_iter().skip(initialized_at + 1) {
        if row.dir == "out" {
            feed_recorded(&claude, row.msg, &mut pending, &mut sent_ids).await;
            continue;
        }
        match row.msg["type"].as_str() {
            Some("control_response") => {
                if let Some(reply) =
                    pending.remove(row.msg["response"]["request_id"].as_str().unwrap())
                {
                    reply.await.unwrap();
                }
            }
            Some("control_cancel_request") => {}
            Some("control_request") => {
                let Incoming::Control(control) = next(&mut incoming).await else {
                    panic!("recorded reverse control");
                };
                answer_recorded(control, &answers, &observed);
            }
            _ => {
                let Incoming::Message(message) = next(&mut incoming).await else {
                    panic!("recorded stream message");
                };
                messages.push(message);
            }
        }
    }
    assert!(pending.is_empty(), "every recorded control settled");
    claude.close().await;
    let controls = observed.lock().unwrap().clone();
    (initialized, messages, controls, sent_ids)
}

type PendingReply = Pin<Box<dyn Future<Output = Result<Value, RequestError>> + Send>>;

async fn feed_recorded(
    claude: &Claude,
    input: Value,
    pending: &mut HashMap<String, PendingReply>,
    sent_ids: &mut Vec<String>,
) {
    match input["type"].as_str() {
        Some("user") => {
            let id = format!("10000000-0000-4000-8000-{:012x}", sent_ids.len() + 1);
            claude
                .send(&id, input["message"]["content"].clone())
                .await
                .unwrap();
            sent_ids.push(id);
        }
        Some("control_request") => {
            let handle = claude.clone();
            let request = input["request"].clone();
            let mut reply: PendingReply =
                Box::pin(async move { handle.control(Control::Raw(request)).await });
            let immediate = std::future::poll_fn(|context| {
                Poll::Ready(match reply.as_mut().poll(context) {
                    Poll::Ready(reply) => Some(reply),
                    Poll::Pending => None,
                })
            })
            .await;
            if let Some(reply) = immediate {
                reply.unwrap();
            } else {
                pending.insert(input["request_id"].as_str().unwrap().to_owned(), reply);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn initialize_is_first_and_recorded_init_and_replay_keep_the_submitted_uuid() {
    // Catches sending user input before initialize or confusing the streamed init with acceptance.
    let (initialized, messages, _, sent) = play("s1-launch-and-stream").await;
    assert!(!initialized.models.is_empty());
    assert!(messages.iter().any(
        |message| matches!(message, StreamMessage::Init(init) if init.model == "claude-haiku-5-5")
    ));
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, StreamMessage::Replay { uuid } if uuid == &sent[0]))
    );
    let (claude, _, _) = fake().await;
    let inspected = command(&claude, "test/inspect", json!({})).await;
    assert_eq!(inspected["received"][0]["request"]["subtype"], "initialize");
    claude.close().await;
}

#[tokio::test]
async fn recording_fake_substitutes_both_live_folded_message_uuids() {
    // Catches substituting only the first UUID in replay, assistant correlation, or folded results.
    let (_, messages, _, sent) = play("s2-message-acknowledgement/b-fold").await;
    assert_eq!(sent.len(), 2);
    let result = messages
        .into_iter()
        .find_map(|message| match message {
            StreamMessage::Result(result) => Some(result),
            _ => None,
        })
        .unwrap();
    assert_eq!(result.message_ids, sent);
}

#[tokio::test]
async fn permission_and_sdk_mcp_controls_round_trip_from_real_recordings() {
    // Catches treating SDK MCP initialization as post-handshake or losing permission suggestions.
    let (_, _, controls, _) = play("s6-in-process-tool-server").await;
    assert!(controls.iter().any(|control| matches!(control, ControlRequest::McpMessage { message, .. } if message["method"] == "initialize")));
    assert!(controls.iter().any(|control| matches!(control, ControlRequest::McpMessage { message, .. } if message["method"] == "tools/call")));
    assert!(controls.iter().any(|control| matches!(control, ControlRequest::CanUseTool(tool) if tool.mcp_server.as_ref().is_some_and(|server| server.name == "kratos" && server.source == "sdk"))));
    let (_, _, controls, _) = play("s3-permission-checks").await;
    assert!(controls.iter().any(|control| matches!(control, ControlRequest::CanUseTool(tool) if tool.tool_name == "Write" && !tool.permission_suggestions.is_empty())));
    assert!(
        controls
            .iter()
            .any(|control| matches!(control, ControlRequest::HookCallback(_)))
    );
}

#[tokio::test]
async fn recorded_interrupt_acknowledgement_does_not_finish_the_turn() {
    // Catches treating the interrupt control acknowledgement as the terminal turn result.
    let (_, messages, _, sent) = play("s4-interrupt/a-running-tool").await;
    let results: Vec<_> = messages
        .into_iter()
        .filter_map(|message| match message {
            StreamMessage::Result(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results[0].outcome, TurnOutcome::Interrupted);
    assert_eq!(results[0].message_ids, [sent[0].clone()]);
    assert_eq!(results[1].outcome, TurnOutcome::Completed);
    assert_eq!(results[1].message_ids, [sent[1].clone()]);
}

#[tokio::test]
async fn echoed_control_responses_are_dropped_without_an_active_turn() {
    // Catches forwarding or answering the driver's own echoed reverse-control responses.
    let scenario = "s11-control-response-echo/with-replay-flag";
    let rows = rows(scenario);
    let answers: Arc<HashMap<_, _>> = Arc::new(
        rows.iter()
            .filter(|row| row.dir == "out" && row.msg["type"] == "control_response")
            .map(|row| {
                (
                    row.msg["response"]["request_id"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    row.msg["response"].clone(),
                )
            })
            .collect(),
    );
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut launch = launch();
    launch.env_set.push((
        "FAKE_CLAUDE_TRANSCRIPT".into(),
        recording_path(scenario).into_os_string(),
    ));
    let initialize = serde_json::from_value(rows[0].msg["request"].clone()).unwrap();
    let startup_answers = answers.clone();
    let startup_observed = observed.clone();
    let (claude, mut incoming, _) = connect(launch, initialize, move |control| {
        answer_recorded(control, &startup_answers, &startup_observed)
    })
    .await
    .unwrap();
    while observed.lock().unwrap().len() < 3 {
        let Incoming::Control(control) = next(&mut incoming).await else {
            panic!("only reverse controls are forwarded");
        };
        answer_recorded(control, &answers, &observed);
    }
    assert_eq!(command(&claude, "test/echo", json!({"value":42})).await, 42);
    assert!(matches!(
        incoming.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    claude.close().await;
}

#[tokio::test]
async fn unanswered_and_unsupported_controls_return_errors() {
    // Catches hanging Claude when a consumer drops a request or a self-update adds a new subtype.
    let (claude, mut incoming, _) = fake().await;
    emit(&claude, json!({"type":"control_request", "request_id":"drop-me", "request":{"subtype":"mcp_message", "server_name":"kratos", "message":{"method":"tools/list"}}})).await;
    let Incoming::Control(control) = next(&mut incoming).await else {
        panic!("incoming control");
    };
    drop(control);
    let Incoming::Message(StreamMessage::Unrecognized { raw, .. }) = next(&mut incoming).await
    else {
        panic!("fake reports the answer");
    };
    assert_eq!(raw["answer"]["response"]["subtype"], "error");
    assert!(
        raw["answer"]["response"]["error"]
            .as_str()
            .unwrap()
            .contains("unanswered")
    );
    emit(&claude, json!({"type":"control_request", "request_id":"future", "request":{"subtype":"future_control"}})).await;
    let Incoming::Message(StreamMessage::Unrecognized { raw, .. }) = next(&mut incoming).await
    else {
        panic!("unsupported control is answered internally");
    };
    assert!(
        raw["answer"]["response"]["error"]
            .as_str()
            .unwrap()
            .contains("future_control")
    );
    claude.close().await;
}

#[tokio::test]
async fn withdrawn_control_cannot_be_answered_or_answered_on_drop() {
    // Catches sending a stale permission answer after Claude has cancelled its request.
    let (claude, mut incoming, _) = fake().await;
    emit(&claude, json!({"type":"control_request", "request_id":"withdrawn", "request":{"subtype":"elicitation", "mcp_server_name":"synthetic", "message":"Choose"}})).await;
    let Incoming::Control(control) = next(&mut incoming).await else {
        panic!("incoming elicitation");
    };
    emit(
        &claude,
        json!({"type":"control_cancel_request", "request_id":"withdrawn"}),
    )
    .await;
    assert!(matches!(next(&mut incoming).await, Incoming::Cancelled { id } if id == "withdrawn"));
    assert!(!control.answer(json!({"action":"accept"})));
    let inspected = command(&claude, "test/inspect", json!({})).await;
    assert!(
        inspected["received"]
            .as_array()
            .unwrap()
            .iter()
            .all(|message| message["type"] != "control_response")
    );
    claude.close().await;
}

#[tokio::test]
async fn malformed_and_dropped_hooks_explicitly_stop_instead_of_returning_no_decision() {
    // Catches hook errors interpreted by Claude as no decision, which could let execution continue.
    let (claude, mut incoming, _) = fake().await;
    emit(&claude, json!({ "type": "control_request", "request_id": "malformed-hook", "request": { "subtype": "hook_callback" } })).await;
    assert!(matches!(next(&mut incoming).await, Incoming::Unreadable(_)));
    let Incoming::Message(StreamMessage::Unrecognized { raw, .. }) = next(&mut incoming).await
    else {
        panic!("fake reports the malformed hook answer");
    };
    assert_eq!(raw["answer"]["response"]["subtype"], "success");
    assert_eq!(raw["answer"]["response"]["response"]["continue"], false);
    emit(&claude, json!({ "type": "control_request", "request_id": "dropped-hook", "request": { "subtype": "hook_callback", "callback_id": "post", "input": {} } })).await;
    let Incoming::Control(control) = next(&mut incoming).await else {
        panic!("incoming hook");
    };
    drop(control);
    let Incoming::Message(StreamMessage::Unrecognized { raw, .. }) = next(&mut incoming).await
    else {
        panic!("fake reports the dropped hook answer");
    };
    assert_eq!(raw["answer"]["response"]["subtype"], "success");
    assert_eq!(raw["answer"]["response"]["response"]["continue"], false);
    claude.close().await;
}

#[tokio::test]
async fn expired_control_times_out_and_later_controls_still_work() {
    // Catches a missing control deadline or leaving the connection unusable after timeout.
    let (claude, mut incoming, _) = fake().await;
    let request = claude
        .control_within(
            Control::Raw(json!({"subtype":"test/hang"})),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
    assert!(matches!(request, RequestError::Timeout { .. }));
    let _pending = next(&mut incoming).await;
    assert_eq!(
        command(&claude, "test/echo", json!({"value":"alive"})).await,
        "alive"
    );
    claude.close().await;
}

#[tokio::test]
async fn exit_fails_every_pending_request_once_with_the_complete_stderr_tail() {
    // Catches orphaned pending futures or taking the stderr snapshot before its final diagnostic.
    let (claude, mut incoming, _) = fake().await;
    let mut requests = Vec::new();
    for _ in 0..3 {
        let handle = claude.clone();
        requests.push(tokio::spawn(async move {
            handle
                .control(Control::Raw(json!({"subtype":"test/hang"})))
                .await
        }));
        let _pending = next(&mut incoming).await;
    }
    let exit = claude
        .control(Control::Raw(
            json!({"subtype":"test/exit", "code":23,"stderr_lines":25}),
        ))
        .await
        .unwrap_err();
    assert!(matches!(exit, RequestError::Exited(_)));
    for request in requests {
        assert!(matches!(
            request.await.unwrap(),
            Err(RequestError::Exited(_))
        ));
    }
    let Incoming::Exited(exit) = next(&mut incoming).await else {
        panic!("terminal exit");
    };
    assert_eq!(exit.code, Some(23));
    assert_eq!(exit.stderr.len(), 20);
    assert_eq!(
        exit.stderr.last().map(String::as_str),
        Some("final fake diagnostic")
    );
    assert!(incoming.recv().await.is_none());
}

#[tokio::test]
async fn a_cli_that_closes_stdout_is_given_time_to_finish_exiting() {
    // Catches killing a CLI at stdout EOF before it flushes its transcript and exits with its own code.
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("finished.txt");
    let (claude, mut incoming, _) = fake().await;
    let request = json!({"subtype":"test/close_stdout_then_exit", "pause_ms":300, "code":3,
        "marker":marker});
    let failure = claude.control(Control::Raw(request)).await.unwrap_err();
    assert!(matches!(failure, RequestError::Exited(ref exit) if exit.code == Some(3)));
    let Incoming::Exited(exit) = next(&mut incoming).await else {
        panic!("terminal exit");
    };
    assert_eq!(exit.code, Some(3), "the CLI's own exit code survives");
    assert!(marker.exists(), "the CLI finished its work before exiting");
}

#[tokio::test]
async fn unanswered_controls_filling_the_reply_queue_end_the_session_with_a_reason() {
    // Catches ending the session on a full reply queue with an exit that looks like a crash.
    let (claude, mut incoming, _) = fake().await;
    let flood = {
        let claude = claude.clone();
        tokio::spawn(async move {
            claude
                .control(Control::Raw(
                    json!({"subtype":"test/flood_controls", "count":400}),
                ))
                .await
        })
    };
    let mut held = Vec::new();
    let exit = loop {
        match next(&mut incoming).await {
            Incoming::Control(control) => held.push(control),
            Incoming::Exited(exit) => break exit,
            other => panic!("unexpected event: {other:?}"),
        }
    };
    assert_eq!(exit.cause, Some(ExitCause::ReplyQueueFull));
    assert!(exit.to_string().contains("waiting for an answer"), "{exit}");
    let _ = flood.await.unwrap();
    assert!(!held.is_empty());
}

#[tokio::test]
async fn typed_controls_serialize_with_their_subtype() {
    // Catches a typed control reaching the CLI without the subtype tag that selects its handler.
    let (claude, _incoming, _) = fake().await;
    claude.interrupt().await.unwrap();
    let inspected = command(&claude, "test/inspect", json!({})).await;
    let requests: Vec<&Value> = inspected["received"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| &message["request"])
        .collect();
    assert_eq!(requests[0]["subtype"], "initialize");
    assert_eq!(requests[1], &json!({"subtype":"interrupt"}));
    claude.close().await;
}

#[tokio::test]
async fn process_exit_settles_requests_even_when_the_event_queue_is_full() {
    // Catches coupling pending-request failure to a consumer draining the incoming queue.
    let (claude, mut incoming, _) = fake().await;
    let request = claude.control(Control::Raw(json!({"subtype":"test/flood_then_exit"})));
    let failure = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .expect("process exit bypasses event backpressure")
        .unwrap_err();
    assert!(matches!(failure, RequestError::Exited(ref exit) if exit.code == Some(19)));
    claude.close().await;
    let mut noises = 0;
    loop {
        match next(&mut incoming).await {
            Incoming::Message(StreamMessage::Unrecognized { kind, .. }) if kind == "noise" => {
                noises += 1
            }
            Incoming::Exited(_) => break,
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert_eq!(noises, 512);
}

#[tokio::test]
async fn oversized_and_malformed_lines_are_reported_and_the_next_frame_survives() {
    // Catches an oversized line exhausting memory or terminating parsing before the next valid frame.
    let (claude, mut incoming, _) = fake().await;
    command(
        &claude,
        "test/oversized",
        json!({"bytes":MAX_LINE_BYTES + 73}),
    )
    .await;
    assert!(
        matches!(next(&mut incoming).await, Incoming::Oversized { bytes } if bytes == MAX_LINE_BYTES + 73)
    );
    assert!(
        matches!(next(&mut incoming).await, Incoming::Message(StreamMessage::Replay { uuid }) if uuid == "after-oversize")
    );
    emit(&claude, json!({"type":"result", "subtype":"success"})).await;
    assert!(matches!(next(&mut incoming).await, Incoming::Unreadable(_)));
    claude.send(A, json!("after malformed")).await.unwrap();
    assert!(
        matches!(next(&mut incoming).await, Incoming::Message(StreamMessage::Replay { uuid }) if uuid == A)
    );
    claude.close().await;
}

#[tokio::test]
async fn dropping_the_last_handle_ends_the_process() {
    // Catches internal tasks or retained connection state accidentally owning shutdown forever.
    let (claude, mut incoming, _) = fake().await;
    let inspected = command(&claude, "test/inspect", json!({})).await;
    let pid = u32::try_from(inspected["pid"].as_u64().unwrap()).unwrap();
    drop(claude);
    assert!(matches!(next(&mut incoming).await, Incoming::Exited(_)));
    assert!(!fake_agents::process_is_running(pid));
}

#[tokio::test]
async fn close_deadline_ends_a_child_that_ignores_closed_stdin() {
    // Catches waiting forever for a CLI that remains alive after its input closes.
    let mut launch = launch();
    launch.close_timeout = Duration::from_millis(40);
    launch
        .env_set
        .push(("FAKE_CLAUDE_HOLD_AFTER_EOF".into(), "1".into()));
    let (claude, mut incoming, _) = connect(launch, Initialize::default(), drop).await.unwrap();
    let inspected = command(&claude, "test/inspect", json!({})).await;
    let pid = u32::try_from(inspected["pid"].as_u64().unwrap()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), claude.close())
        .await
        .expect("close timeout ends the process tree");
    assert!(!fake_agents::process_is_running(pid));
    assert!(matches!(next(&mut incoming).await, Incoming::Exited(_)));
}

#[tokio::test]
async fn failed_and_cancelled_handshakes_end_the_process_tree() {
    // Catches leaking the native process when initialization times out or connect is cancelled.
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let mut launch = launch();
    launch.env_set.extend([
        ("FAKE_CLAUDE_INITIALIZE".into(), "hang".into()),
        (
            "FAKE_CLAUDE_PID_FILE".into(),
            pid_file.clone().into_os_string(),
        ),
    ]);
    launch.control_timeout = Duration::from_secs(1);
    let failure = connect(launch.clone(), Initialize::default(), drop)
        .await
        .unwrap_err();
    assert!(matches!(
        failure,
        ConnectError::Initialize(RequestError::Timeout { .. })
    ));
    let first_pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    assert!(!fake_agents::process_is_running(first_pid));
    std::fs::remove_file(&pid_file).unwrap();
    launch.control_timeout = Duration::from_secs(30);
    let task = tokio::spawn(connect(launch, Initialize::default(), drop));
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&pid_file).await
                && let Ok(pid) = text.parse::<u32>()
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), async {
        while fake_agents::process_is_running(pid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled handshake process ends");
}

#[test]
fn launch_filters_inherited_provider_and_engine_environment_in_a_helper_process() {
    // Catches testing env_set instead of inherited filtering or changing the test runner's environment.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "environment_probe_child", "--nocapture"])
        .env("FAKE_CLAUDE_ENVIRONMENT_PROBE", "1")
        .env("CLAUDE_PARENT_SECRET", "synthetic-claude")
        .env("anthropic_parent_secret", "synthetic-anthropic")
        .env("KRATOS_API_TOKEN", "synthetic-token")
        .env("DISABLE_AUTOUPDATER", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "helper probe failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn environment_probe_child() {
    // Catches leaked inherited prefixes, launch flags that disable subscription authentication,
    // and a session left free to update the CLI by itself.
    if std::env::var_os("FAKE_CLAUDE_ENVIRONMENT_PROBE").is_none() {
        return;
    }
    let mut launch = launch();
    launch.permission_mode = PermissionMode::BypassPermissions;
    launch.setting_sources = vec![SettingSource::User, SettingSource::Project];
    launch.disallowed_tools = vec!["Bash".to_owned(), "PowerShell".to_owned()];
    launch.model = Some("haiku".to_owned());
    launch.env_set.extend([
        ("CLAUDE_INTENTIONAL".into(), "allowed".into()),
        ("SYNTHETIC_OVERRIDE".into(), "arrived".into()),
    ]);
    let (claude, _, _) = connect(launch, Initialize::default(), drop).await.unwrap();
    let inspected = command(&claude, "test/inspect", json!({})).await;
    let arguments = inspected["arguments"].as_array().unwrap();
    assert!(
        arguments
            .iter()
            .all(|argument| argument != "-p" && argument != "--bare")
    );
    assert!(arguments.contains(&json!("--allow-dangerously-skip-permissions")));
    assert!(arguments.contains(&json!("--setting-sources=user,project")));
    assert!(arguments.contains(&json!("Bash,PowerShell")));
    assert!(arguments.contains(&json!("--model=haiku")));
    let environment = inspected["environment"].as_object().unwrap();
    let leaked: Vec<_> = environment
        .keys()
        .filter(|name| {
            let name = name.to_ascii_uppercase();
            (name.starts_with("CLAUDE")
                || name.starts_with("ANTHROPIC")
                || name.starts_with("KRATOS"))
                && name != "CLAUDE_INTENTIONAL"
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "inherited prefix names leaked: {leaked:?}"
    );
    assert_eq!(environment["CLAUDE_INTENTIONAL"], "allowed");
    assert_eq!(environment["SYNTHETIC_OVERRIDE"], "arrived");
    assert_eq!(
        environment["DISABLE_AUTOUPDATER"], "1",
        "a session may update the CLI under the others"
    );
    claude.close().await;
}
