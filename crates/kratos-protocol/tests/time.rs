use kratos_protocol::Timestamp;

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
    for text in ["2030-01-01T00:00:00Z", "2030-01-01T05:00:00.123456789Z"] {
        let timestamp: Timestamp = serde_json::from_value(serde_json::json!(text)).unwrap();
        assert_eq!(timestamp.as_str(), text);
        assert!(timestamp.instant().is_some());
        assert_eq!(serde_json::to_value(timestamp).unwrap(), text);
    }
}
