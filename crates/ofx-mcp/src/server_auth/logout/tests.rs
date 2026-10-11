use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use ofx_auth::query_value;
use tokio::sync::Notify;
use zeroize::Zeroizing;

use super::super::{GrantUse, HttpAuth, authenticate};
use crate::auth_state::AuthState;
use crate::error::McpError;
use crate::health::AuthenticationState;
use crate::mcp_auth::Credentials;
use crate::mcp_auth_store::{CredentialStore, GrantLookup};
use crate::mcp_contract::{McpServerConfig, TransportType};
use crate::server_lifecycle::{Lifecycle, Server};
use crate::server_transport::ConnectOptions;
use crate::test_support::{FakeServer, RecordedRequest, Reply};

const RENEWED: &str = r#"{"access_token":"renewed-access","refresh_token":"renewed-refresh","expires_in":3600,"token_type":"Bearer"}"#;

fn mcp_reply(request: &RecordedRequest) -> Reply {
    let Some(id) = request.request_id() else {
        return Reply::status(202);
    };
    match request.method_name().as_deref() {
        Some("initialize") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{}},"serverInfo":{{"name":"remote","version":"1"}}}}}}"#
        )),
        Some("tools/list") => Reply::json(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[]}}}}"#
        )),
        _ => Reply::status(202),
    }
}

fn authority_reply(request: &RecordedRequest) -> Reply {
    let origin = format!("http://{}", request.header("host").unwrap_or_default());
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/.well-known/oauth-protected-resource/mcp") => Reply::json(&format!(
            r#"{{"resource":"{origin}/mcp","authorization_servers":["{origin}"]}}"#
        )),
        ("GET", "/.well-known/oauth-authorization-server") => Reply::json(&format!(
            r#"{{"issuer":"{origin}","authorization_endpoint":"{origin}/authorize","token_endpoint":"{origin}/token","registration_endpoint":"{origin}/register","code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"]}}"#
        )),
        ("POST", "/register") => {
            Reply::json(r#"{"client_id":"registered-client"}"#).with_status(201)
        }
        ("POST", "/token") => Reply::json(RENEWED),
        _ => mcp_reply(request),
    }
}

struct Profile {
    _data: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Profile {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(data.path()).unwrap().join("oh-fx");
        Self { _data: data, root }
    }

    fn store(&self) -> CredentialStore {
        CredentialStore::new(&self.root)
    }

    fn options(&self) -> ConnectOptions {
        ConnectOptions {
            profile_data: Some(self.root.clone()),
            ..ConnectOptions::default()
        }
    }

    fn file(&self) -> std::path::PathBuf {
        self.root.join("mcp-credentials/credentials.json")
    }
}

fn config(url: &str) -> McpServerConfig {
    McpServerConfig {
        allow_stored_credentials: true,
        startup_timeout_ms: 5_000,
        ..McpServerConfig::remote("remote", TransportType::Http, url)
    }
}

fn grant(url: &str, expires_at_ms: i64) -> Credentials {
    Credentials {
        endpoint: url.to_owned(),
        resource: url.to_owned(),
        issuer: "https://issuer.example".to_owned(),
        client_id: "client".to_owned(),
        client_secret: None,
        access_token: Zeroizing::new("stored-access".to_owned()),
        refresh_token: Some(Zeroizing::new("stored-refresh".to_owned())),
        scope: "tools".to_owned(),
        token_type: "Bearer".to_owned(),
        token_endpoint_auth_method: "none".to_owned(),
        expires_at_ms,
        authorization_endpoint: "https://issuer.example/authorize".to_owned(),
        token_endpoint: url.replace("/mcp", "/token"),
        revocation_endpoint: None,
    }
}

fn oauth() -> reqwest::Client {
    ofx_http::build_connection_client(&ofx_http::ConnectionOptions {
        follow_redirects: false,
        ..ofx_http::ConnectionOptions::default()
    })
    .unwrap()
}

async fn fenced_logout(state: &AuthState, profile: &Profile, config: &McpServerConfig) {
    let detached = state.begin_logout().await.unwrap();
    profile
        .store()
        .delete(&GrantLookup::for_server(config).unwrap())
        .unwrap();
    drop(detached);
    state.commit_logout();
    state.finish_logout();
}

#[tokio::test]
async fn logout_cannot_persist_an_in_flight_refresh() {
    let release = Arc::new(Notify::new());
    let held = Arc::clone(&release);
    let server = FakeServer::start(move |request| {
        if request.path == "/token" {
            return Reply::json(RENEWED).released_by(&held);
        }
        mcp_reply(request)
    })
    .await;
    let profile = Profile::new();
    let config = config(&server.url);
    profile
        .store()
        .save(
            &GrantLookup::for_server(&config).unwrap(),
            &grant(&server.url, 1),
        )
        .unwrap();
    let state = Arc::new(AuthState::default());
    let auth = Arc::new(
        HttpAuth::resolve(
            &config,
            Some(profile.store()),
            &state,
            || Ok(oauth()),
            &|_| None,
        )
        .await
        .ok()
        .unwrap(),
    );
    let requesting = Arc::clone(&auth);
    let url = server.url.clone();
    let request = tokio::spawn(async move { requesting.apply(oauth().get(url)).await.is_ok() });
    assert!(server.wait_for(|request| request.path == "/token").await);
    fenced_logout(&state, &profile, &config).await;
    release.notify_one();
    assert!(request.await.unwrap());
    assert!(!profile.file().exists());
    assert!(state.grant().is_none());
    assert_eq!(state.authentication(&config), AuthenticationState::None);
    let fresh = HttpAuth::resolve(
        &config,
        Some(profile.store()),
        &state,
        || Ok(oauth()),
        &|_| None,
    )
    .await
    .ok()
    .unwrap();
    assert!(fresh.current());
    assert_eq!(state.authentication(&config), AuthenticationState::None);
}

#[tokio::test]
async fn an_authorization_that_meets_a_logout_is_not_published() {
    let release = Arc::new(Notify::new());
    let held = Arc::clone(&release);
    let server = FakeServer::start(move |request| {
        let reply = authority_reply(request);
        if request.path == "/token" {
            return reply.released_by(&held);
        }
        reply
    })
    .await;
    let profile = Profile::new();
    let config = config(&server.url);
    let state = Arc::new(AuthState::default());
    let open = |url: &str| {
        let (_, query) = url.split_once('?').unwrap();
        let callback = format!(
            "{}?code=the-code&state={}",
            query_value(query, "redirect_uri").unwrap().as_str(),
            query_value(query, "state").unwrap().as_str()
        );
        tokio::spawn(async move {
            let _ = oauth().get(callback).send().await;
        });
        true
    };
    let authorizing = {
        let state = Arc::clone(&state);
        let config = config.clone();
        let options = profile.options();
        tokio::spawn(async move {
            authenticate(
                &config,
                &options,
                &state,
                GrantUse::ThisSession,
                &open,
                &tokio_util::sync::CancellationToken::new(),
                &|_| None,
            )
            .await
        })
    };
    assert!(server.wait_for(|request| request.path == "/token").await);
    fenced_logout(&state, &profile, &config).await;
    release.notify_one();
    assert_eq!(
        authorizing.await.unwrap(),
        Err(McpError::McpAuthorityChanged)
    );
    assert!(state.grant().is_none());
    assert!(!profile.file().exists());
}

#[tokio::test]
async fn an_authorization_during_the_fence_fails_before_any_request() {
    let server = FakeServer::start(authority_reply).await;
    let profile = Profile::new();
    let config = config(&server.url);
    let state = Arc::new(AuthState::default());
    let detached = state.begin_logout().await.unwrap();
    let opened = Mutex::new(false);
    let open = |_: &str| {
        *opened.lock().unwrap() = true;
        true
    };
    assert_eq!(
        authenticate(
            &config,
            &profile.options(),
            &state,
            GrantUse::ThisSession,
            &open,
            &tokio_util::sync::CancellationToken::new(),
            &|_| None,
        )
        .await,
        Err(McpError::McpAuthorityChanged)
    );
    assert!(server.requests().is_empty());
    assert!(!*opened.lock().unwrap());
    state.restore_after_logout(detached);
}

#[tokio::test]
async fn a_connection_resolved_before_a_logout_is_not_published_after_it() {
    let release = Arc::new(Notify::new());
    let held = Arc::clone(&release);
    let server = FakeServer::start(move |request| {
        let reply = mcp_reply(request);
        if request.method_name().as_deref() == Some("initialize") {
            return reply.released_by(&held);
        }
        reply
    })
    .await;
    let profile = Profile::new();
    let config = config(&server.url);
    profile
        .store()
        .save(
            &GrantLookup::for_server(&config).unwrap(),
            &grant(&server.url, i64::MAX),
        )
        .unwrap();
    let live = Arc::new(Server::new(
        config.clone(),
        profile.options(),
        Arc::new(AtomicU64::new(0)),
    ));
    let starting = {
        let live = Arc::clone(&live);
        tokio::spawn(async move { live.start().await })
    };
    assert!(
        server
            .wait_for(|request| request.method_name().as_deref() == Some("initialize"))
            .await
    );
    fenced_logout(&live.auth, &profile, &config).await;
    release.notify_one();
    assert_eq!(starting.await.unwrap(), Err(McpError::McpAuthorityChanged));
    assert!(!matches!(live.lifecycle(), Lifecycle::Ready(_)));
}

#[tokio::test]
async fn requests_during_the_fence_fail_with_authority_changed() {
    let server = FakeServer::start(mcp_reply).await;
    let profile = Profile::new();
    let config = config(&server.url);
    profile
        .store()
        .save(
            &GrantLookup::for_server(&config).unwrap(),
            &grant(&server.url, i64::MAX),
        )
        .unwrap();
    let state = AuthState::default();
    let state = Arc::new(state);
    let auth = HttpAuth::resolve(
        &config,
        Some(profile.store()),
        &state,
        || Ok(oauth()),
        &|_| None,
    )
    .await
    .ok()
    .unwrap();
    let detached = state.begin_logout().await.unwrap();
    assert_eq!(
        auth.apply(oauth().get(&server.url)).await.err(),
        Some(McpError::McpAuthorityChanged)
    );
    let closing = auth
        .apply_current(oauth().delete(&server.url))
        .build()
        .unwrap();
    assert_eq!(
        closing
            .headers()
            .get("authorization")
            .map(|value| value.to_str().unwrap().to_owned())
            .as_deref(),
        Some("Bearer stored-access")
    );
    state.restore_after_logout(detached);
}

fn stored_token(profile: &Profile) -> Option<String> {
    std::fs::read_to_string(profile.file())
        .ok()
        .and_then(|text| {
            let value: serde_json::Value = serde_json::from_str(&text).ok()?;
            value["credentials"][0]["access_token"]
                .as_str()
                .map(str::to_owned)
        })
}

#[tokio::test]
async fn a_refresh_inside_the_fence_window_neither_saves_nor_installs() {
    let release = Arc::new(Notify::new());
    let held = Arc::clone(&release);
    let server = FakeServer::start(move |request| {
        if request.path == "/token" {
            return Reply::json(RENEWED).released_by(&held);
        }
        mcp_reply(request)
    })
    .await;
    let profile = Profile::new();
    let config = config(&server.url);
    profile
        .store()
        .save(
            &GrantLookup::for_server(&config).unwrap(),
            &grant(&server.url, 1),
        )
        .unwrap();
    let state = Arc::new(AuthState::default());
    let auth = Arc::new(
        HttpAuth::resolve(
            &config,
            Some(profile.store()),
            &state,
            || Ok(oauth()),
            &|_| None,
        )
        .await
        .ok()
        .unwrap(),
    );
    let requesting = Arc::clone(&auth);
    let url = server.url.clone();
    let request = tokio::spawn(async move {
        requesting
            .apply(oauth().get(url))
            .await
            .unwrap()
            .build()
            .unwrap()
            .headers()
            .get("authorization")
            .map(|value| value.to_str().unwrap().to_owned())
    });
    assert!(server.wait_for(|request| request.path == "/token").await);
    let detached = state.begin_logout().await.unwrap();
    release.notify_one();
    assert_eq!(
        request.await.unwrap().as_deref(),
        Some("Bearer stored-access")
    );
    assert!(state.grant().is_none());
    assert_eq!(stored_token(&profile).as_deref(), Some("stored-access"));
    state.restore_after_logout(detached);
    assert_eq!(
        state.authentication(&config),
        AuthenticationState::Authenticated
    );
    assert!(state.grant().is_none());
}

#[tokio::test]
async fn an_authorization_inside_the_fence_window_is_refused() {
    let release = Arc::new(Notify::new());
    let held = Arc::clone(&release);
    let server = FakeServer::start(move |request| {
        let reply = authority_reply(request);
        if request.path == "/token" {
            return reply.released_by(&held);
        }
        reply
    })
    .await;
    let profile = Profile::new();
    let config = McpServerConfig::remote("remote", TransportType::Http, &server.url);
    let state = Arc::new(AuthState::default());
    let authorizing = {
        let state = Arc::clone(&state);
        let config = config.clone();
        let options = profile.options();
        tokio::spawn(async move {
            let open = |url: &str| {
                let (_, query) = url.split_once('?').unwrap();
                let callback = format!(
                    "{}?code=the-code&state={}",
                    query_value(query, "redirect_uri").unwrap().as_str(),
                    query_value(query, "state").unwrap().as_str()
                );
                tokio::spawn(async move {
                    let _ = oauth().get(callback).send().await;
                });
                true
            };
            authenticate(
                &config,
                &options,
                &state,
                GrantUse::ThisSession,
                &open,
                &tokio_util::sync::CancellationToken::new(),
                &|_| None,
            )
            .await
        })
    };
    assert!(server.wait_for(|request| request.path == "/token").await);
    let detached = state.begin_logout().await.unwrap();
    release.notify_one();
    assert_eq!(
        authorizing.await.unwrap(),
        Err(McpError::McpAuthorityChanged)
    );
    assert!(state.grant().is_none());
    assert_eq!(stored_token(&profile), None);
    state.restore_after_logout(detached);
    assert!(state.grant().is_none());
    assert_eq!(state.check_authority(), Ok(()));
}
