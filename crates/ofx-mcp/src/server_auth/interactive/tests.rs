use std::sync::Arc;

use ofx_auth::query_value;

use super::*;
use crate::health::AuthenticationState;
use crate::test_support::{FakeServer, Reply};

const TOKEN: &str = r#"{"access_token":"granted-access","refresh_token":"granted-refresh","expires_in":3600,"token_type":"Bearer"}"#;

async fn authority() -> FakeServer {
    FakeServer::start(|request| {
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
            ("POST", "/token") => Reply::json(TOKEN),
            _ => Reply::status(404),
        }
    })
    .await
}

fn callback(url: &str) -> String {
    let (_, query) = url.split_once('?').unwrap();
    format!(
        "{}?code=the-code&state={}",
        query_value(query, "redirect_uri").unwrap().as_str(),
        query_value(query, "state").unwrap().as_str()
    )
}

fn visit(url: &str) {
    let callback = callback(url);
    tokio::spawn(async move {
        let client = ofx_http::build_connection_client(&ConnectionOptions::default()).unwrap();
        let _ = client.get(callback).send().await;
    });
}

struct Session {
    _data: tempfile::TempDir,
    root: std::path::PathBuf,
    options: ConnectOptions,
    store: CredentialStore,
}

impl Session {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(data.path()).unwrap().join("oh-fx");
        Self {
            options: ConnectOptions {
                profile_data: Some(root.clone()),
                ..ConnectOptions::default()
            },
            store: CredentialStore::new(&root),
            root,
            _data: data,
        }
    }

    fn saved(&self, config: &McpServerConfig) -> Option<String> {
        self.store
            .load(&GrantLookup::for_server(config).unwrap())
            .unwrap()
            .map(|credentials| credentials.access_token.as_str().to_owned())
    }
}

async fn run(
    session: &Session,
    config: &McpServerConfig,
    state: &Arc<AuthState>,
    grant_use: GrantUse,
    open_url: &(dyn Fn(&str) -> bool + Sync),
    cancel: &CancellationToken,
) -> Result<AuthenticationOutcome, McpError> {
    authenticate(
        config,
        &session.options,
        state,
        grant_use,
        open_url,
        cancel,
        &|_| None,
    )
    .await
}

#[tokio::test]
async fn a_session_grant_is_installed_for_a_project_server_and_saved() {
    let server = authority().await;
    let session = Session::new();
    let config = McpServerConfig {
        source: ConfigSource::Workspace,
        workspace_admission: Some(WorkspaceAdmission::Approved),
        ..McpServerConfig::remote("project", TransportType::Http, &server.url)
    };
    let state = Arc::new(AuthState::default());
    state.store_pending(crate::mcp_auth::Challenge::default());
    let open = |url: &str| {
        visit(url);
        true
    };
    let outcome = run(
        &session,
        &config,
        &state,
        GrantUse::ThisSession,
        &open,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(
        outcome,
        Ok(AuthenticationOutcome::Authenticated {
            repaired_entries: 0
        })
    );
    assert_eq!(
        state
            .grant()
            .map(|grant| grant.access_token.as_str().to_owned()),
        Some("granted-access".to_owned())
    );
    assert_eq!(
        state.authentication(&config),
        AuthenticationState::Authenticated
    );
    assert_eq!(state.pending(), crate::mcp_auth::Challenge::default());
    assert_eq!(session.saved(&config).as_deref(), Some("granted-access"));
    let refused = run(
        &session,
        &config,
        &Arc::default(),
        GrantUse::LaterSessions,
        &open,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(refused, Err(McpError::McpStoredCredentialsNotAllowed));
}

#[tokio::test]
async fn a_challenge_or_a_cancel_during_the_flow_withholds_the_grant() {
    let server = authority().await;
    let session = Session::new();
    let config = McpServerConfig {
        allow_stored_credentials: true,
        ..McpServerConfig::remote("remote", TransportType::Http, &server.url)
    };
    let state = Arc::new(AuthState::default());
    let challenged = Arc::clone(&state);
    let challenge = move |url: &str| {
        challenged.store_pending(crate::mcp_auth::Challenge {
            scope: Some("tools.admin".to_owned()),
            ..crate::mcp_auth::Challenge::default()
        });
        visit(url);
        true
    };
    assert_eq!(
        run(
            &session,
            &config,
            &state,
            GrantUse::ThisSession,
            &challenge,
            &CancellationToken::new(),
        )
        .await,
        Err(McpError::McpAuthorityChanged)
    );
    assert!(state.grant().is_none());
    assert_eq!(state.authentication(&config), AuthenticationState::Required);
    assert_eq!(session.saved(&config), None);
    let cancel = CancellationToken::new();
    let cancelling = cancel.clone();
    let abandon = move |url: &str| {
        cancelling.cancel();
        visit(url);
        true
    };
    assert_eq!(
        run(
            &session,
            &config,
            &state,
            GrantUse::ThisSession,
            &abandon,
            &cancel,
        )
        .await,
        Err(McpError::Cancelled)
    );
    assert!(state.grant().is_none());
    assert_eq!(session.saved(&config), None);
}

#[test]
fn the_callback_timeout_message_names_oh_fx() {
    assert_eq!(
        authentication_error_message(&McpError::McpAuthorizationCallbackTimedOut),
        CALLBACK_TIMED_OUT
    );
    assert_eq!(
        authentication_error_message(&McpError::McpAuthorityChanged),
        "McpAuthorityChanged"
    );
}

#[tokio::test]
async fn a_flow_superseded_while_the_store_is_locked_neither_saves_nor_publishes() {
    for challenged in [false, true] {
        let server = authority().await;
        let session = Session::new();
        let config = McpServerConfig::remote("remote", TransportType::Http, &server.url);
        let state = Arc::new(AuthState::default());
        let held = ofx_config::PrivateDir::open_or_create(&session.root)
            .unwrap()
            .open_or_create_child("mcp-credentials")
            .unwrap()
            .try_lock("credentials.lock")
            .unwrap()
            .unwrap();
        let cancel = CancellationToken::new();
        let flow = {
            let config = config.clone();
            let options = session.options.clone();
            let state = Arc::clone(&state);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let open = |url: &str| {
                    visit(url);
                    true
                };
                authenticate(
                    &config,
                    &options,
                    &state,
                    GrantUse::ThisSession,
                    &open,
                    &cancel,
                    &|_| None,
                )
                .await
            })
        };
        assert!(server.wait_for(|request| request.path == "/token").await);
        if challenged {
            state.store_pending(crate::mcp_auth::Challenge {
                scope: Some("tools.admin".to_owned()),
                ..crate::mcp_auth::Challenge::default()
            });
        } else {
            cancel.cancel();
        }
        drop(held);
        let expected = if challenged {
            McpError::McpAuthorityChanged
        } else {
            McpError::Cancelled
        };
        assert_eq!(flow.await.unwrap(), Err(expected), "{challenged}");
        assert_eq!(session.saved(&config), None, "{challenged}");
        assert!(state.grant().is_none());
        if challenged {
            assert_eq!(state.authentication(&config), AuthenticationState::Required);
            assert_eq!(state.pending().scope.as_deref(), Some("tools.admin"));
        }
    }
}
