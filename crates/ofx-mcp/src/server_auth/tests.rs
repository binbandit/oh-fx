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
            .save("remote", &fixture.credentials(i64::MAX, None))
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
            .save("remote", &fixture.credentials(i64::MAX, None))
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
            .save("remote", &fixture.credentials(1, Some("stored-refresh")))
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
        let saved = fixture
            .store()
            .load("remote", &fixture.server.url, None, None)
            .unwrap()
            .unwrap();
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
            .save("remote", &fixture.credentials(1, None))
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
            .save("remote", &fixture.credentials(1, Some("stored-refresh")))
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
        assert!(fixture.server.requests().is_empty());
    }
}
