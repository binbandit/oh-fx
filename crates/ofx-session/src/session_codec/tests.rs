use serde_json::Value;

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
        subagent_child: false,
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
fn a_child_session_says_so_as_upstream_writes_it() {
    let mut child = metadata("child");
    child.subagent_child = true;
    let encoded = encode_session_metadata(&child).unwrap();
    assert!(
        String::from_utf8(encoded.clone())
            .unwrap()
            .ends_with(",\"title\":null,\"subagent_child\":true}")
    );
    assert_eq!(decode_session_metadata(&encoded).unwrap(), child);
    assert!(
        !decode_session_metadata(document("").as_bytes())
            .unwrap()
            .subagent_child
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
fn metadata_is_never_written_in_a_form_its_reader_rejects() {
    for effort in [String::new(), "not valid".to_owned(), "e".repeat(65)] {
        let mut value = metadata("session");
        value.preferences.effort = ReasoningEffort::Named(effort.clone());
        assert_eq!(
            encode_session_metadata(&value),
            Err(SessionError::InvalidSessionMetadata),
            "{effort:?}"
        );
    }
    for name in ["", "not valid", "Gateway", "codex"] {
        assert!(
            SavedProvider::new(ProviderId::Configured(name.to_owned()), Some([0; 32])).is_none(),
            "{name:?}"
        );
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
        document(",\"subagent_child\":1"),
        document(",\"subagent_child\":null"),
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SerdeRecord {
    schema_version: u8,
    id: String,
    origin_workspace_root: String,
    workspace_root: String,
    created_at_ms: i64,
    updated_at_ms: i64,
    conversation_language: String,
    provider: SavedProvider,
    model: String,
    effort: String,
    fast_mode: bool,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    subagent_child: bool,
}

#[derive(serde::Deserialize)]
struct SerdeSchemaProbe {
    schema_version: Option<Value>,
}

fn serde_decode_session_metadata(bytes: &[u8]) -> Result<SessionMetadata, SessionError> {
    if bytes.is_empty() || bytes.len() > MAX_SESSION_METADATA_BYTES {
        return Err(SessionError::SessionMetadataTooLarge);
    }
    let record: SerdeRecord = serde_json::from_slice(bytes).map_err(|_| {
        match serde_json::from_slice::<SerdeSchemaProbe>(bytes) {
            Ok(SerdeSchemaProbe {
                schema_version: Some(version),
            }) if version.as_u64() == Some(u64::from(SESSION_METADATA_SCHEMA_VERSION)) => {
                SessionError::InvalidSessionMetadata
            }
            Ok(SerdeSchemaProbe {
                schema_version: Some(_),
            }) => SessionError::UnsupportedSessionSchema,
            Ok(SerdeSchemaProbe {
                schema_version: None,
            })
            | Err(_) => SessionError::InvalidSessionFormat,
        }
    })?;
    if record.schema_version != SESSION_METADATA_SCHEMA_VERSION {
        return Err(SessionError::UnsupportedSessionSchema);
    }
    let effort =
        ReasoningEffort::parse(&record.effort).ok_or(SessionError::InvalidSessionMetadata)?;
    let metadata = SessionMetadata {
        id: record.id,
        origin_workspace_root: record.origin_workspace_root,
        workspace_root: record.workspace_root,
        created_at_ms: record.created_at_ms,
        updated_at_ms: record.updated_at_ms,
        conversation_language: record.conversation_language,
        preferences: SessionPreferences {
            provider: record.provider,
            model: record.model,
            effort,
            fast_mode: record.fast_mode,
        },
        title: record.title,
        subagent_child: record.subagent_child,
    };
    validate_session_metadata(&metadata).map_err(|_| SessionError::InvalidSessionMetadata)?;
    Ok(metadata)
}

fn metadata_samples() -> Vec<Value> {
    [
        "null",
        "true",
        "false",
        "0",
        "-0",
        "1",
        "2",
        "3",
        "4",
        "5",
        "-1",
        "255",
        "256",
        "4.0",
        "1.5",
        "1e2",
        "9223372036854775807",
        "9223372036854775808",
        "18446744073709551616",
        "-9223372036854775809",
        "\"\"",
        "\"4\"",
        "\"good\"",
        "\"/tmp/a\"",
        "\"en\"",
        "\"gateway\"",
        "\"codex\"",
        "\"router\"",
        "\"m\"",
        "\"auto\"",
        "\"high\"",
        "\"not valid\"",
        "\"0707070707070707070707070707070707070707070707070707070707070707\"",
        "[]",
        "[4]",
        "{}",
        "{\"a\":1}",
        "{\"name\":\"router\",\"binding\":\"0707070707070707070707070707070707070707070707070707070707070707\"}",
        "{\"name\":\"gateway\",\"binding\":\"0707070707070707070707070707070707070707070707070707070707070707\"}",
    ]
    .iter()
    .map(|text| serde_json::from_str(text).unwrap())
    .collect()
}

fn render_with(fields: &[(String, Value)]) -> String {
    let entries: Vec<String> = fields
        .iter()
        .map(|(key, value)| format!("{}:{value}", Value::from(key.as_str())))
        .collect();
    format!("{{{}}}", entries.join(","))
}

fn assert_same_metadata(text: &str, stricter: bool) -> usize {
    let serde = serde_decode_session_metadata(text.as_bytes());
    let hand = decode_session_metadata(text.as_bytes());
    if stricter && hand.is_err() && serde != hand {
        return 1;
    }
    assert_eq!(hand, serde, "{text}");
    0
}

#[test]
fn the_hand_metadata_decoder_matches_the_serde_decoder() {
    let mut configured = metadata("session");
    configured.preferences.provider =
        SavedProvider::new(ProviderId::Configured("router".to_owned()), Some([7; 32])).unwrap();
    configured.preferences.effort = ReasoningEffort::Named("high".to_owned());
    configured.title = Some("Title".to_owned());
    let samples = metadata_samples();
    let mut stricter = 0;
    let mut compared = 0;
    for value in [metadata("session"), configured] {
        let encoded = encode_session_metadata(&value).unwrap();
        assert_eq!(decode_session_metadata(&encoded).unwrap(), value);
        let Value::Object(document) = serde_json::from_slice::<Value>(&encoded).unwrap() else {
            panic!("metadata is an object");
        };
        let fields: Vec<(String, Value)> = document.into_iter().collect();
        assert_same_metadata(&render_with(&fields), false);
        for index in 0..fields.len() {
            let key = &fields[index].0;
            let mut removed = fields.clone();
            removed.remove(index);
            stricter += assert_same_metadata(&render_with(&removed), false);
            for sample in &samples {
                let mut replaced = fields.clone();
                replaced[index].1 = sample.clone();
                stricter += assert_same_metadata(&render_with(&replaced), false);
                let mut repeated = fields.clone();
                repeated.push((key.clone(), sample.clone()));
                stricter += assert_same_metadata(&render_with(&repeated), key == "schema_version");
                compared += 2;
            }
            let mut extended = fields.clone();
            extended.insert(index, ("unexpected".to_owned(), Value::Null));
            stricter += assert_same_metadata(&render_with(&extended), false);
            compared += 2;
        }
    }
    for sample in metadata_samples() {
        assert_same_metadata(&sample.to_string(), sample.is_array());
    }
    for broken in ["", "{", "{\"schema_version\":4", "{} []", "\u{feff}{}"] {
        assert_same_metadata(broken, false);
    }
    assert!(compared > 1_000, "{compared}");
    assert!(stricter > 0);
}

#[test]
fn the_hand_metadata_decoder_is_stricter_only_on_shapes_the_writer_never_writes() {
    let as_array =
        "[4,\"good\",\"/tmp/a\",\"/tmp/a\",1,1,\"en\",\"gateway\",\"m\",\"auto\",false,null,false]"
            .to_owned();
    let repeated_schema = document("").replace(
        "\"schema_version\":4",
        "\"schema_version\":4,\"schema_version\":4",
    );
    for (text, error) in [
        (as_array, SessionError::InvalidSessionFormat),
        (repeated_schema, SessionError::InvalidSessionMetadata),
    ] {
        assert_ne!(
            serde_decode_session_metadata(text.as_bytes()),
            Err(error),
            "{text}"
        );
        assert_eq!(
            decode_session_metadata(text.as_bytes()),
            Err(error),
            "{text}"
        );
    }
}
