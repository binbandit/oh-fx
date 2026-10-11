use ofx_testkit::{FakeServer, RefusedPort, Reply};
use zeroize::Zeroizing;

use super::*;
use crate::model_catalog::PublicOnlyReason;

const KEY: &str = "vck_live_0123456789abcdef";

fn parsed(body: &str) -> Vec<GatewayModel> {
    parse_catalog(body.as_bytes()).unwrap()
}

fn listing(ids: &[&str]) -> Reply {
    let entries: Vec<String> = ids
        .iter()
        .map(|id| format!(r#"{{"id":"{id}","type":"language","tags":["tool-use"]}}"#))
        .collect();
    Reply::status(200, format!(r#"{{"data":[{}]}}"#, entries.join(",")))
}

fn catalog(server: &FakeServer) -> GatewayModelCatalog {
    GatewayModelCatalog::new(
        "oh-fx/test",
        GatewayModelsEndpoints {
            models: format!("http://{}{GATEWAY_MODELS_PATH}", server.address()),
        },
    )
    .unwrap()
}

fn keyed(explicit: bool) -> CatalogAccess {
    CatalogAccess::Authenticated {
        source: "ai_gateway_api_key",
        credential: Zeroizing::new(KEY.to_owned()),
        login_team: None,
        explicit,
    }
}

fn signed_in(team: &str) -> CatalogAccess {
    CatalogAccess::Authenticated {
        source: "fx_login",
        credential: Zeroizing::new(KEY.to_owned()),
        login_team: Some(team.to_owned()),
        explicit: false,
    }
}

fn ids(fetch: &CatalogFetch<GatewayModel>) -> Vec<&str> {
    match fetch {
        CatalogFetch::Loaded { models, .. } => {
            models.iter().map(|model| model.id.as_str()).collect()
        }
        CatalogFetch::Failed { .. } => panic!("{fetch:?}"),
    }
}

#[test]
fn only_language_entries_with_string_ids_are_listed_in_catalog_order() {
    let models = parsed(
        r#"{"object":"list","data":[
            {"id":"vendor/image-1","type":"image-generation","released":30,"tags":["image-generation"]},
            {"id":"vendor/small-1","type":"LANGUAGE","released":20,"tags":["tool-use"]},
            {"id":"openai/gpt-5","type":"language","released":40,"tags":["tool-use"]},
            {"id":"vendor/untyped-1","released":50,"tags":["Tool-Use"]},
            {"id":"vendor/odd-type","type":7,"released":5},
            {"id":7,"type":"language"},
            "not an object"
        ]}"#,
    );
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "openai/gpt-5",
            "vendor/untyped-1",
            "vendor/small-1",
            "vendor/odd-type"
        ]
    );
}

#[test]
fn tool_use_models_come_before_unsupported_ones() {
    let models = parsed(
        r#"{"data":[
            {"id":"openai/gpt-5","released":10,"tags":[]},
            {"id":"openai/gpt-5-mini","released":9,"tags":["tool-use"]}
        ]}"#,
    );
    assert_eq!(models[0].id, "openai/gpt-5-mini");
    assert_eq!(models[1].id, "openai/gpt-5");
}

#[test]
fn malformed_envelopes_are_refused_and_an_empty_list_is_kept() {
    for body in [
        "",
        "[]",
        "{}",
        r#"{"data":{}}"#,
        r#"{"data":null}"#,
        r#"{"data":[],"data":[]}"#,
        "{not json",
    ] {
        assert_eq!(parse_catalog(body.as_bytes()), None, "{body}");
    }
    assert_eq!(parsed(r#"{"data":[]}"#), Vec::new());
}

#[tokio::test]
async fn a_signed_in_catalog_request_sends_the_token_and_team_query_without_attribution() {
    let server = FakeServer::start([listing(&["openai/gpt-5"])]);
    let fetched = catalog(&server)
        .load(&signed_in("team_1"), &CancellationToken::new())
        .await;
    assert_eq!(ids(&fetched), ["openai/gpt-5"]);
    let CatalogFetch::Loaded {
        access,
        anonymous_fallback,
        fallback_failure,
        ..
    } = fetched
    else {
        unreachable!();
    };
    assert_eq!(access.public_only, None);
    assert!(!anonymous_fallback);
    assert_eq!(fallback_failure, None);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, format!("{GATEWAY_MODELS_PATH}?teamId=team_1"));
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(request.header("x-vercel-ai-gateway-team"), None);
    assert_eq!(request.header("user-agent"), Some("oh-fx/test"));
    for header in ["http-referer", "x-title"] {
        assert_eq!(request.header(header), None);
    }
}

#[tokio::test]
async fn public_and_host_managed_requests_carry_no_credential() {
    for access in [
        CatalogAccess::PublicOnly(PublicOnlyReason::NoCredential),
        CatalogAccess::HostManaged,
        keyed(false),
    ] {
        let server = FakeServer::start([listing(&["openai/gpt-5"])]);
        let fetched = catalog(&server)
            .load(&access, &CancellationToken::new())
            .await;
        assert_eq!(ids(&fetched), ["openai/gpt-5"]);
        let request = &server.requests()[0];
        assert_eq!(request.header("x-vercel-ai-gateway-team"), None);
        if !matches!(access, CatalogAccess::Authenticated { .. }) {
            assert_eq!(request.header("authorization"), None);
        }
    }
}

#[tokio::test]
async fn a_rejected_key_falls_back_once_to_the_public_catalog() {
    for status in [401, 403] {
        let server = FakeServer::start([
            Reply::status(status, format!("key {KEY} rejected")),
            listing(&["openai/gpt-5"]),
        ]);
        let fetched = catalog(&server)
            .load(&signed_in("team_1"), &CancellationToken::new())
            .await;
        assert_eq!(ids(&fetched), ["openai/gpt-5"]);
        let CatalogFetch::Loaded {
            access,
            anonymous_fallback,
            fallback_failure,
            ..
        } = &fetched
        else {
            unreachable!();
        };
        assert_eq!(
            access.public_only,
            Some(PublicOnlyReason::CredentialRejected("fx_login"))
        );
        assert!(anonymous_fallback);
        assert_eq!(
            *fallback_failure,
            Some(CatalogFailed {
                failure: CatalogFailure::Authentication,
                http_status: Some(status),
                retryable: false,
            })
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].path,
            format!("{GATEWAY_MODELS_PATH}?teamId=team_1")
        );
        assert_eq!(requests[1].path, GATEWAY_MODELS_PATH);
        assert_eq!(requests[1].header("authorization"), None);
        assert!(!format!("{fetched:?}").contains(KEY));
    }
}

#[tokio::test]
async fn a_failed_public_fallback_reports_the_public_attempt() {
    let server = FakeServer::start([Reply::status(401, "{}"), Reply::status(503, "{}")]);
    let fetched = catalog(&server)
        .load(&keyed(false), &CancellationToken::new())
        .await;
    assert_eq!(
        fetched,
        CatalogFetch::Failed {
            access: AccessMetadata {
                source: Some("ai_gateway_api_key"),
                public_only: Some(PublicOnlyReason::CredentialRejected("ai_gateway_api_key")),
            },
            anonymous_fallback: true,
            failure: CatalogFailed {
                failure: CatalogFailure::GatewayUnavailable { retryable: true },
                http_status: Some(503),
                retryable: true,
            },
        }
    );
}

#[tokio::test]
async fn other_failures_and_public_rejections_never_retry() {
    for (access, reply, failure) in [
        (
            keyed(false),
            Reply::status(429, "{}"),
            CatalogFailed::for_http_status(429),
        ),
        (
            keyed(false),
            Reply::status(200, "{\"data\":1}"),
            CatalogFailed {
                failure: CatalogFailure::MalformedResponse,
                http_status: Some(200),
                retryable: false,
            },
        ),
        (
            CatalogAccess::PublicOnly(PublicOnlyReason::NoCredential),
            Reply::status(401, "{}"),
            CatalogFailed::for_http_status(401),
        ),
        (
            CatalogAccess::HostManaged,
            Reply::status(403, "{}"),
            CatalogFailed::for_http_status(403),
        ),
        (
            keyed(true),
            Reply::status(401, "{}"),
            CatalogFailed::for_http_status(401),
        ),
    ] {
        let server = FakeServer::start([reply, listing(&["openai/gpt-5"])]);
        let fetched = catalog(&server)
            .load(&access, &CancellationToken::new())
            .await;
        let CatalogFetch::Failed {
            anonymous_fallback,
            failure: failed,
            ..
        } = fetched
        else {
            panic!("{fetched:?}");
        };
        assert!(!anonymous_fallback);
        assert_eq!(failed, failure);
        assert_eq!(server.requests().len(), 1);
    }
}

#[tokio::test]
async fn a_cancelled_load_sends_nothing() {
    let server = FakeServer::start([]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let fetched = catalog(&server).load(&keyed(false), &cancel).await;
    let CatalogFetch::Failed { failure, .. } = fetched else {
        panic!("{fetched:?}");
    };
    assert_eq!(failure.failure, CatalogFailure::Cancellation);
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn a_refused_connection_is_a_transport_failure_that_is_not_retried() {
    let refused = RefusedPort::reserve();
    let catalog = GatewayModelCatalog::new(
        "oh-fx/test",
        GatewayModelsEndpoints {
            models: format!("{}/models", refused.base_url()),
        },
    )
    .unwrap();
    let fetched = catalog.load(&keyed(false), &CancellationToken::new()).await;
    let CatalogFetch::Failed { failure, .. } = fetched else {
        panic!("{fetched:?}");
    };
    assert_eq!(
        failure,
        CatalogFailed {
            failure: CatalogFailure::Transport,
            http_status: None,
            retryable: false,
        }
    );
}
