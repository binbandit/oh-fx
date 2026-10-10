use super::*;
use crate::mcp_contract::{HttpHeaderEnv, TransportType};
use crate::streamable_http::HeaderError;

fn config() -> McpServerConfig {
    McpServerConfig {
        headers: vec![HttpHeader {
            name: "X-Workspace".to_owned(),
            value: "one".to_owned(),
        }],
        header_env: vec![HttpHeaderEnv {
            name: "X-Org".to_owned(),
            env: "ORG_ENV".to_owned(),
        }],
        bearer_token_env: Some("MCP_TOKEN".to_owned()),
        ..McpServerConfig::remote("api", TransportType::Http, "https://api.example.com/mcp")
    }
}

#[test]
fn resolved_headers_append_environment_values_and_the_bearer_token() {
    let lookup = |name: &str| match name {
        "ORG_ENV" => Some("acme".to_owned()),
        "MCP_TOKEN" => Some("secret".to_owned()),
        _ => None,
    };
    let headers = resolve_headers(&config(), &lookup, None).unwrap();
    let pairs: Vec<_> = headers
        .iter()
        .map(|header| (header.name.as_str(), header.value.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("X-Workspace", "one"),
            ("X-Org", "acme"),
            ("Authorization", "Bearer secret")
        ]
    );
}

#[test]
fn missing_environment_values_fail_before_any_request() {
    let only_org = |name: &str| (name == "ORG_ENV").then(|| "acme".to_owned());
    assert_eq!(
        resolve_headers(&config(), &|_| None, None),
        Err(McpError::McpHeaderEnvironmentMissing)
    );
    assert_eq!(
        resolve_headers(&config(), &only_org, None),
        Err(McpError::McpBearerEnvironmentMissing)
    );
    let injected = |name: &str| match name {
        "ORG_ENV" => Some("a\r\nb".to_owned()),
        _ => Some("token".to_owned()),
    };
    assert_eq!(
        resolve_headers(&config(), &injected, None),
        Err(McpError::Header(HeaderError::InvalidHeaderValue))
    );
}

mod stored {
    use std::path::PathBuf;

    use reqwest::RequestBuilder;
    use reqwest::header::AUTHORIZATION;
    use zeroize::Zeroizing;

    use super::*;
    use crate::mcp_auth_store::CredentialStore;
    use crate::server_connection::McpClient;
    use crate::server_transport::{ConnectOptions, startup_failure_message};
    use crate::test_support::{FakeServer, RecordedRequest, Reply};
    use crate::transport::ShutdownMode;

    const RENEWED: &str = r#"{"access_token":"renewed-token","refresh_token":"renewed-refresh","expires_in":3600,"token_type":"Bearer"}"#;

    fn reply(request: &RecordedRequest, token: &'static str) -> Reply {
        if request.path == "/token" {
            return Reply::json(token);
        }
        match request.method_name().as_deref() {
            Some("initialize") => Reply::json(&format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"protocolVersion":"2025-11-25","capabilities":{{}},"serverInfo":{{"name":"remote","version":"2"}}}}}}"#,
                request.request_id().unwrap()
            )),
            Some("tools/list") => Reply::json(&format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"tools":[]}}}}"#,
                request.request_id().unwrap()
            )),
            _ => Reply::status(202),
        }
    }

    struct Fixture {
        server: FakeServer,
        data: tempfile::TempDir,
    }

    impl Fixture {
        async fn start(token: &'static str) -> Self {
            Self {
                server: FakeServer::start(move |request| reply(request, token)).await,
                data: tempfile::tempdir().unwrap(),
            }
        }

        fn data(&self) -> PathBuf {
            std::fs::canonicalize(self.data.path())
                .unwrap()
                .join("oh-fx")
        }

        fn store(&self) -> CredentialStore {
            CredentialStore::new(&self.data())
        }

        fn lookup(&self) -> GrantLookup {
            GrantLookup::for_server(&self.config()).unwrap()
        }

        fn config(&self) -> McpServerConfig {
            McpServerConfig {
                bearer_token_env: Some("OH_FX_TEST_UNSET_BEARER".to_owned()),
                allow_stored_credentials: true,
                startup_timeout_ms: 5_000,
                ..McpServerConfig::remote("remote", TransportType::Http, &self.server.url)
            }
        }

        fn credentials(&self, expires_at_ms: i64, refresh_token: Option<&str>) -> Credentials {
            Credentials {
                endpoint: self.server.url.clone(),
                resource: self.server.url.clone(),
                issuer: "https://issuer.example".to_owned(),
                client_id: "client".to_owned(),
                client_secret: None,
                access_token: Zeroizing::new("stored-token".to_owned()),
                refresh_token: refresh_token.map(|value| Zeroizing::new(value.to_owned())),
                scope: "tools".to_owned(),
                token_type: "Bearer".to_owned(),
                token_endpoint_auth_method: "none".to_owned(),
                expires_at_ms,
                authorization_endpoint: "https://issuer.example/authorize".to_owned(),
                token_endpoint: self.server.url.replace("/mcp", "/token"),
                revocation_endpoint: None,
            }
        }

        fn options(&self) -> ConnectOptions {
            ConnectOptions {
                profile_data: Some(self.data()),
                ..ConnectOptions::default()
            }
        }

        fn bearers(&self) -> Vec<Option<String>> {
            self.server
                .requests()
                .iter()
                .filter(|request| request.path == "/mcp")
                .map(|request| request.header("authorization").map(str::to_owned))
                .collect()
        }
    }

    #[tokio::test]
    async fn a_stored_bearer_authorizes_every_request_in_place_of_the_environment_token() {
        let fixture = Fixture::start(RENEWED).await;
        fixture
            .store()
            .save(&fixture.lookup(), &fixture.credentials(i64::MAX, None))
            .unwrap();
        let client = McpClient::connect(&fixture.config(), &fixture.options())
            .await
            .unwrap();
        client.shutdown(ShutdownMode::Immediate).await;
        let bearers = fixture.bearers();
        assert!(!bearers.is_empty());
        assert!(
            bearers
                .iter()
                .all(|bearer| bearer.as_deref() == Some("Bearer stored-token")),
            "{bearers:?}"
        );
    }

    #[tokio::test]
    async fn stored_credentials_need_the_server_to_allow_them_and_a_profile() {
        let fixture = Fixture::start(RENEWED).await;
        fixture
            .store()
            .save(&fixture.lookup(), &fixture.credentials(i64::MAX, None))
            .unwrap();
        let refused = McpServerConfig {
            allow_stored_credentials: false,
            ..fixture.config()
        };
        let failure = McpClient::connect(&refused, &fixture.options())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::McpBearerEnvironmentMissing);
        let failure = McpClient::connect(&fixture.config(), &ConnectOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::McpBearerEnvironmentMissing);
        assert!(fixture.server.requests().is_empty());
    }

    #[tokio::test]
    async fn an_expiring_grant_is_refreshed_saved_and_used_before_the_first_request() {
        let fixture = Fixture::start(RENEWED).await;
        fixture
            .store()
            .save(
                &fixture.lookup(),
                &fixture.credentials(1, Some("stored-refresh")),
            )
            .unwrap();
        let client = McpClient::connect(&fixture.config(), &fixture.options())
            .await
            .unwrap();
        client.shutdown(ShutdownMode::Immediate).await;
        let requests = fixture.server.requests();
        assert_eq!(requests[0].path, "/token");
        assert!(requests[0].body.contains("refresh_token=stored-refresh"));
        assert_eq!(
            fixture.bearers()[0].as_deref(),
            Some("Bearer renewed-token")
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.path == "/token")
                .count(),
            1
        );
        let saved = fixture.store().load(&fixture.lookup()).unwrap().unwrap();
        assert_eq!(saved.access_token.as_str(), "renewed-token");
        assert_eq!(
            saved.refresh_token.as_deref().map(String::as_str),
            Some("renewed-refresh")
        );
        assert!(saved.expires_at_ms > 1);
    }

    #[tokio::test]
    async fn an_expired_grant_without_a_refresh_token_fails_startup_with_upstreams_guidance() {
        let fixture = Fixture::start(RENEWED).await;
        fixture
            .store()
            .save(&fixture.lookup(), &fixture.credentials(1, None))
            .unwrap();
        let failure = McpClient::connect(&fixture.config(), &fixture.options())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::McpAuthenticationRequired);
        assert_eq!(
            startup_failure_message(&failure, 5_000, 5_000),
            "MCP credentials expired. Run /mcp auth remote --open."
        );
        assert!(fixture.server.requests().is_empty());
    }

    #[tokio::test]
    async fn a_rejected_refresh_fails_startup_with_upstreams_guidance() {
        let fixture = Fixture::start(r#"{"access_token":""}"#).await;
        fixture
            .store()
            .save(
                &fixture.lookup(),
                &fixture.credentials(1, Some("stored-refresh")),
            )
            .unwrap();
        let failure = McpClient::connect(&fixture.config(), &fixture.options())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::InvalidOAuthResponse);
        assert_eq!(
            startup_failure_message(&failure, 5_000, 5_000),
            "MCP credential refresh failed. Run /mcp auth remote --open."
        );
        let shown = format!(
            "{failure} {failure:?} {}",
            startup_failure_message(&failure, 5_000, 5_000)
        );
        for secret in ["stored-token", "stored-refresh"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert_eq!(fixture.bearers(), Vec::<Option<String>>::new());
    }

    #[tokio::test]
    async fn an_unreadable_store_fails_startup_with_its_error() {
        let fixture = Fixture::start(RENEWED).await;
        let directory = fixture.data().join("mcp-credentials");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("credentials.json"),
            r#"{"version":9,"credentials":[]}"#,
        )
        .unwrap();
        std::fs::set_permissions(
            directory.join("credentials.json"),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        let failure = McpClient::connect(&fixture.config(), &fixture.options())
            .await
            .err()
            .unwrap();
        assert_eq!(failure.error, McpError::InvalidMcpCredentialStore);
        assert_eq!(
            startup_failure_message(&failure, 5_000, 5_000),
            "Stored MCP credentials could not be read securely."
        );
        assert!(fixture.server.requests().is_empty());
    }

    async fn resolved(fixture: &Fixture) -> HttpAuth {
        HttpAuth::resolve(
            &fixture.config(),
            Some(fixture.store()),
            &Arc::default(),
            || Ok(oauth_client()),
            &|_| None,
        )
        .await
        .ok()
        .unwrap()
    }

    fn oauth_client() -> reqwest::Client {
        ofx_http::build_connection_client(&ConnectionOptions {
            follow_redirects: false,
            ..ConnectionOptions::default()
        })
        .unwrap()
    }

    fn token_requests(fixture: &Fixture) -> usize {
        fixture
            .server
            .requests()
            .iter()
            .filter(|request| request.path == "/token")
            .count()
    }

    fn authorization(builder: RequestBuilder) -> Option<String> {
        builder
            .build()
            .unwrap()
            .headers()
            .get(AUTHORIZATION)
            .map(|value| value.to_str().unwrap().to_owned())
    }

    #[tokio::test]
    async fn a_refresh_replaces_only_the_grant_it_refreshed() {
        let fixture = Fixture::start(RENEWED).await;
        fixture
            .store()
            .save(
                &fixture.lookup(),
                &fixture.credentials(1, Some("stored-refresh")),
            )
            .unwrap();
        let auth = resolved(&fixture).await;
        let other_issuer = GrantLookup::new(
            "remote",
            &fixture.server.url,
            None,
            Some("https://other-issuer.example"),
        )
        .unwrap();
        let mut newer = fixture.credentials(i64::MAX, None);
        newer.issuer = "https://other-issuer.example".to_owned();
        newer.access_token = Zeroizing::new("newer-token".to_owned());
        fixture.store().save(&other_issuer, &newer).unwrap();
        let builder = auth
            .apply(oauth_client().get(&fixture.server.url))
            .await
            .unwrap();
        assert_eq!(
            authorization(builder).as_deref(),
            Some("Bearer renewed-token")
        );
        let kept = fixture.store().load(&other_issuer).unwrap().unwrap();
        assert_eq!(kept.access_token.as_str(), "newer-token");
        let refreshed = fixture
            .store()
            .load(
                &GrantLookup::new(
                    "remote",
                    &fixture.server.url,
                    None,
                    Some("https://issuer.example"),
                )
                .unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.access_token.as_str(), "renewed-token");
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_refresh() {
        let fixture = Fixture::start(RENEWED).await;
        fixture
            .store()
            .save(
                &fixture.lookup(),
                &fixture.credentials(1, Some("stored-refresh")),
            )
            .unwrap();
        let auth = resolved(&fixture).await;
        let http = oauth_client();
        let (first, second) = tokio::join!(
            auth.apply(http.get(&fixture.server.url)),
            auth.apply(http.get(&fixture.server.url))
        );
        assert_eq!(token_requests(&fixture), 1);
        for builder in [first.unwrap(), second.unwrap()] {
            assert_eq!(
                authorization(builder).as_deref(),
                Some("Bearer renewed-token")
            );
        }
    }

    #[tokio::test]
    async fn closing_a_session_uses_the_current_bearer_without_a_refresh() {
        let fixture = Fixture::start(RENEWED).await;
        fixture
            .store()
            .save(
                &fixture.lookup(),
                &fixture.credentials(1, Some("stored-refresh")),
            )
            .unwrap();
        let auth = resolved(&fixture).await;
        let closing = auth.apply_current(oauth_client().delete(&fixture.server.url));
        assert_eq!(
            authorization(closing).as_deref(),
            Some("Bearer stored-token")
        );
        assert_eq!(token_requests(&fixture), 0);
    }

    #[tokio::test]
    async fn closing_during_a_blocked_refresh_still_sends_the_last_bearer() {
        let server = FakeServer::start(|request| {
            if request.path == "/token" {
                return Reply::sse(&[]).held_open();
            }
            if request.method == "DELETE" {
                return Reply::status(204);
            }
            match request.method_name().as_deref() {
                Some("initialize") => Reply::json(&format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":{{"protocolVersion":"2025-11-25","capabilities":{{}},"serverInfo":{{"name":"remote","version":"2"}}}}}}"#,
                    request.request_id().unwrap()
                ))
                .header("Mcp-Session-Id", "session-1"),
                Some("tools/list") => Reply::json(&format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":{{"tools":[]}}}}"#,
                    request.request_id().unwrap()
                )),
                _ => Reply::status(202),
            }
        })
        .await;
        let fixture = Fixture {
            server,
            data: tempfile::tempdir().unwrap(),
        };
        let expires_at_ms = now_ms() + 61_000;
        fixture
            .store()
            .save(
                &fixture.lookup(),
                &fixture.credentials(expires_at_ms, Some("stored-refresh")),
            )
            .unwrap();
        let client = Arc::new(
            McpClient::connect(&fixture.config(), &fixture.options())
                .await
                .unwrap(),
        );
        tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
        let calling = Arc::clone(&client);
        let call = tokio::spawn(async move {
            calling
                .call_tool(
                    "echo",
                    "{}",
                    crate::tool_operations::CallOptions::default(),
                    tokio::time::Instant::now() + std::time::Duration::from_secs(30),
                )
                .await
        });
        assert!(
            fixture
                .server
                .wait_for(|request| request.path == "/token")
                .await
        );
        client.shutdown(ShutdownMode::Graceful).await;
        assert!(
            fixture
                .server
                .wait_for(|request| request.method == "DELETE"
                    && request.header("authorization") == Some("Bearer stored-token")
                    && request.header("mcp-session-id") == Some("session-1"))
                .await
        );
        call.abort();
    }
}

mod authenticating {
    use std::sync::{Arc, Mutex as StdMutex};

    use zeroize::Zeroizing;

    use super::*;
    use crate::mcp_auth_store::CredentialStore;
    use crate::mcp_contract::McpAuthConfig;
    use crate::test_support::{FakeServer, Reply};

    fn remote(url: &str) -> McpServerConfig {
        McpServerConfig {
            allow_stored_credentials: true,
            auth: Some(McpAuthConfig {
                client_id: Some("configured".to_owned()),
                ..McpAuthConfig::default()
            }),
            ..McpServerConfig::remote("remote", TransportType::Http, url)
        }
    }

    fn options(data: &std::path::Path) -> ConnectOptions {
        ConnectOptions {
            profile_data: Some(data.to_path_buf()),
            ..ConnectOptions::default()
        }
    }

    async fn attempt(
        config: &McpServerConfig,
        options: &ConnectOptions,
        environment: &(dyn Fn(&str) -> Option<String> + Sync),
    ) -> (Result<AuthenticationOutcome, McpError>, Vec<String>) {
        let opened = Arc::new(StdMutex::new(Vec::new()));
        let seen = Arc::clone(&opened);
        let open = move |url: &str| {
            seen.lock().unwrap().push(url.to_owned());
            false
        };
        let result = authenticate(
            config,
            options,
            &Challenge::default(),
            &open,
            &CancellationToken::new(),
            environment,
        )
        .await;
        let urls = opened.lock().unwrap().clone();
        (result, urls)
    }

    #[tokio::test]
    async fn only_remote_servers_that_read_stored_grants_and_have_their_secrets_authenticate() {
        let data = tempfile::tempdir().unwrap();
        let options = options(data.path());
        let pending = McpServerConfig {
            source: ConfigSource::Workspace,
            workspace_admission: Some(WorkspaceAdmission::Pending),
            ..remote("https://mcp.example/mcp")
        };
        let approved = McpServerConfig {
            source: ConfigSource::Workspace,
            workspace_admission: Some(WorkspaceAdmission::Approved),
            allow_stored_credentials: false,
            ..remote("https://mcp.example/mcp")
        };
        let stdio = McpServerConfig::stdio("local", "/bin/true", Vec::new());
        let mut secretive = remote("https://mcp.example/mcp");
        secretive.auth = Some(McpAuthConfig {
            client_secret_env: Some("OH_FX_TEST_CLIENT_SECRET".to_owned()),
            ..McpAuthConfig::default()
        });
        for (config, options, expected) in [
            (&pending, &options, McpError::McpWorkspaceApprovalRequired),
            (&stdio, &options, McpError::McpAuthenticationNotRemote),
            (
                &approved,
                &options,
                McpError::McpStoredCredentialsNotAllowed,
            ),
            (
                &secretive,
                &options,
                McpError::McpClientSecretEnvironmentMissing,
            ),
            (
                &remote("https://mcp.example/mcp"),
                &ConnectOptions::default(),
                McpError::HomeNotSet,
            ),
        ] {
            let (result, urls) = attempt(config, options, &|_| None).await;
            assert_eq!(result, Err(expected));
            assert!(urls.is_empty());
        }
    }

    #[tokio::test]
    async fn the_stored_grant_scope_is_requested_again() {
        let server = FakeServer::start(|request| {
            let origin = format!(
                "http://{}",
                request.header("host").unwrap_or_default()
            );
            match request.path.as_str() {
                "/.well-known/oauth-protected-resource/mcp" => Reply::json(&format!(
                    r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}"]}}"#
                )),
                "/.well-known/oauth-authorization-server" => Reply::json(&format!(
                    r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"]}}"#
                )),
                _ => Reply::status(404),
            }
        })
        .await;
        let data = tempfile::tempdir().unwrap();
        let data_path = std::fs::canonicalize(data.path()).unwrap().join("oh-fx");
        let previous = Credentials {
            endpoint: server.url.clone(),
            resource: server.url.clone(),
            issuer: "https://issuer.example".to_owned(),
            client_id: "configured".to_owned(),
            client_secret: None,
            access_token: Zeroizing::new("old".to_owned()),
            refresh_token: None,
            scope: "earlier.scope".to_owned(),
            token_type: "Bearer".to_owned(),
            token_endpoint_auth_method: "none".to_owned(),
            expires_at_ms: i64::MAX,
            authorization_endpoint: "https://issuer.example/authorize".to_owned(),
            token_endpoint: "https://issuer.example/token".to_owned(),
            revocation_endpoint: None,
        };
        CredentialStore::new(&data_path)
            .save(
                &GrantLookup::for_server(&remote(&server.url)).unwrap(),
                &previous,
            )
            .unwrap();
        let (result, urls) = attempt(&remote(&server.url), &options(&data_path), &|_| None).await;
        assert_eq!(result, Err(McpError::McpAuthorizationBrowserOpenFailed));
        let (_, query) = urls[0].split_once('?').unwrap();
        assert_eq!(
            ofx_auth::query_value(query, "scope").unwrap().as_str(),
            "earlier.scope"
        );
    }
}
