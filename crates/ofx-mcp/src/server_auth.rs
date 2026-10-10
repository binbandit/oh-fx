use std::sync::{Arc, Mutex as StateMutex, PoisonError};

use ofx_http::ConnectionOptions;
use reqwest::RequestBuilder;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::error::McpError;
use crate::mcp_auth::{
    AuthorizationResult, ClientConfig, Credentials, authorize_interactive, now_ms,
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
    headers: Vec<HttpHeader>,
    stored: Option<StoredAuth>,
}

struct StoredAuth {
    server: String,
    lookup: GrantLookup,
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
        oauth_client: impl FnOnce() -> Result<reqwest::Client, McpError>,
        environment: &(dyn Fn(&str) -> Option<String> + Sync),
    ) -> Result<Self, StartupFailure> {
        let loaded = match store {
            Some(store) if config.allow_stored_credentials => {
                let lookup = GrantLookup::for_server(config)
                    .map_err(|error| unreadable_store(&config.name, error))?;
                load_stored(lookup.clone(), &store)
                    .await
                    .map_err(|error| unreadable_store(&config.name, error))?
                    .map(|credentials| (store, lookup, credentials))
            }
            _ => None,
        };
        let headers = resolve_headers(
            config,
            environment,
            loaded.as_ref().map(|(_, _, credentials)| credentials),
        )?;
        let stored = match loaded {
            Some((store, lookup, credentials)) => Some(StoredAuth {
                server: config.name.clone(),
                lookup,
                store,
                http: oauth_client()?,
                bearer: Arc::new(StateMutex::new(bearer_header(&credentials)?)),
                credentials: Arc::new(Mutex::new(credentials)),
                failure: Arc::default(),
            }),
            None => None,
        };
        Ok(Self { headers, stored })
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
        if credentials.refresh_token.is_none() {
            self.fail(CREDENTIALS_EXPIRED);
            return Err(McpError::McpAuthenticationRequired);
        }
        let refreshed = match refresh_credentials(&self.http, &credentials).await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                self.fail(REFRESH_FAILED);
                return Err(error);
            }
        };
        let installing = Installing {
            store: self.store.clone(),
            server: self.server.clone(),
            lookup: self.lookup.clone(),
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
    lookup: GrantLookup,
    bearer: Arc<StateMutex<HeaderValue>>,
    failure: Arc<StateMutex<Option<String>>>,
}

impl Installing {
    async fn install(
        self,
        mut credentials: OwnedMutexGuard<Credentials>,
        refreshed: Credentials,
    ) -> Result<HeaderValue, McpError> {
        let saved = refreshed.clone();
        let store = self.store;
        let (server, result) = tokio::task::spawn_blocking(move || {
            let result = store.save(&self.lookup, &saved);
            (self.server, result)
        })
        .await
        .map_err(|_| McpError::Cancelled)?;
        trace_store_repair("refresh", &server, result?.repaired_entries);
        let header = bearer_header(&refreshed)?;
        *credentials = refreshed;
        *self.bearer.lock().unwrap_or_else(PoisonError::into_inner) = header.clone();
        *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(header)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationOutcome {
    Authenticated { repaired_entries: usize },
    IssuerMismatch,
}

pub(crate) async fn authenticate(
    config: &McpServerConfig,
    options: &ConnectOptions,
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
        previous_scope,
        open_url,
        cancel,
    )
    .await?
    {
        AuthorizationResult::Credentials(credentials) => credentials,
        AuthorizationResult::IssuerMismatch => return Ok(AuthenticationOutcome::IssuerMismatch),
    };
    let saved = tokio::task::spawn_blocking(move || store.save(&lookup, &credentials))
        .await
        .map_err(|_| McpError::Cancelled)??;
    Ok(AuthenticationOutcome::Authenticated {
        repaired_entries: saved.repaired_entries,
    })
}

#[cfg(test)]
mod tests;
