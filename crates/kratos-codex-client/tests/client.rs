//! The client against fake-codex: the handshake, requests and their deadlines, notifications,
//! server requests and their answers, exit, and close.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use kratos_codex_client::types::{ClientInfo, InitializeCapabilities};
use kratos_codex_client::{
    Codex, ConnectError, Incoming, Launch, Notification, RequestError, ServerRequest, connect,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn launch(args: &[&str]) -> Launch {
    Launch {
        program: fake_agents::executable("fake-codex"),
        args: args.iter().map(OsString::from).collect(),
        cwd: std::env::temp_dir(),
        env_remove: Vec::new(),
        env_set: Vec::new(),
        client_info: ClientInfo {
            name: "kratos-test".to_owned(),
            title: Some("Kratos test".to_owned()),
            version: "9.9.9".to_owned(),
        },
        request_timeout: REQUEST_TIMEOUT,
        close_timeout: Duration::from_secs(5),
    }
}

fn capabilities() -> InitializeCapabilities {
    serde_json::from_value(json!({ "experimentalApi": true })).unwrap()
}

async fn connect_fake() -> (Codex, mpsc::Receiver<Incoming>) {
    connect(launch(&[]), capabilities()).await.unwrap()
}

async fn control(codex: &Codex, method: &str, params: Value) -> Value {
    codex
        .request_value(method, params, REQUEST_TIMEOUT)
        .await
        .unwrap()
}

/// The next message that is not one of the fake's own `test/answered` reports.
async fn next(incoming: &mut mpsc::Receiver<Incoming>) -> Incoming {
    tokio::time::timeout(Duration::from_secs(10), incoming.recv())
        .await
        .expect("a message arrives")
        .expect("the stream is open")
}

/// What the fake reports the client answered to its last `test/ask`.
async fn answer_seen(incoming: &mut mpsc::Receiver<Incoming>) -> Value {
    loop {
        if let Incoming::Notification(Notification::Other { method, params }) = next(incoming).await
            && method == "test/answered"
        {
            return params;
        }
    }
}

fn question(thread_id: &str) -> Value {
    json!({
        "threadId": thread_id,
        "turnId": "turn-A",
        "itemId": "item-A",
        "questions": [],
        "isBlocking": true,
    })
}

#[tokio::test]
async fn completes_the_handshake_and_exposes_the_initialize_result() {
    let (codex, _incoming) = connect_fake().await;

    let info = codex.info();
    assert_eq!(info.user_agent, "kratos-test/9.9.9 (fake codex)");
    assert_eq!(*info.codex_home, "C:\\fake\\.codex");
    assert_eq!(info.platform_family, "windows");
    codex.close().await;
}

#[tokio::test]
async fn refuses_a_handshake_whose_result_has_the_wrong_shape() {
    let error = connect(launch(&["--bad-initialize"]), capabilities())
        .await
        .unwrap_err();

    assert!(
        matches!(error, ConnectError::Handshake(RequestError::Unreadable { ref method, .. }) if method == "initialize"),
        "{error:?}"
    );
}

#[tokio::test]
async fn bounds_a_stalled_handshake_and_ends_the_process() {
    let mut stalled = launch(&["--hang-initialize"]);
    stalled.request_timeout = Duration::from_millis(200);

    let error = connect(stalled, capabilities()).await.unwrap_err();

    assert!(
        matches!(
            error,
            ConnectError::Handshake(RequestError::Timeout { ref method, .. }) if method == "initialize"
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn passes_the_working_folder_and_codex_home_to_the_process() {
    let folder = tempfile::tempdir().unwrap();
    let mut with_home = launch(&[]);
    with_home.cwd = folder.path().to_path_buf();
    with_home.env_set = vec![("CODEX_HOME".into(), "C:\\somewhere\\.codex".into())];
    let (codex, _incoming) = connect(with_home, capabilities()).await.unwrap();

    let environment = control(&codex, "test/environment", json!({})).await;

    assert_eq!(environment["codexHome"], "C:\\somewhere\\.codex");
    let cwd = PathBuf::from(environment["cwd"].as_str().unwrap());
    assert_eq!(
        cwd.canonicalize().unwrap(),
        folder.path().canonicalize().unwrap()
    );
    codex.close().await;
}

#[tokio::test]
async fn names_codex_exe_and_the_install_command_when_the_program_is_missing() {
    let mut missing = launch(&[]);
    missing.program = PathBuf::from("kratos-no-such-codex");

    let error = connect(missing, capabilities()).await.unwrap_err();

    assert!(matches!(error, ConnectError::NotFound(_)), "{error:?}");
    let message = error.to_string();
    assert!(
        message.contains("npm install -g @openai/codex"),
        "{message}"
    );
    assert!(message.contains("CODEX_EXE"), "{message}");
}

#[tokio::test]
async fn returns_results_and_errors_and_skips_lines_that_are_not_json() {
    let (codex, _incoming) = connect_fake().await;

    let echoed = control(
        &codex,
        "test/echo",
        json!({ "hello": "world", "n": [1, 2] }),
    )
    .await;
    let failure = codex
        .request_value("test/fail", json!({}), REQUEST_TIMEOUT)
        .await
        .unwrap_err();
    let after_noise = control(&codex, "test/noise", json!({})).await;

    assert_eq!(echoed, json!({ "hello": "world", "n": [1, 2] }));
    assert!(
        matches!(failure, RequestError::Rpc { code: -32000, ref message } if message == "boom"),
        "{failure:?}"
    );
    assert_eq!(after_noise, json!("still alive"));
    codex.close().await;
}

#[tokio::test]
async fn delivers_a_notification_before_the_reply_that_follows_it() {
    let (codex, mut incoming) = connect_fake().await;

    control(&codex, "test/notify", json!({ "n": 1 })).await;

    let Incoming::Notification(Notification::Other { method, params }) = incoming
        .try_recv()
        .expect("the notification is already here")
    else {
        panic!("expected the test notification");
    };
    assert_eq!(method, "test/notified");
    assert_eq!(params, json!({ "n": 1 }));
    codex.close().await;
}

#[tokio::test]
async fn answers_a_server_request_under_its_own_id() {
    let (codex, mut incoming) = connect_fake().await;
    control(
        &codex,
        "test/ask",
        json!({ "method": "item/tool/requestUserInput", "requestId": 42, "params": question("thread-A") }),
    )
    .await;

    let Incoming::Request(request) = next(&mut incoming).await else {
        panic!("expected the question");
    };
    assert!(
        matches!(request.request, ServerRequest::UserInput(ref params) if params.thread_id == "thread-A")
    );
    assert!(request.answer(&json!({ "answers": { "choice": { "answers": ["Yes"] } } })));

    assert_eq!(
        answer_seen(&mut incoming).await,
        json!({ "result": { "answers": { "choice": { "answers": ["Yes"] } } } })
    );
    codex.close().await;
}

#[tokio::test]
async fn answers_unknown_methods_and_unreadable_requests_itself() {
    let (codex, mut incoming) = connect_fake().await;

    control(
        &codex,
        "test/ask",
        json!({ "method": "test/unhandled", "params": {} }),
    )
    .await;
    let unknown = answer_seen(&mut incoming).await;
    control(
        &codex,
        "test/ask",
        json!({ "method": "item/tool/requestUserInput", "params": { "threadId": 7 } }),
    )
    .await;
    let unreadable = answer_seen(&mut incoming).await;

    assert_eq!(
        unknown,
        json!({ "error": { "code": -32601, "message": "Method not handled by Kratos: test/unhandled" } })
    );
    assert_eq!(unreadable["error"]["code"], -32602);
    codex.close().await;
}

#[tokio::test]
async fn answers_a_dropped_request_with_an_internal_error() {
    let (codex, mut incoming) = connect_fake().await;
    control(
        &codex,
        "test/ask",
        json!({ "method": "item/tool/requestUserInput", "params": question("thread-A") }),
    )
    .await;

    let Incoming::Request(request) = next(&mut incoming).await else {
        panic!("expected the question");
    };
    drop(request);

    assert_eq!(
        answer_seen(&mut incoming).await,
        json!({ "error": { "code": -32603, "message": "Kratos dropped the request unanswered" } })
    );
    codex.close().await;
}

#[tokio::test]
async fn sends_no_answer_after_the_server_withdraws_its_request() {
    let (codex, mut incoming) = connect_fake().await;
    control(
        &codex,
        "test/ask",
        json!({ "method": "item/tool/requestUserInput", "requestId": "stale", "params": question("thread-A") }),
    )
    .await;
    let Incoming::Request(request) = next(&mut incoming).await else {
        panic!("expected the question");
    };
    control(
        &codex,
        "test/emit",
        json!({ "method": "serverRequest/resolved", "params": { "threadId": "thread-A", "requestId": "stale" } }),
    )
    .await;

    assert!(!request.answer(&json!({ "answers": {} })));

    control(&codex, "test/notify", json!({ "barrier": true })).await;
    while let Ok(message) = incoming.try_recv() {
        if let Incoming::Notification(Notification::Other { method, .. }) = &message {
            assert_ne!(method, "test/answered", "a withdrawn request was answered");
        }
    }
    codex.close().await;
}

#[tokio::test]
async fn fails_pending_and_later_requests_once_the_process_exits() {
    let (codex, mut incoming) = connect_fake().await;

    let hanging = {
        let codex = codex.clone();
        tokio::spawn(async move {
            codex
                .request_value("test/hang", json!({}), REQUEST_TIMEOUT)
                .await
        })
    };
    tokio::task::yield_now().await;
    let exiting = codex
        .request_value("test/exit", json!({ "code": 3 }), REQUEST_TIMEOUT)
        .await;

    for outcome in [hanging.await.unwrap(), exiting] {
        assert!(
            matches!(outcome, Err(RequestError::Exited(ref exit)) if exit.code == Some(3)),
            "{outcome:?}"
        );
    }
    loop {
        if let Incoming::Exited(exit) = next(&mut incoming).await {
            assert_eq!(exit.code, Some(3));
            break;
        }
    }
    assert!(incoming.recv().await.is_none(), "nothing follows the exit");
    let later = codex
        .request_value("test/echo", json!({}), REQUEST_TIMEOUT)
        .await;
    assert!(matches!(later, Err(RequestError::Exited(_))), "{later:?}");
}

#[tokio::test]
async fn a_stalled_request_times_out_without_breaking_later_ones() {
    let (codex, _incoming) = connect_fake().await;

    let stalled = codex
        .request_value("test/hang", json!({}), Duration::from_millis(50))
        .await;

    assert!(
        matches!(stalled, Err(RequestError::Timeout { ref method, .. }) if method == "test/hang"),
        "{stalled:?}"
    );
    assert_eq!(
        control(&codex, "test/echo", json!({ "still": "alive" })).await,
        json!({ "still": "alive" })
    );
    codex.close().await;
}

#[tokio::test]
async fn close_ends_the_process_tree_and_later_requests_fail() {
    let (codex, mut incoming) = connect_fake().await;
    let server = control(&codex, "test/pid", json!({}))
        .await
        .as_u64()
        .unwrap() as u32;
    let grandchild = control(&codex, "test/spawnChild", json!({}))
        .await
        .as_u64()
        .unwrap() as u32;
    assert!(fake_agents::process_is_running(grandchild));

    codex.close().await;
    codex.close().await;

    assert!(!fake_agents::process_is_running(server));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while fake_agents::process_is_running(grandchild) {
        assert!(
            std::time::Instant::now() < deadline,
            "the server's own child outlived close"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    loop {
        if let Incoming::Exited(_) = next(&mut incoming).await {
            break;
        }
    }
    let later = codex
        .request_value("test/echo", json!({}), REQUEST_TIMEOUT)
        .await;
    assert!(matches!(later, Err(RequestError::Exited(_))), "{later:?}");
}
