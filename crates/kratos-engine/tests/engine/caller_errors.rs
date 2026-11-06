use kratos_api_client::ApiRequest;
use serde_json::json;

use crate::running_engine::RunningEngine;

#[test]
fn malformed_path_and_query_extractors_return_json_errors() {
    // Catches Axum's plain-text extractor rejection escaping the API's JSON error envelope.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    for (method, path) in [
        ("GET", "/api/threads/%ff/events"),
        ("GET", "/api/threads/%ff"),
        ("POST", "/api/threads/%ff/messages"),
        ("POST", "/api/threads/%ff/stop"),
        ("POST", "/api/threads/%ff/resume"),
        ("PUT", "/api/threads/%ff/settings"),
        ("GET", "/api/threads/%ff/skills"),
        ("DELETE", "/api/threads/thread/messages/%ff"),
        ("POST", "/api/threads/thread/messages/%ff/steer"),
        ("POST", "/api/threads/thread/messages/%ff/retry"),
        ("POST", "/api/threads/thread/messages/%ff/discard"),
        ("POST", "/api/approvals/%ff"),
        ("POST", "/api/questions/%ff"),
        ("POST", "/api/continuations/%ff/schedule"),
    ] {
        let reply = client.send(&ApiRequest::new(method, path)).unwrap();
        assert_eq!(reply.status, 400, "{method} {path}");
        assert_eq!(reply.body, json!({"error": "Malformed URL path"}), "{path}");
    }
    for path in ["/api/threads", "/api/threads/thread/events?after=abc"] {
        let reply = client.get(path).unwrap();
        assert_eq!(reply.status, 400, "{path}");
        assert!(reply.body["error"].is_string(), "{path}: {}", reply.body);
    }
}
