use ofx_testkit::{FakeServer, RefusedPort, Reply};
use zeroize::Zeroizing;

use super::*;
use crate::model_catalog::PublicOnlyReason;

const KEY: &str = "vck_live_0123456789abcdef";

fn parsed(body: &str) -> Vec<GatewayModel> {
    parse_catalog(body.as_bytes(), CatalogView::Full).unwrap()
}

fn only(body: &str) -> GatewayModel {
    let mut models = parsed(&format!(r#"{{"data":[{body}]}}"#));
    assert_eq!(models.len(), 1, "{body}");
    models.remove(0)
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
        assert_eq!(
            parse_catalog(body.as_bytes(), CatalogView::Full),
            None,
            "{body}"
        );
    }
    assert_eq!(parsed(r#"{"data":[]}"#), Vec::new());
}

#[tokio::test]
async fn a_signed_in_catalog_request_sends_the_token_and_team_query_without_attribution() {
    let server = FakeServer::start([listing(&["openai/gpt-5"])]);
    let fetched = catalog(&server)
        .load(
            &signed_in("team_1"),
            CatalogView::Full,
            &CancellationToken::new(),
        )
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
            .load(&access, CatalogView::Full, &CancellationToken::new())
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
            .load(
                &signed_in("team_1"),
                CatalogView::Full,
                &CancellationToken::new(),
            )
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
        .load(&keyed(false), CatalogView::Full, &CancellationToken::new())
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
            .load(&access, CatalogView::Full, &CancellationToken::new())
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
    let fetched = catalog(&server)
        .load(&keyed(false), CatalogView::Full, &cancel)
        .await;
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
    let fetched = catalog
        .load(&keyed(false), CatalogView::Full, &CancellationToken::new())
        .await;
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

#[test]
fn catalog_capabilities_follow_the_entry_metadata() {
    let model = only(
        r#"{"id":"vendor/model","tags":["vision","file-input"],"context_window":256000,"max_tokens":32000,
            "reasoning_options":[{"type":"toggle"},{"type":"effort","values":["auto","low","Adaptive","high","bad effort",7,"default","xhigh"]},{"type":"effort","values":["ignored"]}]}"#,
    );
    let capabilities = model.capabilities();
    assert_eq!(capabilities.reasoning_efforts, ["low", "high", "xhigh"]);
    assert_eq!(capabilities.context_window, Some(256_000));
    assert_eq!(capabilities.image_input_support, ImageInputSupport::Native);
    assert!(!capabilities.supports_fast_mode);
    assert_eq!(model.max_output_tokens(), Some(32_000));
    let vision_only = only(r#"{"id":"vendor/model","tags":["vision"]}"#);
    assert_eq!(
        vision_only.capabilities().image_input_support,
        ImageInputSupport::NonNative
    );
    for limits in [
        r#""context_window":0,"max_tokens":-1"#,
        r#""context_window":4294967296,"max_tokens":1.5"#,
        r#""context_window":"256000","max_tokens":null"#,
    ] {
        let model = only(&format!(r#"{{"id":"vendor/model",{limits}}}"#));
        assert_eq!(model.capabilities().context_window, None, "{limits}");
        assert_eq!(model.max_output_tokens(), None, "{limits}");
    }
}

#[test]
fn an_effort_option_lists_at_most_sixteen_efforts() {
    let values: Vec<String> = (0..20).map(|index| format!(r#""e{index}""#)).collect();
    let model = only(&format!(
        r#"{{"id":"vendor/model","reasoning_options":[{{"type":"effort","values":[{}]}}]}}"#,
        values.join(",")
    ));
    let efforts = model.capabilities().reasoning_efforts;
    assert_eq!(efforts.len(), 16);
    assert_eq!(efforts[15], "e15");
}

#[test]
fn fast_mode_comes_from_a_toggle_a_fast_price_or_an_openai_priority_tier() {
    for (entry, fast) in [
        (r#""fast_options":[{"type":"toggle"}]"#, true),
        (r#""fast_options":[{"type":"effort"}]"#, false),
        (r#""pricing":{"fast":{"input":"1"}}"#, true),
        (r#""pricing":{"fast":"1"}"#, false),
        (
            r#""owned_by":"OpenAI","pricing":{"service_tiers":{"priority":{"input":"1"}}}"#,
            true,
        ),
        (
            r#""owned_by":"vendor","pricing":{"service_tiers":{"priority":{"input":"1"}}}"#,
            false,
        ),
        (r#""tags":["fast"]"#, false),
    ] {
        let model = only(&format!(r#"{{"id":"vendor/model",{entry}}}"#));
        assert_eq!(model.capabilities().supports_fast_mode, fast, "{entry}");
    }
}

#[test]
fn ultra_mode_needs_openai_priced_tier_metadata_rather_than_names_or_tags() {
    let listing = r#"{"data":[
        {"id":"openai/gpt-6-astra","type":"language","owned_by":"openai","pricing":{"input_cache_read":"0.000001","service_tiers":{"ultrafast":{"input":"0.00006","output":"0.0003","input_cache_read":"0.000006"}}}},
        {"id":"openai/gpt-5.6-sol","type":"language","owned_by":"openai","tags":["ultrafast"],"pricing":{"service_tiers":{"priority":{"input":"0.1","output":"0.2"}}}},
        {"id":"openai/incomplete","type":"language","owned_by":"openai","pricing":{"service_tiers":{"ultrafast":{"input":"0.1"}}}},
        {"id":"provider/model","type":"language","owned_by":"provider","pricing":{"service_tiers":{"ultrafast":{"input":"0.1","output":"0.2"}}}}
    ]}"#;
    let models = parsed(listing);
    assert_eq!(models.len(), 4);
    for model in models {
        assert_eq!(
            model.capabilities().supports_ultrafast_mode,
            model.id == "openai/gpt-6-astra",
            "{}",
            model.id
        );
    }
    for (pricing, ultra) in [
        (
            r#"{"service_tiers":{"ultrafast":{"input":1,"output":2.5}}}"#,
            true,
        ),
        (
            r#"{"service_tiers":{"ultrafast":{"input":"0","output":"1"}}}"#,
            false,
        ),
        (
            r#"{"service_tiers":{"ultrafast":{"input":"-1","output":"1"}}}"#,
            false,
        ),
        (
            r#"{"service_tiers":{"ultrafast":{"input":"inf","output":"1"}}}"#,
            false,
        ),
        (
            r#"{"service_tiers":{"ultrafast":{"input":true,"output":"1"}}}"#,
            false,
        ),
        (
            r#"{"input_cache_read":"1","service_tiers":{"ultrafast":{"input":"1","output":"1"}}}"#,
            false,
        ),
        (r#"{"service_tiers":{"ultrafast":"1"}}"#, false),
    ] {
        let model = only(&format!(
            r#"{{"id":"openai/model","owned_by":"OpenAI","pricing":{pricing}}}"#
        ));
        assert_eq!(
            model.capabilities().supports_ultrafast_mode,
            ultra,
            "{pricing}"
        );
    }
}

#[test]
fn the_picker_view_reorders_the_parsed_catalog() {
    let body = r#"{"data":[
        {"id":"vendor/b-1","released":1,"tags":["tool-use"]},
        {"id":"zai/glm-5.2","released":2,"tags":[]}
    ]}"#;
    let full = parse_catalog(body.as_bytes(), CatalogView::Full).unwrap();
    let picker = parse_catalog(body.as_bytes(), CatalogView::Picker).unwrap();
    assert_eq!(full[0].id, "vendor/b-1");
    assert_eq!(picker[0].id, "zai/glm-5.2");
}
