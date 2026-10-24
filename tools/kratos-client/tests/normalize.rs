use kratos_client::Normalizer;
use serde_json::{Value, json};

fn normalized(mut value: Value) -> Value {
    Normalizer::default().apply(&mut value);
    value
}

fn run(workspace: &str, thread: &str, at: &str) -> Value {
    json!({
        "workspace": { "id": workspace, "createdAt": at },
        "thread": { "id": thread, "workspaceId": workspace, "updatedAt": at },
        "events": [{ "threadId": thread, "at": at }],
    })
}

#[test]
fn two_runs_compare_equal_and_a_swapped_id_still_differs() {
    // Catches a normalizer that blanks every id, which would hide an engine that links a thread to
    // the wrong workspace, and one that numbers ids by run instead of by first appearance.
    let first = run(
        "11111111-1111-4111-8111-111111111111",
        "22222222-2222-4222-8222-222222222222",
        "2030-01-01T00:00:01.000Z",
    );
    let second = run(
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        "2031-06-07T08:09:10.111Z",
    );
    assert_eq!(normalized(first.clone()), normalized(second));

    let swapped = run(
        "11111111-1111-4111-8111-111111111111",
        "11111111-1111-4111-8111-111111111111",
        "2030-01-01T00:00:01.000Z",
    );
    assert_ne!(normalized(first), normalized(swapped));
}

#[test]
fn leaves_other_text_alone() {
    // Catches a normalizer that rewrites anything near the shape of an id or a time, such as a
    // title.
    let text = json!({
        "title": "11111111-1111-4111-8111-11111111111",
        "other": "2030-01-01T00:00:01Z",
        "path": "C:\\work\\repo",
        "count": 3,
    });
    assert_eq!(normalized(text.clone()), text);
}
