use serde_json::json;

use super::*;

fn model(slug: &str) -> Value {
    json!({
        "slug": slug,
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}],
        "additional_speed_tiers": [],
        "input_modalities": ["text", "image"],
        "context_window": 272_000,
    })
}

fn catalog(models: &[Value]) -> Vec<u8> {
    json!({ "models": models }).to_string().into_bytes()
}

fn with(mut model: Value, key: &str, value: Value) -> Value {
    model[key] = value;
    model
}

fn without(mut model: Value, key: &str) -> Value {
    model.as_object_mut().unwrap().remove(key);
    model
}

fn listed_ids(body: &[u8]) -> Option<Vec<String>> {
    parse_catalog(body).map(|models| models.into_iter().map(|model| model.id).collect())
}

fn codex_model(id: &str, efforts: &[&str], fast: bool) -> CodexModel {
    CodexModel {
        id: id.to_owned(),
        capabilities: ModelCapabilities {
            reasoning_efforts: efforts.iter().map(|effort| (*effort).to_owned()).collect(),
            supports_fast_mode: fast,
        },
    }
}

#[test]
fn codex_catalog_parser_keeps_visible_api_models_in_server_order() {
    let body = catalog(&[
        model("gpt-6.1-sol"),
        with(model("hidden"), "visibility", json!("hide")),
        with(model("unsupported"), "supported_in_api", json!(false)),
        with(
            model("gpt-5.6-luna"),
            "additional_speed_tiers",
            json!(["fast"]),
        ),
        without(
            without(
                without(
                    without(model("gpt-5.6-terra"), "supported_reasoning_levels"),
                    "context_window",
                ),
                "input_modalities",
            ),
            "additional_speed_tiers",
        ),
    ]);
    assert_eq!(
        parse_catalog(&body),
        Some(vec![
            codex_model("gpt-6.1-sol", &["low", "high"], false),
            codex_model("gpt-5.6-luna", &["low", "high"], true),
            codex_model("gpt-5.6-terra", &[], false),
        ])
    );
}

#[test]
fn reasoning_levels_keep_their_server_names_and_order() {
    let body = catalog(&[with(
        model("gpt-6.1-sol"),
        "supported_reasoning_levels",
        json!([
            {"effort": "xhigh", "description": "Deepest"},
            {"effort": "Minimal"},
            {"effort": "auto"},
        ]),
    )]);
    assert_eq!(
        parse_catalog(&body),
        Some(vec![codex_model(
            "gpt-6.1-sol",
            &["xhigh", "Minimal", "auto"],
            false
        )])
    );
}

#[test]
fn codex_catalog_without_the_reviewer_model_is_still_valid() {
    assert_eq!(
        listed_ids(&catalog(&[model("gpt-6.1-sol")])),
        Some(vec!["gpt-6.1-sol".to_owned()])
    );
    assert_eq!(parse_catalog(&catalog(&[])), Some(Vec::new()));
}

#[test]
fn hidden_models_still_need_their_visibility_and_api_flags() {
    for hidden in [
        without(with(model("x"), "visibility", json!("hide")), "slug"),
        with(
            with(model("x"), "supported_in_api", json!(false)),
            "context_window",
            json!(-1),
        ),
    ] {
        assert_eq!(parse_catalog(&catalog(&[hidden])), Some(Vec::new()));
    }
    for broken in [
        without(model("x"), "visibility"),
        with(model("x"), "visibility", json!("")),
        with(model("x"), "visibility", json!(1)),
        without(model("x"), "supported_in_api"),
        with(model("x"), "supported_in_api", json!("true")),
        without(
            with(model("x"), "visibility", json!("hide")),
            "supported_in_api",
        ),
    ] {
        assert_eq!(
            parse_catalog(&catalog(std::slice::from_ref(&broken))),
            None,
            "{broken}"
        );
    }
}

#[test]
fn any_invalid_listed_model_makes_the_whole_catalog_malformed() {
    let long_slug = "a".repeat(MAX_MODEL_ID_BYTES + 1);
    let efforts: Vec<Value> = (0..=MAX_REASONING_EFFORTS)
        .map(|_| json!({"effort": "low"}))
        .collect();
    let tiers: Vec<Value> = (0..=MAX_LISTED_VALUES).map(|_| json!("slow")).collect();
    for broken in [
        without(model("x"), "slug"),
        with(model("x"), "slug", json!("")),
        with(model("x"), "slug", json!("gpt 6")),
        with(model("x"), "slug", json!("gpt\u{7f}")),
        with(model("x"), "slug", json!(long_slug)),
        with(model("x"), "supported_reasoning_levels", json!({})),
        with(model("x"), "supported_reasoning_levels", json!(efforts)),
        with(model("x"), "supported_reasoning_levels", json!(["low"])),
        with(model("x"), "supported_reasoning_levels", json!([{}])),
        with(
            model("x"),
            "supported_reasoning_levels",
            json!([{"effort": ""}]),
        ),
        with(
            model("x"),
            "supported_reasoning_levels",
            json!([{"effort": "very high"}]),
        ),
        with(model("x"), "context_window", json!(-1)),
        with(model("x"), "context_window", json!(272_000.5)),
        with(model("x"), "context_window", json!(272_000.0)),
        with(model("x"), "context_window", json!(u64::from(u32::MAX) + 1)),
        with(model("x"), "context_window", json!("272000")),
        with(model("x"), "input_modalities", json!("image")),
        with(model("x"), "input_modalities", json!([1])),
        with(model("x"), "additional_speed_tiers", json!(tiers)),
        with(model("x"), "additional_speed_tiers", json!([null, "fast"])),
        json!("gpt-6.1-sol"),
    ] {
        let body = catalog(&[model("gpt-6.1-sol"), broken.clone()]);
        assert_eq!(parse_catalog(&body), None, "{broken}");
    }
}

#[test]
fn listed_values_after_the_matching_capability_are_not_checked() {
    let body = catalog(&[
        with(model("a"), "input_modalities", json!(["image", 1])),
        with(model("b"), "additional_speed_tiers", json!(["fast", null])),
        with(model("c"), "context_window", Value::Null),
    ]);
    assert_eq!(
        parse_catalog(&body),
        Some(vec![
            codex_model("a", &["low", "high"], false),
            codex_model("b", &["low", "high"], true),
            codex_model("c", &["low", "high"], false),
        ])
    );
}

#[test]
fn catalog_shape_and_size_limits_are_enforced() {
    let full: Vec<Value> = (0..MAX_CATALOG_MODELS)
        .map(|index| model(&format!("m{index}")))
        .collect();
    assert_eq!(
        listed_ids(&catalog(&full)).map(|ids| ids.len()),
        Some(MAX_CATALOG_MODELS)
    );
    let mut over = full;
    over.push(model("extra"));
    assert_eq!(parse_catalog(&catalog(&over)), None);
    for body in [
        &b"[]"[..],
        b"{}",
        br#"{"models":{}}"#,
        br#"{"models":[],"models":[]}"#,
        br#"{"models":[{"slug":"a","slug":"b","visibility":"list","supported_in_api":true}]}"#,
        b"not json",
    ] {
        assert_eq!(parse_catalog(body), None, "{body:?}");
    }
}

#[test]
fn codex_catalog_url_uses_the_resolved_compatibility_version() {
    let version = Version::parse("0.999.1").unwrap();
    assert_eq!(
        models_url(
            "https://chatgpt.com/backend-api/codex/models",
            Some(&version)
        ),
        "https://chatgpt.com/backend-api/codex/models?client_version=0.999.1"
    );
    assert_eq!(
        models_url("http://127.0.0.1:1/models?x=1", Some(&version)),
        "http://127.0.0.1:1/models?x=1&client_version=0.999.1"
    );
    assert_eq!(
        models_url("https://chatgpt.com/backend-api/codex/models", None),
        "https://chatgpt.com/backend-api/codex/models"
    );
}

#[test]
fn catalog_credentials_never_print_their_token_or_account() {
    let credential = CatalogCredential::new("secret-token".to_owned(), "acct_9f2c".to_owned());
    let debug = format!("{credential:?}");
    assert!(!debug.contains("secret-token"));
    assert!(!debug.contains("acct_9f2c"));
}
