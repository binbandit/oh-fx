use super::*;

fn preferences(raw: &str) -> Option<SessionPreferences> {
    decode_preferences(raw.as_bytes()).ok()
}

#[test]
fn preferences_decode_as_v1_decodes_them_and_drop_a_childs_instructions() {
    let decoded = preferences(
        r#"{"provider":"gateway","model":"spacexai/grok-4.7","effort":"auto","fast_mode":false}"#,
    )
    .unwrap();
    assert_eq!(decoded.model, "spacexai/grok-4.7");
    assert!(!decoded.fast_mode && !decoded.ultrafast_mode);
    let decoded = preferences(
        r#"{"provider":"gateway","model":"m","effort":"high","fast_mode":true,"ultrafast_mode":true,"instructions":"Be brief."}"#,
    )
    .unwrap();
    assert!(decoded.fast_mode && decoded.ultrafast_mode);
    assert_eq!(decoded.effort, ReasoningEffort::Named("high".to_owned()));
    assert!(preferences(r#"{"model":"m","effort":"auto","fast_mode":false}"#).is_some());
    assert!(
        preferences(
            r#"{"connection_id":"vercel","model_id":"m","effort":"auto","fast_mode":false}"#
        )
        .is_some()
    );
    for bad in [
        r#"{"connection_id":"other","model_id":"m","effort":"auto","fast_mode":false}"#,
        r#"{"provider":"gateway","model":"m","effort":"auto","fast_mode":false,"color":1}"#,
        r#"{"provider":"gateway","model":"m","effort":"auto"}"#,
        r#"{"provider":"gateway","model":"m","effort":"","fast_mode":false}"#,
        r#"{"provider":"gateway","model":"","effort":"auto","fast_mode":false}"#,
        r#"{"provider":"gateway","model":"m","effort":"auto","fast_mode":false,"ultrafast_mode":1}"#,
        "[]",
    ] {
        assert!(preferences(bad).is_none(), "{bad}");
    }
}

#[test]
fn a_usage_checkpoint_needs_an_integer_time_and_a_valid_snapshot() {
    assert!(matches!(
        decode_usage(br#"{"at_ms":1.5,"snapshot":{}}"#),
        Err(RecordError::InvalidUsageCheckpoint)
    ));
    assert!(matches!(
        decode_usage(br#"{"snapshot":{}}"#),
        Err(RecordError::InvalidUsageCheckpoint)
    ));
    assert!(matches!(
        decode_usage(br#"{"at_ms":1,"snapshot":{"schema_version":3}}"#),
        Err(RecordError::InvalidUsageCheckpoint)
    ));
}

#[test]
fn titles_are_json_strings_and_languages_valid_tags() {
    assert_eq!(
        decode_title(Some(br#""T\u00e9""#))
            .ok()
            .flatten()
            .as_deref(),
        Some("T\u{e9}")
    );
    assert!(decode_title(Some(b"5")).is_err());
    assert_eq!(decode_language(None).ok().as_deref(), Some("und"));
    assert!(decode_language(Some(br#""""#)).is_err());
    assert!(decode_language(Some(br#"" en""#)).is_err());
}
