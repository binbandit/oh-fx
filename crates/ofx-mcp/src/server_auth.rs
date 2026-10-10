use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StateMutex, PoisonError};

use ofx_http::ConnectionOptions;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{RequestBuilder, Response, StatusCode};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::auth_state::AuthState;
use crate::error::McpError;
use crate::mcp_auth::{
    AuthorizationResult, Challenge, ClientConfig, Credentials, IssuerMismatch,
    authorize_interactive, collect_authenticate_header, now_ms, parse_challenge,
    refresh_credentials,
};
use crate::mcp_auth_store::{CredentialStore, GrantLookup};
use crate::mcp_contract::{
    ConfigSource, HttpHeader, McpServerConfig, TransportType, WorkspaceAdmission,
};
use crate::server_transport::{ConnectOptions, StartupFailure};
use crate::streamable_http::{HeaderError, validate_header_value, validate_static_headers};

const CREDENTIALS_EXPIRED: &str = "MCP credentials expired.";
const REFRESH_FAILED: &str = "MCP credential refresh failed.";
const UNREADABLE_STORE: &str = "Stored MCP credentials could not be read securely.";

pub(crate) struct HttpAuth {
    server: String,
    state: Arc<AuthState>,
    headers: Vec<HttpHeader>,
    stored: Option<StoredAuth>,
    challenge: StateMutex<Option<String>>,
    stream_rejected: AtomicBool,
}

struct StoredAuth {
    server: String,
    state: Arc<AuthState>,
    store: CredentialStore,
    http: reqwest::Client,
    credentials: Arc<Mutex<Credentials>>,
    bearer: Arc<StateMutex<HeaderValue>>,
    failure: Arc<StateMutex<Option<String>>>,
}

impl HttpAuth {
    pub(crate) async fn resolve(
        config: &McpServerConfig,
        store: Option<CredentialStore>,
        state: &Arc<AuthState>,
        oauth_client: impl FnOnce() -> Result<reqwest::Client, McpError>,
        environment: &(dyn Fn(&str) -> Option<String> + Sync),
    ) -> Result<Self, StartupFailure> {
        let loaded = match store {
            Some(store) if config.allow_stored_credentials => {
                let lookup = GrantLookup::for_server(config)
                    .map_err(|error| unreadable_store(&config.name, error))?;
                load_stored(lookup, &store)
                    .await
                    .map_err(|error| unreadable_store(&config.name, error))?
                    .map(|credentials| (store, credentials))
            }
            _ => None,
        };
        let headers = resolve_headers(
            config,
            environment,
            loaded.as_ref().map(|(_, credentials)| credentials),
        )?;
        state.set_credentials_loaded(loaded.is_some());
        let stored = match loaded {
            Some((store, credentials)) => Some(StoredAuth {
                server: config.name.clone(),
                state: Arc::clone(state),
                store,
                http: oauth_client()?,
                bearer: Arc::new(StateMutex::new(bearer_header(&credentials)?)),
                credentials: Arc::new(Mutex::new(credentials)),
                failure: Arc::default(),
            }),
            None => None,
        };
        Ok(Self {
            server: config.name.clone(),
            state: Arc::clone(state),
            headers,
            stored,
            challenge: StateMutex::new(None),
            stream_rejected: AtomicBool::new(false),
        })
    }

    pub(crate) fn reject(&self, response: &Response) -> Result<(), McpError> {
        let status = response.status();
        if status.is_redirection() {
            return Err(McpError::RedirectNotAllowed);
        }
        let header = collect_authenticate_header(response.headers());
        if status == StatusCode::UNAUTHORIZED
            || (status == StatusCode::FORBIDDEN && header.is_some())
        {
            *self
                .challenge
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = header;
            return Err(McpError::McpAuthenticationRequired);
        }
        Ok(())
    }

    pub(crate) fn reject_stream(&self) {
        self.stream_rejected.store(true, Ordering::Release);
    }

    pub(crate) fn stream_rejected(&self) -> bool {
        self.stream_rejected.load(Ordering::Acquire)
    }

    pub(crate) fn capture(&self) -> String {
        if let Some(failure) = self.failure() {
            return failure;
        }
        let header = self
            .challenge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let challenge = header
            .as_deref()
            .and_then(parse_challenge)
            .unwrap_or_default();
        self.state.store_pending(challenge);
        format!(
            "Authentication required. Run /mcp auth {} --open, or configure bearer_token_env.",
            self.server
        )
    }

    pub(crate) fn startup_failure(&self, failure: StartupFailure) -> StartupFailure {
        if failure.message.is_some() {
            return failure;
        }
        let message = if failure.error == McpError::McpAuthenticationRequired {
            Some(self.capture())
        } else {
            self.failure()
        };
        StartupFailure::explained(failure.error, message)
    }

    pub(crate) async fn apply(&self, builder: RequestBuilder) -> Result<RequestBuilder, McpError> {
        let builder = self.apply_static(builder);
        match &self.stored {
            Some(stored) => Ok(builder.header(AUTHORIZATION, stored.authorization().await?)),
            None => Ok(builder),
        }
    }

    pub(crate) fn apply_current(&self, builder: RequestBuilder) -> RequestBuilder {
        let builder = self.apply_static(builder);
        match &self.stored {
            Some(stored) => builder.header(AUTHORIZATION, stored.current_bearer()),
            None => builder,
        }
    }

    pub(crate) fn failure(&self) -> Option<String> {
        self.stored.as_ref().and_then(StoredAuth::failure)
    }

    fn apply_static(&self, builder: RequestBuilder) -> RequestBuilder {
        self.headers.iter().fold(builder, |builder, header| {
            builder.header(header.name.as_str(), header.value.as_str())
        })
    }
}

fn unreadable_store(server: &str, error: McpError) -> StartupFailure {
    if error == McpError::Cancelled {
        return StartupFailure::from(error);
    }
    ofx_trace::log(
        "mcp",
        format_args!("credential load failed server={server} err={error}"),
    );
    StartupFailure::explained(error, Some(UNREADABLE_STORE.to_owned()))
}

async fn load_stored(
    lookup: GrantLookup,
    store: &CredentialStore,
) -> Result<Option<Credentials>, McpError> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || store.load(&lookup))
        .await
        .map_err(|_| McpError::Cancelled)?
}

impl StoredAuth {
    async fn authorization(&self) -> Result<HeaderValue, McpError> {
        let credentials = Arc::clone(&self.credentials).lock_owned().await;
        if !credentials.needs_refresh(now_ms()) {
            return bearer_header(&credentials);
        }
        let generation = self.state.generation();
        if credentials.refresh_token.is_none() {
            self.fail(CREDENTIALS_EXPIRED);
            self.state.mark_reauthentication_required(generation);
            return Err(McpError::McpAuthenticationRequired);
        }
        let refreshed = match refresh_credentials(&self.http, &credentials).await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                self.fail(REFRESH_FAILED);
                if error == McpError::McpRefreshRejected {
                    self.state.mark_reauthentication_required(generation);
                }
                return Err(error);
            }
        };
        let installing = Installing {
            store: self.store.clone(),
            server: self.server.clone(),
            state: Arc::clone(&self.state),
            generation,
            bearer: Arc::clone(&self.bearer),
            failure: Arc::clone(&self.failure),
        };
        tokio::spawn(installing.install(credentials, refreshed))
            .await
            .map_err(|_| McpError::Cancelled)?
    }

    fn current_bearer(&self) -> HeaderValue {
        self.bearer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn fail(&self, reason: &str) {
        let message = format!("{reason} Run /mcp auth {} --open.", self.server);
        *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(message);
    }

    fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct Installing {
    store: CredentialStore,
    server: String,
    state: Arc<AuthState>,
    generation: u64,
    bearer: Arc<StateMutex<HeaderValue>>,
    failure: Arc<StateMutex<Option<String>>>,
}

impl Installing {
    async fn install(
        self,
        mut credentials: OwnedMutexGuard<Credentials>,
        refreshed: Credentials,
    ) -> Result<HeaderValue, McpError> {
        if self.state.generation() != self.generation {
            return Ok(self.current_bearer());
        }
        let saved = refreshed.clone();
        let store = self.store.clone();
        let server = self.server.clone();
        let result = tokio::task::spawn_blocking(move || store.save_refreshed(&server, &saved))
            .await
            .map_err(|_| McpError::Cancelled)?;
        trace_store_repair("refresh", &self.server, result?.repaired_entries);
        let header = bearer_header(&refreshed)?;
        if !self.state.refreshed(self.generation) {
            return Ok(self.current_bearer());
        }
        *credentials = refreshed;
        *self.bearer.lock().unwrap_or_else(PoisonError::into_inner) = header.clone();
        *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(header)
    }

    fn current_bearer(&self) -> HeaderValue {
        self.bearer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

pub(crate) fn trace_store_repair(actor: &str, server: &str, repaired_entries: usize) {
    if repaired_entries > 0 {
        ofx_trace::log(
            "mcp",
            format_args!(
                "removed unreadable MCP credential entries actor={actor} server={server} count={repaired_entries}"
            ),
        );
    }
}

fn bearer_header(credentials: &Credentials) -> Result<HeaderValue, McpError> {
    let bearer = credentials.bearer();
    validate_header_value(&bearer)?;
    let mut value = HeaderValue::from_str(&bearer).map_err(|_| HeaderError::InvalidHeaderValue)?;
    value.set_sensitive(true);
    Ok(value)
}

fn resolve_headers(
    config: &McpServerConfig,
    environment: &dyn Fn(&str) -> Option<String>,
    stored: Option<&Credentials>,
) -> Result<Vec<HttpHeader>, McpError> {
    let mut headers = config.headers.clone();
    for reference in &config.header_env {
        let value = environment(&reference.env).ok_or(McpError::McpHeaderEnvironmentMissing)?;
        headers.push(HttpHeader {
            name: reference.name.clone(),
            value,
        });
    }
    let authorization = |value: String| HttpHeader {
        name: "Authorization".to_owned(),
        value,
    };
    if let Some(credentials) = stored {
        validate_header_value(&credentials.bearer())?;
        let mut checked = headers.clone();
        checked.push(authorization(String::new()));
        validate_static_headers(&checked)?;
    } else {
        if let Some(env_name) = &config.bearer_token_env {
            let token = environment(env_name).ok_or(McpError::McpBearerEnvironmentMissing)?;
            headers.push(authorization(format!("Bearer {token}")));
        }
        validate_static_headers(&headers)?;
    }
    Ok(headers)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthenticationOutcome {
    Authenticated { repaired_entries: usize },
    IssuerMismatch(IssuerMismatch),
}

pub(crate) async fn authenticate(
    config: &McpServerConfig,
    options: &ConnectOptions,
    challenge: &Challenge,
    open_url: &(dyn Fn(&str) -> bool + Sync),
    cancel: &CancellationToken,
    environment: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<AuthenticationOutcome, McpError> {
    if config.transport == TransportType::Stdio {
        return Err(McpError::McpAuthenticationNotRemote);
    }
    if config.source == ConfigSource::Workspace
        && config.workspace_admission != Some(WorkspaceAdmission::Approved)
    {
        return Err(McpError::McpWorkspaceApprovalRequired);
    }
    if !config.allow_stored_credentials {
        return Err(McpError::McpStoredCredentialsNotAllowed);
    }
    let store = options
        .profile_data
        .as_deref()
        .map(CredentialStore::new)
        .ok_or(McpError::HomeNotSet)?;
    let lookup = GrantLookup::for_server(config)?;
    let previous = load_stored(lookup.clone(), &store).await?;
    let auth = config.auth.clone().unwrap_or_default();
    let client_secret = auth
        .client_secret_env
        .as_deref()
        .map(|name| {
            environment(name)
                .map(Zeroizing::new)
                .ok_or(McpError::McpClientSecretEnvironmentMissing)
        })
        .transpose()?;
    let http = ofx_http::build_connection_client(&ConnectionOptions {
        user_agent: options.user_agent.clone(),
        follow_redirects: false,
        ..ConnectionOptions::default()
    })
    .map_err(|_| McpError::HttpClientUnavailable)?;
    let client = ClientConfig {
        resource: auth.resource.as_deref(),
        issuer: auth.issuer.as_deref(),
        client_id: auth.client_id.as_deref(),
        client_secret: client_secret.as_deref().map(String::as_str),
        client_metadata_url: auth.client_metadata_url.as_deref(),
        scopes: &auth.scopes,
        callback_port: auth.callback_port,
    };
    let previous_scope = previous
        .as_ref()
        .map(|credentials| credentials.scope.as_str());
    let credentials = match authorize_interactive(
        &http,
        config.remote_url()?,
        &client,
        challenge,
        previous_scope,
        open_url,
        cancel,
    )
    .await?
    {
        AuthorizationResult::Credentials(credentials) => credentials,
        AuthorizationResult::IssuerMismatch(mismatch) => {
            return Ok(AuthenticationOutcome::IssuerMismatch(mismatch));
        }
    };
    let saved = tokio::task::spawn_blocking(move || store.save(&lookup, &credentials))
        .await
        .map_err(|_| McpError::Cancelled)??;
    Ok(AuthenticationOutcome::Authenticated {
        repaired_entries: saved.repaired_entries,
    })
}

#[cfg(test)]
mod capture_tests;

#[cfg(test)]
mod tests;
