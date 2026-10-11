use zeroize::Zeroizing;

use super::*;

fn key(source: GatewaySource, team: Option<&str>, refresh_after_ms: Option<i64>) -> GatewayKey {
    GatewayKey {
        source,
        secret: Zeroizing::new("secret-token".to_owned()),
        team_id: team.map(str::to_owned),
        team_slug: None,
        refresh_after_ms,
    }
}

fn public_reason(access: &CatalogAccess) -> Option<PublicOnlyReason> {
    match access {
        CatalogAccess::PublicOnly(reason) => Some(*reason),
        CatalogAccess::Authenticated { .. } | CatalogAccess::HostManaged => None,
    }
}

#[test]
fn a_missing_credential_lists_the_public_catalog() {
    assert_eq!(
        public_reason(&gateway_catalog_access(None, true, 0)),
        Some(PublicOnlyReason::NoCredential)
    );
}

#[test]
fn a_login_needs_a_fresh_token_and_a_valid_team() {
    let due = key(GatewaySource::FxLogin, Some("team_1"), Some(100));
    assert_eq!(
        public_reason(&gateway_catalog_access(Some(&due), false, 100)),
        Some(PublicOnlyReason::LoginRefreshRequired)
    );
    for team in [None, Some("team one")] {
        let teamless = key(GatewaySource::FxLogin, team, Some(i64::MAX));
        assert_eq!(
            public_reason(&gateway_catalog_access(Some(&teamless), false, 0)),
            Some(PublicOnlyReason::LoginTeamRequired),
            "{team:?}"
        );
    }
    let ready = key(GatewaySource::FxLogin, Some("team_1"), Some(i64::MAX));
    let CatalogAccess::Authenticated {
        source,
        credential,
        login_team,
        explicit,
    } = gateway_catalog_access(Some(&ready), true, 0)
    else {
        panic!("a signed-in team is authenticated");
    };
    assert_eq!(source, "fx_login");
    assert_eq!(credential.as_str(), "secret-token");
    assert_eq!(login_team.as_deref(), Some("team_1"));
    assert!(explicit);
}

#[test]
fn keys_authenticate_without_a_team_whatever_their_expiry() {
    for source in [
        GatewaySource::VercelOidcToken,
        GatewaySource::AiGatewayApiKey,
        GatewaySource::StoredKey,
    ] {
        let keyed = key(source, Some("team_1"), Some(0));
        let CatalogAccess::Authenticated {
            source: named,
            login_team,
            explicit,
            ..
        } = gateway_catalog_access(Some(&keyed), false, 10)
        else {
            panic!("{source:?} is authenticated");
        };
        assert_eq!(named, source.name());
        assert_eq!(login_team, None);
        assert!(!explicit);
    }
}

use ofx_contract::{BoxFuture, ImageInputSupport, ModelCapabilities};
use ofx_gateway::{GatewayAccess, GatewayCredential, GatewayEndpoints, GatewayRefresh};
use ofx_testkit::{FakeServer, RefusedPort, Reply};

const LISTING: &str = r#"{"data":[
    {"id":"vendor/plain-1","type":"language","released":1,"tags":["tool-use"]},
    {"id":"zai/glm-5.2","type":"language","released":2,"tags":["tool-use","vision","file-input"],"context_window":200000,"max_tokens":32000,
     "reasoning_options":[{"type":"effort","values":["low","high"]}],"fast_options":[{"type":"toggle"}]}
]}"#;

fn catalog_at(url: String, credential: CatalogCredential) -> GatewayCatalog {
    GatewayCatalog::new(
        "oh-fx/test",
        GatewayModelsEndpoints { models: url },
        credential,
    )
    .unwrap()
}

fn served(server: &FakeServer, credential: CatalogCredential) -> GatewayCatalog {
    catalog_at(
        format!("http://{}/coding-agent/v1/models", server.address()),
        credential,
    )
}

fn route(
    credential: GatewayCredential,
    source: GatewaySource,
    explicit: bool,
) -> CatalogCredential {
    CatalogCredential::Route {
        provider: Arc::new(
            GatewayProvider::new(credential, "oh-fx/test", GatewayEndpoints::default()).unwrap(),
        ),
        source,
        explicit,
    }
}

fn api_key() -> CatalogCredential {
    route(
        GatewayCredential::new(Some("secret-token".to_owned()), None),
        GatewaySource::AiGatewayApiKey,
        false,
    )
}

struct Renewed;

impl GatewayRefresh for Renewed {
    fn refresh<'a>(
        &'a self,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<GatewayAccess>, String>> {
        Box::pin(async {
            Ok(Some(GatewayAccess::new(
                "renewed-token".to_owned(),
                Some("team_1".to_owned()),
                i64::MAX,
            )))
        })
    }
}

#[tokio::test]
async fn a_due_login_is_refreshed_before_the_catalog_is_fetched_with_its_team() {
    let server = FakeServer::start([Reply::status(200, LISTING)]);
    let login = GatewayCredential::new(Some("stale-token".to_owned()), Some("team_1".to_owned()))
        .refreshed_by(0, Arc::new(Renewed));
    let catalog = served(&server, route(login, GatewaySource::FxLogin, true));
    assert!(matches!(
        catalog.catalog().await,
        ModelCatalog::Listed {
            source: ModelCatalogSource::Gateway {
                note: "Gateway catalog: authenticated with oh-fx login."
            },
            ..
        }
    ));
    let request = &server.requests()[0];
    assert_eq!(request.path, "/coding-agent/v1/models?teamId=team_1");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer renewed-token")
    );
}

#[tokio::test]
async fn a_host_managed_catalog_sends_no_credential() {
    let server = FakeServer::start([Reply::status(200, LISTING)]);
    let catalog = served(&server, CatalogCredential::HostManaged);
    let listed = catalog.catalog().await;
    assert!(matches!(
        listed,
        ModelCatalog::Listed {
            source: ModelCatalogSource::Gateway {
                note: "Provider catalog: authentication is managed by the host."
            },
            ..
        }
    ));
    assert_eq!(server.requests()[0].header("authorization"), None);
}

#[tokio::test]
async fn concurrent_lookups_share_one_catalog_request() {
    let server = FakeServer::start([Reply::status(200, LISTING), Reply::status(200, LISTING)]);
    let catalog = served(&server, api_key());
    let cancel = CancellationToken::new();
    let (first, second) = tokio::join!(catalog.resolve("zai/glm-5.2", &cancel), catalog.catalog());
    assert!(matches!(first, CapabilityLookup::Resolved(_)));
    assert!(matches!(second, ModelCatalog::Listed { .. }));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn the_shell_lists_the_picker_view_once_with_capabilities_and_its_note() {
    let server = FakeServer::start([Reply::status(200, LISTING)]);
    let catalog = served(&server, api_key());
    assert_eq!(catalog.cached(), None);
    let listed = catalog.catalog().await;
    let ModelCatalog::Listed { models, source } = &listed else {
        panic!("{listed:?}");
    };
    assert_eq!(models[0].id, "zai/glm-5.2");
    assert_eq!(
        models[0].capabilities,
        ModelCapabilities {
            reasoning_efforts: vec!["low".to_owned(), "high".to_owned()],
            supports_fast_mode: true,
            context_window: Some(200_000),
            image_input_support: ImageInputSupport::Native,
        }
    );
    assert_eq!(models[0].max_output_tokens, Some(32_000));
    assert_eq!(models[1].id, "vendor/plain-1");
    assert_eq!(
        *source,
        ModelCatalogSource::Gateway {
            note: "Note: Gateway catalog is authenticated with an API key"
        }
    );
    assert_eq!(catalog.cached(), Some(listed.clone()));
    assert_eq!(catalog.catalog().await, listed);
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn capabilities_resolve_from_the_catalog_and_default_for_an_unknown_model() {
    let server = FakeServer::start([Reply::status(200, LISTING)]);
    let catalog = served(&server, api_key());
    let cancel = CancellationToken::new();
    let CapabilityLookup::Resolved(known) = catalog.resolve("zai/glm-5.2", &cancel).await else {
        panic!("the listed model resolves");
    };
    assert_eq!(known.reasoning_efforts, ["low", "high"]);
    assert!(known.supports_fast_mode);
    assert_eq!(
        catalog.resolve("vendor/missing-1", &cancel).await,
        CapabilityLookup::Resolved(ModelCapabilities::default())
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_failed_catalog_is_unavailable_and_retried_on_the_next_lookup() {
    let server = FakeServer::start([Reply::status(503, "{}"), Reply::status(200, LISTING)]);
    let catalog = served(&server, api_key());
    let cancel = CancellationToken::new();
    assert_eq!(
        catalog.resolve("zai/glm-5.2", &cancel).await,
        CapabilityLookup::CatalogUnavailable
    );
    assert!(matches!(
        catalog.resolve("zai/glm-5.2", &cancel).await,
        CapabilityLookup::Resolved(_)
    ));
    let stopped = CancellationToken::new();
    stopped.cancel();
    let fresh = served(&server, api_key());
    assert_eq!(
        fresh.resolve("zai/glm-5.2", &stopped).await,
        CapabilityLookup::Cancelled
    );
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn only_rate_limits_and_retryable_outages_offer_a_retry() {
    for (reply, retry) in [
        (Reply::status(429, "{}"), Some(CatalogRetry::RateLimited)),
        (Reply::status(503, "{}"), Some(CatalogRetry::Unreachable)),
        (Reply::status(501, "{}"), None),
        (Reply::status(404, "{}"), None),
        (Reply::status(401, "{}"), None),
    ] {
        let server = FakeServer::start([reply.clone(), reply]);
        let catalog = served(&server, api_key());
        assert_eq!(catalog.catalog().await, ModelCatalog::Failed { retry });
    }
    let refused = RefusedPort::reserve();
    let catalog = catalog_at(format!("{}/models", refused.base_url()), api_key());
    assert_eq!(
        catalog.catalog().await,
        ModelCatalog::Failed { retry: None }
    );
}

#[test]
fn the_menu_note_names_the_access_the_catalog_was_loaded_with() {
    let authenticated = |source| AccessMetadata {
        source: Some(source),
        public_only: None,
    };
    let public = |reason| AccessMetadata {
        source: None,
        public_only: Some(reason),
    };
    for (access, expected) in [
        (
            public(PublicOnlyReason::NoCredential),
            "Using the public model catalog; sign in or use an API key for team-private models.",
        ),
        (
            public(PublicOnlyReason::LoginTeamRequired),
            "Choose a Vercel team to load its private models.",
        ),
        (
            public(PublicOnlyReason::LoginRefreshRequired),
            "Vercel sign-in must refresh before team-private models can load.",
        ),
        (
            public(PublicOnlyReason::CredentialRejected("stored_key")),
            "Your Gateway credential was rejected; using the public model catalog.",
        ),
        (
            authenticated("fx_login"),
            "Gateway catalog: authenticated with oh-fx login.",
        ),
        (
            authenticated("vercel_oidc_token"),
            "Gateway catalog: authenticated with the Vercel session.",
        ),
        (
            authenticated("stored_key"),
            "Gateway catalog: authenticated with the stored API key.",
        ),
        (
            authenticated("host_managed"),
            "Provider catalog: authentication is managed by the host.",
        ),
    ] {
        assert_eq!(note(access), expected);
    }
}
