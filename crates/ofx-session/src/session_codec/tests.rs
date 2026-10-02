use super::*;

fn metadata(id: &str) -> SessionMetadata {
    SessionMetadata {
        id: id.to_owned(),
        origin_workspace_root: "/tmp/origin".to_owned(),
        workspace_root: "/tmp/current".to_owned(),
        created_at_ms: 1,
        updated_at_ms: 2,
        conversation_language: "en".to_owned(),
        preferences: SessionPreferences {
            provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
            model: "openai/gpt-5".to_owned(),
            effort: ReasoningEffort::Auto,
            fast_mode: false,
        },
        title: None,
    }
}

fn document(fields: &str) -> String {
    format!(
        "{{\"schema_version\":4,\"id\":\"good\",\"origin_workspace_root\":\"/tmp/a\",\"workspace_root\":\"/tmp/a\",\"created_at_ms\":1,\"updated_at_ms\":1,\"conversation_language\":\"en\",\"provider\":\"gateway\",\"model\":\"m\",\"effort\":\"auto\",\"fast_mode\":false{fields}}}"
    )
}

#[test]
fn metadata_is_written_in_upstream_field_order() {
    let encoded = encode_session_metadata(&metadata("session")).unwrap();
    assert_eq!(
        String::from_utf8(encoded.clone()).unwrap(),
        "{\"schema_version\":4,\"id\":\"session\",\"origin_workspace_root\":\"/tmp/origin\",\"workspace_root\":\"/tmp/current\",\"created_at_ms\":1,\"updated_at_ms\":2,\"conversation_language\":\"en\",\"provider\":\"gateway\",\"model\":\"openai/gpt-5\",\"effort\":\"auto\",\"fast_mode\":false,\"title\":null,\"subagent_child\":false}"
    );
    assert_eq!(
        decode_session_metadata(&encoded).unwrap(),
        metadata("session")
    );
}

#[test]
fn configured_providers_carry_their_binding_and_round_trip() {
    let mut value = metadata("session");
    value.preferences.provider = SavedProvider::new(
        ProviderId::Configured("portkey".to_owned()),
        Some([0xab; 32]),
    )
    .unwrap();
    value.preferences.effort = ReasoningEffort::Named("high".to_owned());
    value.preferences.fast_mode = true;
    value.title = Some("Fix the build".to_owned());
    let encoded = String::from_utf8(encode_session_metadata(&value).unwrap()).unwrap();
    assert!(encoded.contains(&format!(
        "\"provider\":{{\"name\":\"portkey\",\"binding\":\"{}\"}}",
        "ab".repeat(32)
    )));
    assert!(encoded.contains("\"effort\":\"high\",\"fast_mode\":true,\"title\":\"Fix the build\""));
    assert_eq!(decode_session_metadata(encoded.as_bytes()).unwrap(), value);
}

#[test]
fn saved_providers_bind_exactly_the_configured_ones() {
    assert!(SavedProvider::new(ProviderId::Codex, None).is_some());
    assert!(SavedProvider::new(ProviderId::Codex, Some([0; 32])).is_none());
    assert!(SavedProvider::new(ProviderId::Configured("x".to_owned()), None).is_none());
    let parse = |value: &str| parse_saved_provider(&serde_json::from_str(value).unwrap());
    assert_eq!(parse("\"GROK\"").unwrap().id(), &ProviderId::Grok);
    assert!(parse("\"portkey\"").is_none());
    let binding = "0F".repeat(32);
    assert_eq!(
        parse(&format!(
            "{{\"name\":\"portkey\",\"binding\":\"{binding}\"}}"
        ))
        .unwrap()
        .binding(),
        Some([0x0f; 32])
    );
    for invalid in [
        format!("{{\"name\":\"gateway\",\"binding\":\"{binding}\"}}"),
        format!(
            "{{\"name\":\"portkey\",\"binding\":\"{}\"}}",
            "0f".repeat(31)
        ),
        format!(
            "{{\"name\":\"portkey\",\"binding\":\"+f{}\"}}",
            "0f".repeat(31)
        ),
        format!("{{\"name\":\"portkey\",\"binding\":\"{binding}\",\"x\":1}}"),
        "{\"name\":\"portkey\"}".to_owned(),
        "7".to_owned(),
    ] {
        assert!(parse(&invalid).is_none(), "{invalid}");
    }
}

#[test]
fn metadata_rejects_invalid_fields_before_returning() {
    let invalid = [
        document("").replace("\"good\"", "\"../bad\""),
        document("").replace(
            "\"origin_workspace_root\":\"/tmp/a\"",
            "\"origin_workspace_root\":\"relative\"",
        ),
        document("").replace("\"en\"", "\" en \""),
        document("").replace("\"en\"", "\"en\\u0007\""),
        document("").replace("\"auto\"", "\"not valid\""),
        document("").replace("\"m\"", "\" m\""),
        document("").replace("\"m\"", "\"m\\u0000\""),
        document("").replace("\"updated_at_ms\":1", "\"updated_at_ms\":0"),
        document("").replace("\"created_at_ms\":1", "\"created_at_ms\":-1"),
        document("").replace("\"gateway\"", "\"portkey\""),
        document(",\"title\":\"\""),
        document(&format!(
            ",\"title\":\"{}\"",
            "t".repeat(MAX_SESSION_TITLE_BYTES + 1)
        )),
        document(",\"subagent_child\":true"),
        document(",\"unknown\":1"),
        document(",\"model\":\"twice\""),
    ];
    for bytes in invalid {
        assert_eq!(
            decode_session_metadata(bytes.as_bytes()),
            Err(SessionError::InvalidSessionMetadata),
            "{bytes}"
        );
    }
    for (bytes, error) in [
        (
            document("").replace("\"schema_version\":4", "\"schema_version\":3"),
            SessionError::UnsupportedSessionSchema,
        ),
        (
            document("").replace("\"schema_version\":4", "\"schema_version\":99"),
            SessionError::UnsupportedSessionSchema,
        ),
        (
            document(",\"x\":1").replace("\"schema_version\":4", "\"schema_version\":\"4\""),
            SessionError::UnsupportedSessionSchema,
        ),
        (
            document("").replace("\"schema_version\":4,", ""),
            SessionError::InvalidSessionFormat,
        ),
        ("[]".to_owned(), SessionError::InvalidSessionFormat),
        ("{".to_owned(), SessionError::InvalidSessionFormat),
    ] {
        assert_eq!(
            decode_session_metadata(bytes.as_bytes()),
            Err(error),
            "{bytes}"
        );
    }
    assert!(
        decode_session_metadata(document(",\"title\":\"ok\",\"subagent_child\":false").as_bytes())
            .is_ok()
    );
    assert!(decode_session_metadata(document("").as_bytes()).is_ok());
}

#[test]
fn metadata_size_is_bounded_on_both_sides() {
    assert_eq!(
        decode_session_metadata(b""),
        Err(SessionError::SessionMetadataTooLarge)
    );
    let oversized = vec![b' '; MAX_SESSION_METADATA_BYTES + 1];
    assert_eq!(
        decode_session_metadata(&oversized),
        Err(SessionError::SessionMetadataTooLarge)
    );
    let mut long = metadata("session");
    long.preferences.model = "m".repeat(MAX_MODEL_BYTES);
    long.workspace_root = format!(
        "/{}",
        "w".repeat(crate::session_store_paths::MAX_PATH_BYTES - 1)
    );
    long.origin_workspace_root.clone_from(&long.workspace_root);
    assert!(encode_session_metadata(&long).is_ok());
    long.preferences.model.push('m');
    assert_eq!(
        encode_session_metadata(&long),
        Err(SessionError::InvalidDurableField)
    );
}

#[test]
fn durable_session_ids_accept_safe_opaque_basenames() {
    for id in [
        "session.v3",
        ".hidden-session",
        "session..branch",
        &"a".repeat(255),
    ] {
        let encoded = encode_session_metadata(&metadata(id)).unwrap();
        assert_eq!(decode_session_metadata(&encoded).unwrap().id, id);
    }
    for id in [
        "",
        ".",
        "..",
        "path/session",
        "path\\session",
        "session:name",
        "session name",
        &"a".repeat(256),
    ] {
        assert_eq!(
            encode_session_metadata(&metadata(id)),
            Err(SessionError::InvalidDurableField),
            "{id:?}"
        );
    }
}
