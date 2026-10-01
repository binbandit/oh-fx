use std::path::PathBuf;

use ofx_contract::ModelCapabilities;
use ofx_gateway::{
    CatalogCredential, CatalogFailure, CodexModel, CodexModelCatalog, CodexModelsEndpoints,
};
use ofx_testkit::{FakeServer, Reply};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJjYXRhbG9nIn0.c2lnbmF0dXJl";
const ACCOUNT: &str = "acct_catalog";

fn release(version: &str) -> Reply {
    Reply::status(200, json!({"version": version}).to_string())
}

fn listed(slug: &str) -> serde_json::Value {
    json!({
        "slug": slug,
        "visibility": "list",
        "supported_in_api": true,
        "supported_reasoning_levels": [{"effort": "medium"}],
        "input_modalities": ["text", "image"],
        "context_window": 272_000,
    })
}

fn catalog_reply() -> Reply {
    Reply::status(
        200,
        json!({"models": [
            listed("gpt-6.1-sol"),
            {"slug": "internal", "visibility": "hide", "supported_in_api": true},
            listed("gpt-5.6-terra"),
            {
                "slug": "gpt-5.6-luna",
                "visibility": "list",
                "supported_in_api": true,
                "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}],
                "additional_speed_tiers": ["fast"],
            },
        ]})
        .to_string(),
    )
}

fn catalog(server: &FakeServer, cache_directory: Option<PathBuf>) -> CodexModelCatalog {
    CodexModelCatalog::new(
        "oh-fx/test",
        CodexModelsEndpoints {
            models: format!("{}/backend-api/codex/models", server.base_url()),
            client_version: format!("{}/@openai/codex/latest", server.base_url()),
        },
        cache_directory,
    )
    .expect("build the catalog client")
}

fn credential() -> CatalogCredential {
    CatalogCredential::new(TOKEN.to_owned(), ACCOUNT.to_owned())
}

#[tokio::test]
async fn the_catalog_is_fetched_with_the_subscription_and_the_live_client_version() {
    let server = FakeServer::start([release("0.153.1"), catalog_reply()]);
    let models = catalog(&server, None)
        .fetch(Some(&credential()), &CancellationToken::new())
        .await;
    let model = |id: &str, efforts: &[&str], fast| CodexModel {
        id: id.to_owned(),
        capabilities: ModelCapabilities {
            reasoning_efforts: efforts.iter().map(|effort| (*effort).to_owned()).collect(),
            supports_fast_mode: fast,
        },
    };
    assert_eq!(
        models,
        Ok(vec![
            model("gpt-6.1-sol", &["medium"], false),
            model("gpt-5.6-terra", &["medium"], false),
            model("gpt-5.6-luna", &["low", "high"], true),
        ])
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/v1/@openai/codex/latest");
    assert_eq!(requests[0].header("authorization"), None);
    let request = &requests[1];
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.path,
        "/v1/backend-api/codex/models?client_version=0.153.1"
    );
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(request.header("chatgpt-account-id"), Some(ACCOUNT));
    assert_eq!(request.header("originator"), Some("oh-fx"));
    assert_eq!(request.header("accept"), Some("application/json"));
    assert_eq!(request.header("user-agent"), Some("oh-fx/test"));
    assert_eq!(request.header("accept-encoding"), None);
}

#[tokio::test]
async fn host_managed_catalogs_send_no_local_credentials_or_version() {
    let server = FakeServer::start([catalog_reply()]);
    let models = catalog(&server, None)
        .fetch(None, &CancellationToken::new())
        .await
        .expect("load the catalog");
    assert_eq!(models.len(), 3);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/backend-api/codex/models");
    assert_eq!(requests[0].header("authorization"), None);
    assert_eq!(requests[0].header("chatgpt-account-id"), None);
}

#[tokio::test]
async fn http_failures_keep_their_catalog_category() {
    for (reply, expected) in [
        (Reply::status(401, "{}"), CatalogFailure::Authentication),
        (Reply::status(403, "{}"), CatalogFailure::Authentication),
        (Reply::status(429, "{}"), CatalogFailure::RateLimited),
        (Reply::status(503, "{}"), CatalogFailure::GatewayUnavailable),
        (Reply::status(404, "{}"), CatalogFailure::HttpStatus),
        (
            Reply::status_with_headers(302, &[("location", "https://example.com/login")], ""),
            CatalogFailure::HttpStatus,
        ),
        (
            Reply::status(200, "{\"models\":[{\"slug\":\"x\"}]}"),
            CatalogFailure::MalformedResponse,
        ),
        (
            Reply::status(200, "x".repeat(4 * 1024 * 1024 + 1)),
            CatalogFailure::Transport,
        ),
    ] {
        let server = FakeServer::start([release("0.153.1"), reply]);
        let failure = catalog(&server, None)
            .fetch(Some(&credential()), &CancellationToken::new())
            .await;
        assert_eq!(failure, Err(expected));
        assert_eq!(server.requests().len(), 2, "{expected:?}");
    }
}

#[tokio::test]
async fn a_failed_version_lookup_without_a_cache_fails_as_transport() {
    let server = FakeServer::start([Reply::status(503, "")]);
    let failure = catalog(&server, None)
        .fetch(Some(&credential()), &CancellationToken::new())
        .await;
    assert_eq!(failure, Err(CatalogFailure::Transport));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn the_client_version_is_cached_for_the_next_fetch() {
    let cache = tempfile::tempdir().expect("create a cache directory");
    let server = FakeServer::start([release("0.153.1"), catalog_reply(), catalog_reply()]);
    let catalog = catalog(&server, Some(cache.path().join("oh-fx")));
    for _ in 0..2 {
        catalog
            .fetch(Some(&credential()), &CancellationToken::new())
            .await
            .expect("load the catalog");
    }
    let paths: Vec<String> = server
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert_eq!(
        paths,
        [
            "/v1/@openai/codex/latest",
            "/v1/backend-api/codex/models?client_version=0.153.1",
            "/v1/backend-api/codex/models?client_version=0.153.1",
        ]
    );
    assert!(
        cache
            .path()
            .join("oh-fx/provider-versions/codex.json")
            .is_file()
    );
}

#[tokio::test]
async fn a_cancelled_fetch_sends_nothing() {
    let server = FakeServer::start([release("0.153.1"), catalog_reply()]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let failure = catalog(&server, None)
        .fetch(Some(&credential()), &cancel)
        .await;
    assert_eq!(failure, Err(CatalogFailure::Cancellation));
    assert!(server.requests().is_empty());
}
