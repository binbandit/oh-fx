use serde_json::{Value, json};

use super::*;
use crate::elicitation::form::parse_form_schema;

const LEGACY_06: ElicitationWire = ElicitationWire::LegacyMcp2025_06;
const LEGACY_11: ElicitationWire = ElicitationWire::LegacyMcp2025_11;

fn schema(text: &str) -> Outcome<Vec<Field>> {
    let value: Value = serde_json::from_str(text).unwrap();
    parse_form_schema(LEGACY_11, &value).map(|form| form.fields)
}

fn request(wire: ElicitationWire, text: &str) -> Outcome<Mode> {
    parse_request(wire, &serde_json::from_str(text).unwrap()).map(|request| request.mode())
}

fn form(text: &str) -> FormSchema {
    let value: Value = serde_json::from_str(text).unwrap();
    parse_form_schema(LEGACY_11, &value).unwrap()
}

fn content(form: &FormSchema, entries: &[(&str, &str)]) -> Outcome<()> {
    let entries: Vec<(String, String)> = entries
        .iter()
        .map(|(name, json)| ((*name).to_owned(), (*json).to_owned()))
        .collect();
    form.validate_answers(&entries)
}

fn values(fields: &[Field], text: &str) -> Outcome<()> {
    let values: Vec<Value> = serde_json::from_str(text).unwrap();
    fields
        .iter()
        .zip(&values)
        .try_for_each(|(field, value)| field.validate_value(value))
}

#[test]
fn form_schemas_parse_every_supported_field_kind_and_validate_values() {
    let form = schema(
        r#"{"type":"object","additionalProperties":false,"properties":{"name":{"type":"string","minLength":2,"maxLength":20,"pattern":"^[A-Za-z]+$"},"age":{"type":"integer","minimum":18,"maximum":120},"ratio":{"type":"number","minimum":0,"maximum":1},"enabled":{"type":"boolean","default":true},"color":{"type":"string","oneOf":[{"const":"red","title":"Red"},{"const":"blue","title":"Blue"}]},"tags":{"type":"array","items":{"anyOf":[{"const":"a","title":"A"},{"const":"b","title":"B"}]},"minItems":1}},"required":["name","age"]}"#,
    )
    .unwrap();
    let kinds: Vec<_> = form.iter().map(|field| field.kind).collect();
    assert_eq!(
        kinds,
        [
            FieldKind::String,
            FieldKind::Integer,
            FieldKind::Number,
            FieldKind::Boolean,
            FieldKind::SingleSelect,
            FieldKind::MultiSelect
        ]
    );
    assert_eq!(
        values(&form, r#"["Alice",42,0.5,false,"red",["a","b"]]"#),
        Ok(())
    );
    for invalid in [
        r#"["A1"]"#,
        r#"["Alice",17]"#,
        r#"["Alice",42.5]"#,
        r#"["Alice",42,1.5]"#,
        r#"["Alice",42,0.5,"true"]"#,
        r#"["Alice",42,0.5,true,"green"]"#,
        r#"["Alice",42,0.5,true,"red",["a","a"]]"#,
        r#"["Alice",42,0.5,true,"red",[]]"#,
        r#"["Alice",42,0.5,true,"red",["c"]]"#,
    ] {
        assert_eq!(
            values(&form, invalid),
            Err(ElicitationError::InvalidResponse),
            "{invalid}"
        );
    }
}

#[test]
fn secret_fields_are_rejected_before_a_form_is_published() {
    for text in [
        r#"{"type":"object","properties":{"api_key":{"type":"string"}}}"#,
        r#"{"type":"object","properties":{"accessToken":{"type":"string"}}}"#,
        r#"{"type":"object","properties":{"value":{"type":"string","title":"PrivateKey"}}}"#,
    ] {
        assert_eq!(schema(text), Err(ElicitationError::SecretField), "{text}");
    }
    assert!(
        schema(r#"{"type":"object","properties":{"value":{"type":"string","description":"Do not give us your phone number, pin, or other sensitive info"}}}"#)
            .is_ok()
    );
}

#[test]
fn secret_classification_covers_normalized_forms_without_truncation_bypasses() {
    for name in [
        "token",
        "client_secret",
        "paymentCredential",
        "PASSWORD",
        "user-passcode",
        "otp",
        "userPin",
        "api key",
        "APIKey",
        "access.token",
        "privateKey",
        "credit-card",
    ] {
        assert!(secret::is_secret_field(name, None), "{name}");
    }
    assert!(!secret::is_secret_field(
        "shipping_address",
        Some("Shipping address")
    ));
    let long_name = format!("{}AccessToken", "a".repeat(1536 - "AccessToken".len()));
    assert!(secret::is_secret_field(&long_name, None));
    let description = format!("{} private key", "a".repeat(900));
    let text = format!(
        r#"{{"type":"object","properties":{{"value":{{"type":"string","description":"{description}"}}}}}}"#
    );
    assert!(schema(&text).is_ok());
}

#[test]
fn legacy_schemas_are_explicitly_scoped_to_their_negotiated_revision() {
    let with_enum_names = r#"{"message":"Choose","requestedSchema":{"type":"object","properties":{"choice":{"type":"string","enum":["a"],"enumNames":["A"]}}}}"#;
    assert_eq!(request(LEGACY_06, with_enum_names), Ok(Mode::Form));
    assert_eq!(
        request(
            LEGACY_06,
            r#"{"mode":"form","message":"No explicit mode","requestedSchema":{"type":"object","properties":{}}}"#
        ),
        Err(ElicitationError::UnsupportedMode)
    );
    assert_eq!(
        request(
            LEGACY_06,
            r#"{"message":"No pattern","requestedSchema":{"type":"object","properties":{"value":{"type":"string","pattern":"^[a-z]+$"}}}}"#
        ),
        Err(ElicitationError::UnsupportedSchema)
    );
    assert_eq!(
        request(
            LEGACY_06,
            r#"{"message":"No arrays","requestedSchema":{"type":"object","properties":{"value":{"type":"array","items":{"type":"string","enum":["a"]}}}}}"#
        ),
        Err(ElicitationError::UnsupportedSchema)
    );
    assert_eq!(
        request(
            LEGACY_11,
            r#"{"mode":"form","message":"Pattern","requestedSchema":{"type":"object","properties":{"value":{"type":"string","pattern":"^[a-z]+$"}}}}"#,
        ),
        Ok(Mode::Form)
    );
    assert_eq!(request(LEGACY_11, with_enum_names), Ok(Mode::Form));
    assert_eq!(
        request(
            LEGACY_11,
            r#"{"message":"Names","requestedSchema":{"type":"object","properties":{"choice":{"type":"string","enum":["a"],"enumNames":["A","B"]}}}}"#,
        ),
        Err(ElicitationError::InvalidSchema)
    );
}

#[test]
fn exact_numeric_constraints_retain_large_integer_and_fractional_values() {
    let form = schema(
        r#"{"type":"object","properties":{"large":{"type":"integer","minimum":9007199254740993,"maximum":9007199254740995,"default":9007199254740994},"fraction":{"type":"number","minimum":0.1,"maximum":0.3,"multipleOf":0.1,"default":0.3}},"required":["large","fraction"]}"#,
    )
    .unwrap();
    assert_eq!(values(&form, "[9007199254740994,0.3]"), Ok(()));
    assert_eq!(
        values(&form, "[9007199254740992,0.3]"),
        Err(ElicitationError::InvalidResponse)
    );
    assert_eq!(
        values(&form, "[9007199254740995,0.25]"),
        Err(ElicitationError::InvalidResponse)
    );
    assert_eq!(
        schema(
            r#"{"type":"object","properties":{"value":{"type":"number","multipleOf":0.1,"default":0.25}}}"#
        ),
        Err(ElicitationError::InvalidResponse)
    );
    assert_eq!(
        schema(r#"{"type":"object","properties":{"value":{"type":"integer","default":1.5}}}"#),
        Err(ElicitationError::InvalidResponse)
    );
}

#[test]
fn ambiguous_schemas_are_rejected_before_interaction() {
    let cases = [
        (
            r#"{"type":"object","properties":{"color":{"type":"string","enum":["red","red"]}}}"#,
            ElicitationError::InvalidSchema,
        ),
        (
            r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name","name"]}"#,
            ElicitationError::InvalidSchema,
        ),
        (
            r#"{"type":"object","additionalProperties":true,"properties":{}}"#,
            ElicitationError::UnsupportedSchema,
        ),
        (
            r#"{"type":"object","title":"Form","properties":{}}"#,
            ElicitationError::UnsupportedSchema,
        ),
        (
            r#"{"type":"object","properties":{"choice":{"type":"string","oneOf":[{"const":"a","title":"A","description":"no"}]}}}"#,
            ElicitationError::UnsupportedSchema,
        ),
        (
            r#"{"type":"object","properties":{"value":{"type":"string","minLength":3,"maxLength":2}}}"#,
            ElicitationError::InvalidSchema,
        ),
        (
            r#"{"type":"object","properties":{"value":{"type":"number","multipleOf":0}}}"#,
            ElicitationError::InvalidSchema,
        ),
        (
            r#"{"type":"object","properties":{"value":{"type":"number","minimum":2,"maximum":1}}}"#,
            ElicitationError::InvalidSchema,
        ),
        (
            r#"{"type":"object","properties":{"value":{"type":"string","default":7}}}"#,
            ElicitationError::InvalidResponse,
        ),
        (
            r#"{"type":"object","properties":{"value":{"type":"object"}}}"#,
            ElicitationError::UnsupportedSchema,
        ),
        (
            r#"{"type":"object","properties":{"value":{"type":"string","minLength":1.0}}}"#,
            ElicitationError::InvalidSchema,
        ),
        (
            r#"{"type":"object","properties":{"value":{"type":"string","maxLength":65537}}}"#,
            ElicitationError::LimitExceeded,
        ),
        (
            r#"{"type":"object","properties":[]}"#,
            ElicitationError::LimitExceeded,
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(schema(text), Err(expected), "{text}");
    }
}

#[test]
fn unknown_formats_are_not_widened_onto_mcp() {
    assert_eq!(
        schema(
            r#"{"type":"object","properties":{"value":{"type":"string","format":"future-format"}}}"#
        ),
        Err(ElicitationError::UnsupportedSchema)
    );
}

#[test]
fn known_formats_use_bounded_structural_validation() {
    let field = |format: &str| {
        let form = schema(&format!(
            r#"{{"type":"object","properties":{{"value":{{"type":"string","format":"{format}"}}}}}}"#
        ))
        .unwrap();
        form[0].clone()
    };
    let accepts = |field: &Field, text: &str| field.validate_value(&json!(text)).is_ok();
    let email = field("email");
    assert!(accepts(&email, "user@example.test"));
    assert!(!accepts(&email, "not-an-email"));
    assert!(!accepts(&email, "@example.test"));
    let date = field("date");
    assert!(accepts(&date, "2024-02-29"));
    assert!(!accepts(&date, "2023-02-29"));
    assert!(accepts(&date, "2024-+2-05"));
    let date_time = field("date-time");
    assert!(accepts(&date_time, "2026-08-02T12:34:56.123-04:00"));
    assert!(accepts(&date_time, "2026-08-02t12:34:60Z"));
    assert!(!accepts(&date_time, "2026-08-02T99:34:56Z"));
    assert!(!accepts(&date_time, "2026-08-02T12:34:56."));
    assert!(!accepts(&date_time, "2026-08-02T12:34:56"));
    let uri = field("uri");
    assert!(accepts(&uri, "urn:isbn:0451450523"));
    assert!(accepts(&uri, "file:///"));
    assert!(!accepts(&uri, "no scheme"));
    assert!(!accepts(&uri, "foobar://"));
    assert!(!accepts(&uri, "http://host:99999/"));
}

#[test]
fn elicitation_patterns_use_bounded_codepoint_semantics() {
    let form = schema(
        r#"{"type":"object","properties":{"glyph":{"type":"string","pattern":"^.{1}$"},"space":{"type":"string","pattern":"^\\s$"}},"required":["glyph","space"]}"#,
    )
    .unwrap();
    assert_eq!(values(&form, r#"["\u03c0","\u00a0"]"#), Ok(()));
    assert_eq!(
        values(&form, r#"["\u03c0a","\u00a0"]"#),
        Err(ElicitationError::InvalidResponse)
    );
    assert_eq!(
        schema(r#"{"type":"object","properties":{"value":{"type":"string","pattern":"(?=x)"}}}"#),
        Err(ElicitationError::InvalidSchema)
    );
    assert_eq!(
        schema(
            r#"{"type":"object","properties":{"value":{"type":"string","pattern":"^a{1024}a{1024}$"}}}"#
        ),
        Err(ElicitationError::LimitExceeded)
    );
    let long = format!(
        r#"{{"type":"object","properties":{{"value":{{"type":"string","pattern":"{}"}}}}}}"#,
        "a".repeat(513)
    );
    assert_eq!(schema(&long), Err(ElicitationError::LimitExceeded));
}

#[test]
fn duplicate_titled_alternatives_remain_valid_distinct_schema_choices() {
    let form = schema(
        r#"{"type":"object","properties":{"choice":{"type":"string","oneOf":[{"const":"first","title":"Duplicate"},{"const":"second","title":"Duplicate"}]}},"required":["choice"]}"#,
    )
    .unwrap();
    assert_eq!(form.len(), 1);
    assert_eq!(values(&form, r#"["second"]"#), Ok(()));
}

#[test]
fn url_classification_permits_secure_and_loopback_urls_only() {
    assert_eq!(
        uri::classify_url("https://example.test/full/path?state=opaque"),
        Ok(b"example.test".to_vec())
    );
    assert_eq!(
        uri::classify_url("http://localhost:4321/callback"),
        Ok(b"localhost".to_vec())
    );
    assert_eq!(uri::classify_url("HTTP://[::1]:80/"), Ok(b"[::1]".to_vec()));
    assert_eq!(
        uri::classify_url("https://ex%41mple.test/"),
        Ok(b"exAmple.test".to_vec())
    );
    assert_eq!(
        uri::classify_url("http://example.test"),
        Err(ElicitationError::InsecureUrl)
    );
    for invalid in [
        "https://user@example.test",
        "https://@example.test",
        " https://example.test",
        "https:///path",
        "https://:443/",
        "https://example.test:port/",
        "",
        "not a url",
    ] {
        assert_eq!(
            uri::classify_url(invalid),
            Err(ElicitationError::InvalidUrl),
            "{invalid}"
        );
    }
}

#[test]
fn requests_are_parsed_in_upstream_order_with_bounded_fields() {
    let cases = [
        ("[]", ElicitationError::InvalidRequest),
        (
            r#"{"mode":"later","message":"x"}"#,
            ElicitationError::UnsupportedMode,
        ),
        (
            r#"{"mode":7,"message":"x"}"#,
            ElicitationError::UnsupportedMode,
        ),
        (
            r#"{"requestedSchema":{"type":"object","properties":{}}}"#,
            ElicitationError::InvalidRequest,
        ),
        (r#"{"message":"x"}"#, ElicitationError::InvalidRequest),
        (
            r#"{"mode":"url","message":"x","url":"https://example.test","requestedSchema":{}}"#,
            ElicitationError::InvalidRequest,
        ),
        (
            r#"{"mode":"url","message":"x","url":"https://example.test"}"#,
            ElicitationError::InvalidRequest,
        ),
        (
            r#"{"mode":"url","message":"x","url":"http://example.test","elicitationId":"e"}"#,
            ElicitationError::InsecureUrl,
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(request(LEGACY_11, text), Err(expected), "{text}");
    }
    assert_eq!(
        request(
            LEGACY_11,
            r#"{"mode":"url","message":"Open","url":"https://example.test","elicitationId":"e"}"#,
        ),
        Ok(Mode::Url)
    );
    assert_eq!(
        request(
            LEGACY_06,
            r#"{"message":"Open","requestedSchema":{"type":"object","properties":{}},"elicitationId":"e"}"#,
        ),
        Ok(Mode::Form)
    );
    let message = json!({"message": "x".repeat(8 * 1024 + 1), "requestedSchema": {"type":"object","properties":{}}});
    assert_eq!(
        parse_request(LEGACY_11, &message),
        Err(ElicitationError::InvalidRequest)
    );
    let oversized = json!({"message": "x", "requestedSchema": {"type":"object","properties":{}}, "pad": "x".repeat(128 * 1024)});
    assert_eq!(
        parse_request(LEGACY_11, &oversized),
        Err(ElicitationError::LimitExceeded)
    );
}

#[test]
fn request_parsers_stay_bounded_under_fuzzed_values() {
    let mut seed: u64 = 0x5eed;
    let alphabet: Vec<Value> = vec![
        json!(null),
        json!(true),
        json!(1),
        json!(-1),
        json!(0.5),
        json!("form"),
        json!("url"),
        json!("string"),
        json!([]),
        json!({}),
        json!({"type": "object", "properties": {}}),
    ];
    let keys = [
        "mode",
        "message",
        "requestedSchema",
        "url",
        "elicitationId",
        "type",
        "properties",
        "required",
    ];
    for _ in 0..2000 {
        let mut object = serde_json::Map::new();
        for key in keys {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let pick = usize::try_from(seed >> 59).unwrap();
            if let Some(value) = alphabet.get(pick) {
                object.insert(key.to_owned(), value.clone());
            }
        }
        let value = Value::Object(object);
        let _ = parse_request(LEGACY_11, &value);
        let _ = parse_request(LEGACY_06, &value);
    }
}

#[test]
fn bracketed_hosts_end_at_their_bracket_as_upstream_parses_them() {
    assert_eq!(
        uri::classify_url("http://[::1]x:80/"),
        Ok(b"[::1]".to_vec())
    );
    assert!(uri::parse("x://u]@[h:1").is_none());
    assert!(uri::parse("x://[h]:port").is_none());
}

#[test]
fn capability_modes_report_support() {
    let form = ElicitationCapabilities {
        form: true,
        url: false,
    };
    assert!(form.supports(Mode::Form) && !form.supports(Mode::Url));
    assert!(form.any() && !ElicitationCapabilities::default().any());
}

#[test]
fn form_content_needs_every_required_field_and_no_unknown_field() {
    let profile = form(
        r#"{"type":"object","properties":{"name":{"type":"string","title":"Name","description":"Who"},"age":{"type":"integer","minimum":18},"color":{"type":"string","enum":["red"],"enumNames":["Red"],"default":"red"}},"required":["name","age"]}"#,
    );
    assert_eq!(profile.fields[0].display_name(), "Name");
    assert_eq!(profile.fields[1].display_name(), "age");
    assert_eq!(profile.fields[0].description.as_deref(), Some("Who"));
    assert_eq!(profile.fields[2].default_json.as_deref(), Some("\"red\""));
    assert_eq!(profile.fields[2].choices[0].title, "Red");
    assert!(profile.fields[0].required && !profile.fields[2].required);
    assert_eq!(
        content(&profile, &[("name", "\"Ada\""), ("age", "42")]),
        Ok(())
    );
    assert_eq!(
        content(&profile, &[("name", "\"Ada\""), ("age", "1e400")]),
        Ok(())
    );
    let invalid: [&[(&str, &str)]; 5] = [
        &[("age", "42")],
        &[("name", "\"Ada\""), ("age", "17")],
        &[("name", "\"Ada\""), ("age", "42"), ("extra", "1")],
        &[("name", "\"Ada\""), ("age", "42"), ("color", "\"blue\"")],
        &[("name", "7"), ("age", "42")],
    ];
    for entries in invalid {
        assert_eq!(
            content(&profile, entries),
            Err(ElicitationError::InvalidResponse),
            "{entries:?}"
        );
    }
}

#[test]
fn typed_numbers_are_checked_from_their_exact_lexeme() {
    let numbers = form(
        r#"{"type":"object","properties":{"fraction":{"type":"number","minimum":0.1,"maximum":0.3,"multipleOf":0.1}}}"#,
    );
    let fraction = &numbers.fields[0];
    assert_eq!(
        validate_field_json(fraction, "0.300000000000000000000000000000"),
        Ok(())
    );
    assert_eq!(
        validate_field_json(fraction, "0.300000000000000000000000000001"),
        Err(ElicitationError::InvalidResponse)
    );
    assert_eq!(validate_field_json(fraction, " 0.2 "), Ok(()));
    for invalid in ["0.25", "\"0.2\"", "01", "-", "true", "0.4", "1e999999999"] {
        assert_eq!(
            validate_field_json(fraction, invalid),
            Err(ElicitationError::InvalidResponse),
            "{invalid}"
        );
    }
}

#[test]
fn answers_are_validated_against_their_request_and_written_as_upstream_does() {
    let name = parse_request(
        LEGACY_11,
        &json!({"message":"Name","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}),
    )
    .unwrap();
    assert_eq!(name.message, "Name");
    let accepted = Answer::AcceptForm(vec![("name".to_owned(), "\"Ada\"".to_owned())]);
    assert_eq!(validate_answer(&name, &accepted), Ok(()));
    assert_eq!(
        accepted.to_json(),
        r#"{"action":"accept","content":{"name":"Ada"}}"#
    );
    assert_eq!(validate_answer(&name, &Answer::Decline), Ok(()));
    assert_eq!(Answer::Decline.to_json(), r#"{"action":"decline"}"#);
    assert_eq!(Answer::Cancel.to_json(), r#"{"action":"cancel"}"#);
    assert_eq!(
        validate_answer(&name, &Answer::AcceptForm(Vec::new())),
        Err(ElicitationError::InvalidResponse)
    );
    let oversized = Answer::AcceptForm(vec![(
        "name".to_owned(),
        format!("\"{}\"", "x".repeat(MAX_RESPONSE_BYTES)),
    )]);
    assert_eq!(
        validate_answer(&name, &oversized),
        Err(ElicitationError::LimitExceeded)
    );
    let url = parse_request(
        LEGACY_11,
        &json!({"mode":"url","message":"Open","url":"https://example.test","elicitationId":"e"}),
    )
    .unwrap();
    assert_eq!(
        validate_answer(&url, &accepted),
        Err(ElicitationError::InvalidResponse)
    );
}
