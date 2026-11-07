use kratos_protocol::Timestamp;
use std::time::{Duration, SystemTime};

#[test]
fn deserializing_a_timestamp_rejects_invalid_times() {
    // Catches JSON bypassing the parser used for stored timestamp columns.
    for text in [
        "",
        "last tuesday",
        "2030-02-30T00:00:00Z",
        "2030-01-01 00:00:00",
    ] {
        let result = serde_json::from_value::<Timestamp>(serde_json::json!(text));
        assert!(result.is_err(), "accepted invalid timestamp {text:?}");
    }
}

#[test]
fn deserializing_a_timestamp_preserves_valid_text() {
    // Catches parsing that silently rewrites stored times or changes the string wire format.
    for (text, seconds, nanos) in [
        ("2030-01-01T00:00:00Z", 1_893_456_000, 0),
        ("2030-01-01T05:00:00.123456789Z", 1_893_474_000, 123_456_789),
    ] {
        let timestamp: Timestamp = serde_json::from_value(serde_json::json!(text)).unwrap();
        assert_eq!(timestamp.as_str(), text);
        assert_eq!(
            timestamp.instant(),
            SystemTime::UNIX_EPOCH + Duration::new(seconds, nanos)
        );
        assert_eq!(serde_json::to_value(timestamp).unwrap(), text);
    }
}

#[test]
fn a_formatted_timestamp_caches_its_serialized_millisecond_instant() {
    // Catches the cached instant keeping precision that its serialized text has discarded.
    let input = SystemTime::UNIX_EPOCH + Duration::new(1, 123_456_789);
    let timestamp = Timestamp::at(input).unwrap();
    assert_eq!(
        timestamp.instant(),
        SystemTime::UNIX_EPOCH + Duration::from_millis(1123)
    );
    let restored: Timestamp =
        serde_json::from_value(serde_json::to_value(&timestamp).unwrap()).unwrap();
    assert_eq!(restored, timestamp);
}
