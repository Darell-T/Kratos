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

#[test]
fn settings_no_provider_can_hold_are_refused_with_400_before_they_are_stored() {
    // Catches a zero or oversized context limit and a blank model reaching the database through a
    // thread of a provider that checks nothing: the blank model was stored, and both limits failed
    // the insert and answered 500.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let workspace = engine.create_workspace().id;
    let create = |settings: serde_json::Value| {
        client
            .post(
                "/api/threads",
                &json!({ "workspaceId": workspace, "title": "chat", "provider": "claude",
                         "settings": settings }),
            )
            .unwrap()
    };
    let settings = |model: serde_json::Value, limit: serde_json::Value| {
        json!({ "model": model, "permission": "askMe", "planMode": false,
                "contextLimitTokens": limit })
    };

    for (field, bad) in [
        ("contextLimitTokens", settings(json!(null), json!(0))),
        (
            "contextLimitTokens",
            settings(json!(null), json!(9_223_372_036_854_775_808_u64)),
        ),
        ("model", settings(json!("  "), json!(null))),
    ] {
        let refused = create(bad.clone());
        assert_eq!(refused.status, 400, "{bad}: {}", refused.body);
        let message = refused.body["error"].as_str().unwrap();
        assert!(message.contains(field), "{bad}: {message}");
    }
    let largest = create(settings(json!(" model "), json!(i64::MAX)));

    assert_eq!(largest.status, 201, "{}", largest.body);
    let threads = client
        .get(&format!("/api/threads?workspaceId={}", workspace.as_str()))
        .unwrap();
    assert_eq!(
        threads.body.as_array().unwrap().len(),
        1,
        "only the valid thread is stored"
    );
    let details = client
        .get(&format!(
            "/api/threads/{}",
            largest.body["id"].as_str().unwrap()
        ))
        .unwrap();
    assert_eq!(details.body["settings"]["model"], "model");
    assert_eq!(details.body["settings"]["contextLimitTokens"], i64::MAX);
}
