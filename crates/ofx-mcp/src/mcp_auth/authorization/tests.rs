use std::sync::Mutex as StdMutex;

use super::*;
use crate::mcp_auth::metadata::parse_authorization_metadata;
use crate::test_support::{FakeServer, RecordedRequest, Reply};

const TOKEN: &str = r#"{"access_token":"granted-access","refresh_token":"granted-refresh","expires_in":3600,"token_type":"Bearer"}"#;

fn http() -> reqwest::Client {
    ofx_http::build_connection_client(&ofx_http::ConnectionOptions {
        follow_redirects: false,
        ..ofx_http::ConnectionOptions::default()
    })
    .unwrap()
}

fn origin(server: &FakeServer) -> String {
    server.url.trim_end_matches("/mcp").to_owned()
}

#[derive(Clone)]
struct Authority {
    origin: String,
    metadata: String,
    token: String,
    register_status: u16,
    token_status: u16,
    resource_at_root: bool,
    metadata_at_openid: bool,
    resource_content_type: &'static str,
}

impl Authority {
    fn reply(&self, request: &RecordedRequest) -> Reply {
        let resource_path = if self.resource_at_root {
            "/.well-known/oauth-protected-resource"
        } else {
            "/.well-known/oauth-protected-resource/mcp"
        };
        let metadata_path = if self.metadata_at_openid {
            "/.well-known/openid-configuration"
        } else {
            "/.well-known/oauth-authorization-server"
        };
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", path) if path == resource_path => Reply::status(200)
                .body(&format!(
                    r#"{{"resource":"{}/mcp","authorization_servers":["{}"],"scopes_supported":["tools"]}}"#,
                    self.origin, self.origin
                ))
                .header("Content-Type", self.resource_content_type),
            ("GET", path) if path == metadata_path => Reply::json(&self.metadata),
            ("POST", "/register") => Reply::json(r#"{"client_id":"registered-client"}"#)
                .with_status(self.register_status),
            ("POST", "/token") => Reply::json(&self.token).with_status(self.token_status),
            _ => Reply::status(404),
        }
    }
}

fn metadata_for(origin: &str, extra: &str) -> String {
    format!(
        r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","registration_endpoint":"{origin}/register","revocation_endpoint":"{origin}/revoke","code_challenge_methods_supported":["S256"],"grant_types_supported":["authorization_code","refresh_token"],"token_endpoint_auth_methods_supported":["none","client_secret_basic"],"scopes_supported":["tools","offline_access"]{extra}}}"#
    )
}

async fn authority(extra: &str, token: &str) -> FakeServer {
    authority_with(|authority| {
        authority.metadata = metadata_for(&authority.origin, extra);
        authority.token = token.to_owned();
    })
    .await
}

async fn authority_with(adjust: impl FnOnce(&mut Authority)) -> FakeServer {
    let placeholder = Arc::new(StdMutex::new(None::<Authority>));
    let shared = Arc::clone(&placeholder);
    let server = FakeServer::start(move |request| {
        let authority = shared.lock().unwrap().clone().unwrap();
        authority.reply(request)
    })
    .await;
    let origin = origin(&server);
    let mut authority = Authority {
        metadata: metadata_for(&origin, ""),
        origin,
        token: TOKEN.to_owned(),
        register_status: 201,
        token_status: 200,
        resource_at_root: false,
        metadata_at_openid: false,
        resource_content_type: "application/json",
    };
    adjust(&mut authority);
    *placeholder.lock().unwrap() = Some(authority);
    server
}

async fn authorize(
    server: &FakeServer,
    config: &ClientConfig<'_>,
) -> Result<AuthorizationResult, McpError> {
    let (_, open) = browser(approving);
    authorize_interactive(
        &http(),
        &server.url,
        config,
        &Challenge::default(),
        None,
        &open,
        &CancellationToken::new(),
    )
    .await
}

fn query(url: &str, key: &str) -> String {
    let (_, query) = url.split_once('?').unwrap();
    query_value(query, key).unwrap().to_string()
}

fn browser(
    answer: impl Fn(&str) -> String + Send + Sync + 'static,
) -> (Arc<StdMutex<Vec<String>>>, impl Fn(&str) -> bool + Sync) {
    let opened = Arc::new(StdMutex::new(Vec::new()));
    let seen = Arc::clone(&opened);
    let answer = Arc::new(answer);
    let open = move |url: &str| {
        seen.lock().unwrap().push(url.to_owned());
        let callback = answer(url);
        tokio::spawn(async move {
            let _ = http().get(callback).send().await;
        });
        true
    };
    (opened, open)
}

fn approving(url: &str) -> String {
    format!(
        "{}?code=the%20code&state={}",
        query(url, "redirect_uri"),
        query(url, "state")
    )
}

#[tokio::test]
async fn an_interactive_authorization_registers_a_client_and_exchanges_the_code() {
    let server = authority("", TOKEN).await;
    let (opened, open) = browser(approving);
    let result = authorize_interactive(
        &http(),
        &server.url.replacen("http", "HTTP", 1),
        &ClientConfig::default(),
        &Challenge::default(),
        Some("tools.read"),
        &open,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let AuthorizationResult::Credentials(credentials) = result else {
        panic!("authorized");
    };
    let origin = origin(&server);
    assert_eq!(credentials.endpoint, format!("{origin}/mcp"));
    assert_eq!(credentials.resource, format!("{origin}/mcp"));
    assert_eq!(credentials.issuer, origin);
    assert_eq!(credentials.client_id, "registered-client");
    assert_eq!(credentials.client_secret, None);
    assert_eq!(credentials.access_token.as_str(), "granted-access");
    assert_eq!(
        credentials.refresh_token.as_deref().map(String::as_str),
        Some("granted-refresh")
    );
    assert_eq!(credentials.scope, "tools.read tools offline_access");
    assert_eq!(credentials.token_type, "Bearer");
    assert_eq!(credentials.token_endpoint_auth_method, "none");
    assert_eq!(
        credentials.authorization_endpoint,
        format!("{origin}/authorize")
    );
    assert_eq!(credentials.token_endpoint, format!("{origin}/token"));
    assert_eq!(
        credentials.revocation_endpoint,
        Some(format!("{origin}/revoke"))
    );
    assert!(credentials.expires_at_ms > now_ms());

    let url = opened.lock().unwrap()[0].clone();
    assert!(url.starts_with(&format!("{origin}/authorize?response_type=code&client_id=registered-client&redirect_uri=http%3A%2F%2F127.0.0.1%3A")));
    let redirect_uri = query(&url, "redirect_uri");
    assert!(redirect_uri.starts_with("http://127.0.0.1:"));
    assert!(redirect_uri.ends_with("/callback"));
    assert_eq!(query(&url, "resource"), format!("{origin}/mcp"));
    assert_eq!(query(&url, "state").len(), 43);
    assert_eq!(query(&url, "code_challenge_method"), "S256");
    assert_eq!(query(&url, "scope"), "tools.read tools offline_access");

    let requests = server.requests();
    let paths: Vec<_> = requests
        .iter()
        .map(|request| format!("{} {}", request.method, request.path))
        .collect();
    assert_eq!(
        paths,
        [
            "GET /.well-known/oauth-protected-resource/mcp",
            "GET /.well-known/oauth-authorization-server",
            "POST /register",
            "POST /token",
        ]
    );
    assert_eq!(
        requests[2].body,
        format!(
            r#"{{"client_name":"oh-fx","application_type":"native","redirect_uris":["{redirect_uri}"],"response_types":["code"],"grant_types":["authorization_code","refresh_token"],"token_endpoint_auth_method":"none"}}"#
        )
    );
    let token = &requests[3].body;
    let verifier = query(&format!("?{token}"), "code_verifier");
    assert_eq!(verifier.len(), 64);
    assert_eq!(query(&url, "code_challenge"), pkce_challenge(&verifier));
    let mut form = FormBody::default();
    for (key, value) in [
        ("grant_type", "authorization_code"),
        ("code", "the code"),
        ("redirect_uri", redirect_uri.as_str()),
        ("code_verifier", verifier.as_str()),
        ("resource", &format!("{origin}/mcp")),
        ("client_id", "registered-client"),
    ] {
        form.append(key, value);
    }
    assert_eq!(token, form.as_str());
}

#[tokio::test]
async fn a_configured_client_skips_registration_and_uses_its_secret() {
    let server = authority("", TOKEN).await;
    let (_, open) = browser(approving);
    let scopes = ["custom".to_owned()];
    let config = ClientConfig {
        client_id: Some("configured"),
        client_secret: Some("s3cret"),
        scopes: &scopes,
        ..ClientConfig::default()
    };
    let AuthorizationResult::Credentials(credentials) = authorize_interactive(
        &http(),
        &server.url,
        &config,
        &Challenge::default(),
        None,
        &open,
        &CancellationToken::new(),
    )
    .await
    .unwrap() else {
        panic!("authorized");
    };
    assert_eq!(credentials.client_id, "configured");
    assert_eq!(
        credentials.client_secret.as_deref().map(String::as_str),
        Some("s3cret")
    );
    assert_eq!(
        credentials.token_endpoint_auth_method,
        "client_secret_basic"
    );
    assert_eq!(credentials.scope, "custom offline_access");
    let requests = server.requests();
    assert!(requests.iter().all(|request| request.path != "/register"));
    let token = requests.last().unwrap();
    assert!(token.header("authorization").unwrap().starts_with("Basic "));
}

#[tokio::test]
async fn a_client_metadata_document_is_the_client_id_when_the_server_supports_it() {
    let server = authority(r#","client_id_metadata_document_supported":true"#, TOKEN).await;
    let (_, open) = browser(approving);
    let config = ClientConfig {
        client_metadata_url: Some("https://client.example/oh-fx.json"),
        ..ClientConfig::default()
    };
    let AuthorizationResult::Credentials(credentials) = authorize_interactive(
        &http(),
        &server.url,
        &config,
        &Challenge::default(),
        None,
        &open,
        &CancellationToken::new(),
    )
    .await
    .unwrap() else {
        panic!("authorized");
    };
    assert_eq!(credentials.client_id, "https://client.example/oh-fx.json");
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.path != "/register")
    );
}

#[tokio::test]
async fn the_callback_must_return_the_state_and_the_expected_issuer() {
    let server = authority(
        r#","authorization_response_iss_parameter_supported":true"#,
        TOKEN,
    )
    .await;
    let origin = origin(&server);
    let issued = |issuer: &str| {
        let issuer = issuer.to_owned();
        move |url: &str| format!("{}&iss={}", approving(url), ofx_auth_percent(&issuer))
    };
    let (_, open) = browser(issued(&format!("{origin}/")));
    assert_eq!(
        authorize_interactive(
            &http(),
            &server.url,
            &ClientConfig::default(),
            &Challenge::default(),
            None,
            &open,
            &CancellationToken::new()
        )
        .await,
        Ok(AuthorizationResult::IssuerMismatch(IssuerMismatch {
            source: IssuerMismatchSource::AuthorizationResponse,
            expected: origin.clone(),
            returned: format!("{origin}/"),
        }))
    );
    let (_, open) = browser(approving);
    assert_eq!(
        authorize_interactive(
            &http(),
            &server.url,
            &ClientConfig::default(),
            &Challenge::default(),
            None,
            &open,
            &CancellationToken::new()
        )
        .await,
        Err(McpError::AuthorizationResponseIssuerMissing)
    );
    let (_, open) =
        browser(|url: &str| format!("{}?code=c&state=forged", query(url, "redirect_uri")));
    assert_eq!(
        authorize_interactive(
            &http(),
            &server.url,
            &ClientConfig::default(),
            &Challenge::default(),
            None,
            &open,
            &CancellationToken::new()
        )
        .await,
        Err(McpError::OAuthStateMismatch)
    );
    let (_, open) =
        browser(|url: &str| format!("{}?error=access_denied", query(url, "redirect_uri")));
    assert_eq!(
        authorize_interactive(
            &http(),
            &server.url,
            &ClientConfig::default(),
            &Challenge::default(),
            None,
            &open,
            &CancellationToken::new()
        )
        .await,
        Err(McpError::MissingQueryParameter)
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.path != "/token")
    );
}

fn ofx_auth_percent(value: &str) -> String {
    let mut encoded = String::new();
    ofx_auth::percent_encode(&mut encoded, value);
    encoded
}

#[tokio::test]
async fn authorization_stops_before_the_browser_on_bad_metadata() {
    let server = authority("", TOKEN).await;
    let (opened, open) = browser(approving);
    let tenant = format!("{}/tenant", origin(&server));
    let config = ClientConfig {
        issuer: Some(&tenant),
        ..ClientConfig::default()
    };
    assert_eq!(
        authorize_interactive(
            &http(),
            &server.url,
            &config,
            &Challenge::default(),
            None,
            &open,
            &CancellationToken::new()
        )
        .await,
        Err(McpError::AuthorizationMetadataUnavailable)
    );
    assert!(opened.lock().unwrap().is_empty());
    let unreachable = FakeServer::start(|_| Reply::status(404)).await;
    assert_eq!(
        authorize_interactive(
            &http(),
            &unreachable.url,
            &ClientConfig::default(),
            &Challenge::default(),
            None,
            &open,
            &CancellationToken::new()
        )
        .await,
        Err(McpError::ProtectedResourceMetadataUnavailable)
    );
    let refused = |_: &str| false;
    assert_eq!(
        authorize_interactive(
            &http(),
            &server.url,
            &ClientConfig::default(),
            &Challenge::default(),
            None,
            &refused,
            &CancellationToken::new()
        )
        .await,
        Err(McpError::McpAuthorizationBrowserOpenFailed)
    );
}

#[tokio::test]
async fn a_cancelled_wait_for_the_browser_ends_promptly() {
    let server = authority("", TOKEN).await;
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let open = move |_: &str| {
        stop.cancel();
        true
    };
    assert_eq!(
        authorize_interactive(
            &http(),
            &server.url,
            &ClientConfig::default(),
            &Challenge::default(),
            None,
            &open,
            &cancel
        )
        .await,
        Err(McpError::Cancelled)
    );
}

#[test]
fn token_endpoint_methods_follow_upstreams_preference() {
    let parsed = |extra: &str| {
        let bytes = format!(
            r#"{{"issuer":"https://login.example.com","authorization_endpoint":"https://login.example.com/authorize","token_endpoint":"https://login.example.com/token"{extra}}}"#
        );
        match parse_authorization_metadata(bytes.as_bytes(), "https://login.example.com").unwrap() {
            MetadataOutcome::Metadata(metadata) => *metadata,
            MetadataOutcome::IssuerMismatch(_) => panic!("the issuer matches"),
        }
    };
    let basic_only = parsed("");
    assert_eq!(
        token_endpoint_auth_method(&basic_only, false),
        Ok("client_secret_basic")
    );
    let post_with_pkce = parsed(
        r#","token_endpoint_auth_methods_supported":["client_secret_post"],"code_challenge_methods_supported":["S256"]"#,
    );
    assert_eq!(
        token_endpoint_auth_method(&post_with_pkce, false),
        Ok("none")
    );
    assert_eq!(
        token_endpoint_auth_method(&post_with_pkce, true),
        Ok("client_secret_post")
    );
    let unsupported = parsed(r#","token_endpoint_auth_methods_supported":["private_key_jwt"]"#);
    assert_eq!(
        token_endpoint_auth_method(&unsupported, true),
        Err(McpError::UnsupportedTokenEndpointAuthenticationMethod)
    );
}

#[test]
fn scopes_union_the_previous_grant_without_duplicates() {
    assert_eq!(
        requested_scope(
            &[],
            Some("tools.call tools.admin"),
            &[],
            Some("tools.read tools.call"),
            true
        ),
        Ok(Some(
            "tools.read tools.call tools.admin offline_access".to_owned()
        ))
    );
    assert_eq!(
        requested_scope(
            &["configured".to_owned()],
            Some(""),
            &["meta".to_owned()],
            None,
            false
        ),
        Ok(None)
    );
    assert_eq!(
        requested_scope(
            &["configured".to_owned()],
            Some("challenged"),
            &[],
            None,
            false
        ),
        Ok(Some("challenged".to_owned()))
    );
    let configured = ["tools.call tools.admin".to_owned()];
    let metadata = ["meta".to_owned()];
    assert_eq!(
        requested_scope(
            &configured,
            None,
            &metadata,
            Some("tools.read tools.call"),
            true
        ),
        Ok(Some(
            "tools.read tools.call tools.admin offline_access".to_owned()
        ))
    );
    assert_eq!(
        requested_scope(&[], None, &metadata, None, false),
        Ok(Some("meta".to_owned()))
    );
    assert_eq!(requested_scope(&[], None, &[], None, false), Ok(None));
    assert_eq!(
        requested_scope(&["bad\"scope".to_owned()], None, &[], None, false),
        Err(McpError::InvalidOAuthScope)
    );
    let many: Vec<String> = (0..65).map(|index| format!("s{index}")).collect();
    assert_eq!(
        requested_scope(&many, None, &[], None, false),
        Err(McpError::TooManyOAuthScopes)
    );
}

#[test]
fn redirects_keep_the_exact_state_and_issuer() {
    let response = parse_authorization_redirect(
        "http://127.0.0.1:3000/callback?code=abc&state=state-1&iss=https%3A%2F%2Flogin.example.com",
    )
    .unwrap();
    assert_eq!(
        validate_authorization_response("state-1", "https://login.example.com", true, &response),
        Ok(None)
    );
    assert_eq!(
        validate_authorization_response("state-1", "https://login.example.com/", true, &response),
        Ok(Some("https://login.example.com".to_owned()))
    );
    assert_eq!(
        validate_authorization_response("state-2", "https://login.example.com", true, &response),
        Err(McpError::OAuthStateMismatch)
    );
    assert_eq!(
        parse_authorization_redirect("http://127.0.0.1:3000/callback"),
        Err(McpError::InvalidAuthorizationRedirect)
    );
    assert_eq!(
        parse_authorization_redirect("/callback?code=%zz&state=s"),
        Err(McpError::InvalidPercentEncoding)
    );
}

#[tokio::test]
async fn discovery_falls_back_to_the_root_and_openid_documents() {
    let server = authority_with(|authority| {
        authority.resource_at_root = true;
        authority.metadata_at_openid = true;
    })
    .await;
    assert!(matches!(
        authorize(&server, &ClientConfig::default()).await,
        Ok(AuthorizationResult::Credentials(_))
    ));
    let paths: Vec<_> = server
        .requests()
        .iter()
        .map(|request| request.path.clone())
        .collect();
    assert_eq!(
        paths[..4],
        [
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-authorization-server",
            "/.well-known/openid-configuration",
        ]
    );
    let wrong_type =
        authority_with(|authority| authority.resource_content_type = "text/html").await;
    assert_eq!(
        authorize(&wrong_type, &ClientConfig::default()).await,
        Err(McpError::InvalidOAuthResponseContentType)
    );
}

#[tokio::test]
async fn each_authorization_step_fails_with_upstreams_error() {
    let without_pkce = authority_with(|authority| {
        authority.metadata = authority
            .metadata
            .replace(r#""code_challenge_methods_supported":["S256"],"#, "");
    })
    .await;
    assert_eq!(
        authorize(&without_pkce, &ClientConfig::default()).await,
        Err(McpError::PkceS256NotSupported)
    );
    let without_registration = authority_with(|authority| {
        authority.metadata = authority.metadata.replace(
            &format!(
                r#""registration_endpoint":"{}/register","#,
                authority.origin
            ),
            "",
        );
    })
    .await;
    assert_eq!(
        authorize(&without_registration, &ClientConfig::default()).await,
        Err(McpError::ClientRegistrationUnavailable)
    );
    let refused_registration = authority_with(|authority| authority.register_status = 400).await;
    assert_eq!(
        authorize(&refused_registration, &ClientConfig::default()).await,
        Err(McpError::ClientRegistrationFailed)
    );
    let refused_exchange = authority_with(|authority| authority.token_status = 400).await;
    assert_eq!(
        authorize(&refused_exchange, &ClientConfig::default()).await,
        Err(McpError::TokenExchangeFailed)
    );
    let mac_token = authority_with(|authority| {
        authority.token = r#"{"access_token":"a","token_type":"mac"}"#.to_owned();
    })
    .await;
    assert_eq!(
        authorize(&mac_token, &ClientConfig::default()).await,
        Err(McpError::InvalidTokenResponse)
    );
}

#[tokio::test]
async fn a_configured_callback_port_redirects_to_localhost_and_must_be_free() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let server = authority("", TOKEN).await;
    let (opened, open) = browser(|url: &str| approving(url).replace("localhost", "127.0.0.1"));
    let config = ClientConfig {
        callback_port: Some(port),
        ..ClientConfig::default()
    };
    assert!(matches!(
        authorize_interactive(
            &http(),
            &server.url,
            &config,
            &Challenge::default(),
            None,
            &open,
            &CancellationToken::new()
        )
        .await,
        Ok(AuthorizationResult::Credentials(_))
    ));
    assert_eq!(
        query(&opened.lock().unwrap()[0], "redirect_uri"),
        format!("http://localhost:{port}/callback")
    );
    let held = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    assert_eq!(
        authorize(&server, &config).await,
        Err(McpError::McpCallbackPortUnavailable)
    );
    drop(held);
}

#[tokio::test]
async fn a_challenged_resource_metadata_url_is_fetched_once_without_fallback() {
    let server = authority("", TOKEN).await;
    let origin = origin(&server);
    let challenge = Challenge {
        resource_metadata: Some(format!("{origin}/.well-known/oauth-protected-resource/mcp")),
        scope: Some("tools.call".to_owned()),
        insufficient_scope: false,
    };
    let (opened, open) = browser(approving);
    assert!(matches!(
        authorize_interactive(
            &http(),
            &server.url,
            &ClientConfig::default(),
            &challenge,
            None,
            &open,
            &CancellationToken::new()
        )
        .await,
        Ok(AuthorizationResult::Credentials(_))
    ));
    assert_eq!(
        query(&opened.lock().unwrap()[0], "scope"),
        "tools.call offline_access"
    );
    let missing = Challenge {
        resource_metadata: Some(format!("{origin}/elsewhere")),
        ..Challenge::default()
    };
    let before = server.requests().len();
    assert_eq!(
        authorize_interactive(
            &http(),
            &server.url,
            &ClientConfig::default(),
            &missing,
            None,
            &open,
            &CancellationToken::new()
        )
        .await,
        Err(McpError::ProtectedResourceMetadataUnavailable)
    );
    let after: Vec<_> = server.requests()[before..]
        .iter()
        .map(|request| request.path.clone())
        .collect();
    assert_eq!(after, ["/elsewhere"]);
    let insecure = Challenge {
        resource_metadata: Some("http://auth.example/prm".to_owned()),
        ..Challenge::default()
    };
    assert_eq!(
        authorize_interactive(
            &http(),
            &server.url,
            &ClientConfig::default(),
            &insecure,
            None,
            &open,
            &CancellationToken::new()
        )
        .await,
        Err(McpError::InsecureMcpAuthEndpoint)
    );
}
