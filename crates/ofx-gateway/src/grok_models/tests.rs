use super::*;
use serde_json::json;

fn model(id: &str) -> Value {
    json!({"model":id,"api_backend":"responses","context_window":500_123,"max_completion_tokens":32_768,"supports_reasoning_effort":true,"reasoning_efforts":[{"value":"xhigh"},{"value":"provider-next"}]})
}

#[test]
fn subscription_catalog_retains_order_and_duplicates() {
    let mut skipped = model("skip");
    skipped["api_backend"] = json!("chat_completions");
    let body = json!({"data":[model("current-b"),skipped,model("current-a"),model("current-b")]})
        .to_string();
    let models = parse_catalog(body.as_bytes()).unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["current-b", "current-a", "current-b"]
    );
}

#[test]
fn subscription_catalog_enforces_exact_count_identity_and_metadata_bounds() {
    for id in ["", "two words", "header\r\ninjected", &"x".repeat(257)] {
        assert_eq!(
            parse_catalog(json!({"data":[model(id)]}).to_string().as_bytes()),
            Err(CatalogFailure::MalformedResponse)
        );
    }
    assert!(
        parse_catalog(
            json!({"data":[model(&"x".repeat(256))]})
                .to_string()
                .as_bytes()
        )
        .is_ok()
    );
    assert_eq!(
        parse_catalog(json!({"data":vec![model("x");129]}).to_string().as_bytes()),
        Err(CatalogFailure::MalformedResponse)
    );
    assert!(parse_catalog(json!({"data":vec![model("x");128]}).to_string().as_bytes()).is_ok());
    for (key, value) in [
        ("context_window", json!(0)),
        ("context_window", json!(4_294_967_296_u64)),
        ("max_completion_tokens", json!(-1)),
        ("supports_reasoning_effort", json!(false)),
        ("reasoning_efforts", json!([{"value":"auto"}])),
        (
            "reasoning_efforts",
            json!([{"value":"high"},{"value":"high"}]),
        ),
    ] {
        let mut invalid = model("x");
        invalid[key] = value;
        assert_eq!(
            parse_catalog(json!({"data":[invalid]}).to_string().as_bytes()),
            Err(CatalogFailure::MalformedResponse),
            "{key}"
        );
    }
}

#[tokio::test]
async fn authenticated_catalog_fetches_version_and_models_without_redirects() {
    use ofx_testkit::{FakeServer, Reply};
    let server = FakeServer::start([
        Reply::status(200, "1.0.13\n"),
        Reply::status(200, json!({"data":[model("current")]}).to_string()),
        Reply::status(503, ""),
    ]);
    let endpoints = GrokModelsEndpoints {
        models: format!("{}/models", server.base_url()),
        modalities: format!("{}/modalities", server.base_url()),
        client_version: format!("{}/version", server.base_url()),
    };
    let catalog = GrokModelCatalog::new("test", endpoints, None).unwrap();
    assert_eq!(
        catalog
            .fetch(
                &CatalogCredential::new("access-secret".to_owned(), "account".to_owned()),
                &CancellationToken::new()
            )
            .await
            .unwrap()[0]
            .id,
        "current"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer access-secret")
    );
    assert_eq!(requests[1].header("x-userid"), Some("account"));
    assert_eq!(requests[1].header("x-grok-client-version"), Some("1.0.13"));
    assert_eq!(requests[2].header("x-xai-token-auth"), None);
}

#[tokio::test]
async fn oversized_and_redirected_catalogs_fail_with_exact_categories() {
    use ofx_testkit::{FakeServer, Reply};
    for (reply, expected) in [
        (
            Reply::status(200, "x".repeat(1024 * 1024 + 1)),
            CatalogFailure::MalformedResponse,
        ),
        (
            Reply::status_with_headers(302, &[("location", "http://127.0.0.1:1/never")], ""),
            CatalogFailure::HttpStatus,
        ),
        (Reply::status(401, "{}"), CatalogFailure::Authentication),
    ] {
        let server = FakeServer::start([Reply::status(200, "1.0.13\n"), reply]);
        let endpoints = GrokModelsEndpoints {
            models: format!("{}/models", server.base_url()),
            modalities: format!("{}/modalities", server.base_url()),
            client_version: format!("{}/version", server.base_url()),
        };
        let catalog = GrokModelCatalog::new("test", endpoints, None).unwrap();
        assert_eq!(
            catalog
                .fetch(
                    &CatalogCredential::new("secret".to_owned(), "account".to_owned()),
                    &CancellationToken::new()
                )
                .await,
            Err(expected)
        );
        assert_eq!(server.requests().len(), 2);
    }
}
