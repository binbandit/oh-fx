use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StateMutex, PoisonError};

use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{RequestBuilder, Response, StatusCode};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::auth_state::AuthState;
use crate::error::McpError;
use crate::mcp_auth::{
    Credentials, collect_authenticate_header, now_ms, parse_challenge, refresh_credentials,
    revoke_credentials,
};
use crate::mcp_auth_store::{CredentialStore, GrantLookup};
use crate::mcp_contract::{HttpHeader, McpServerConfig};
use crate::server_transport::StartupFailure;
use crate::streamable_http::{HeaderError, validate_header_value, validate_static_headers};

const CREDENTIALS_EXPIRED: &str = "MCP credentials expired.";
const REFRESH_FAILED: &str = "MCP credential refresh failed.";
const UNREADABLE_STORE: &str = "Stored MCP credentials could not be read securely.";

pub(crate) struct HttpAuth {
    server: String,
    state: Arc<AuthState>,
    epoch: u64,
    headers: Vec<HttpHeader>,
    stored: Option<StoredAuth>,
    rejection: StateMutex<Option<Rejection>>,
    stream_rejected: AtomicBool,
}

struct Rejection {
    header: Option<String>,
}

struct StoredAuth {
    server: String,
    state: Arc<AuthState>,
    generation: u64,
    epoch: u64,
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
        let epoch = state.begin_connection()?;
        let loaded = match (store, state.grant()) {
            (Some(store), Some(credentials)) => Some((store, credentials)),
            (Some(store), None) if config.allow_stored_credentials => {
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
        let generation = state.credentials_loaded(epoch, loaded.is_some())?;
        let stored = match loaded {
            Some((store, credentials)) => Some(StoredAuth {
                server: config.name.clone(),
                state: Arc::clone(state),
                generation,
                epoch,
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
            epoch,
            headers,
            stored,
            rejection: StateMutex::new(None),
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
                .rejection
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(Rejection { header });
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
        let rejection = self
            .rejection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if rejection.is_none()
            && let Some(failure) = self.failure()
        {
            return failure;
        }
        let challenge = rejection
            .and_then(|rejection| rejection.header)
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

    pub(crate) fn current(&self) -> bool {
        self.state.bound_to(self.epoch)
    }

    pub(crate) async fn apply(&self, builder: RequestBuilder) -> Result<RequestBuilder, McpError> {
        self.ensure_current()?;
        let builder = self.apply_static(builder);
        match &self.stored {
            Some(stored) => {
                let bearer = stored.authorization().await?;
                self.ensure_current()?;
                Ok(builder.header(AUTHORIZATION, bearer))
            }
            None => Ok(builder),
        }
    }

    fn ensure_current(&self) -> Result<(), McpError> {
        self.state.check_authority()?;
        if self.current() {
            Ok(())
        } else {
            Err(McpError::McpAuthorityChanged)
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
        self.state.check_authority()?;
        let generation = self.generation;
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
            epoch: self.epoch,
            http: self.http.clone(),
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
    epoch: u64,
    http: reqwest::Client,
    bearer: Arc<StateMutex<HeaderValue>>,
    failure: Arc<StateMutex<Option<String>>>,
}

impl Installing {
    async fn install(
        self,
        mut credentials: OwnedMutexGuard<Credentials>,
        refreshed: Credentials,
    ) -> Result<HeaderValue, McpError> {
        let _publishing = self.state.publication().await;
        if !self.state.publishable(self.generation) {
            return Ok(self.discard(refreshed));
        }
        let saved = refreshed.clone();
        let store = self.store.clone();
        let server = self.server.clone();
        let result = tokio::task::spawn_blocking(move || store.save_refreshed(&server, &saved))
            .await
            .map_err(|_| McpError::Cancelled)?;
        if let Some(saved) = result? {
            trace_store_repair("refresh", &self.server, saved.repaired_entries);
        }
        let header = bearer_header(&refreshed)?;
        if !self.state.refreshed(self.generation, &refreshed) {
            return Ok(self.discard(refreshed));
        }
        *credentials = refreshed;
        *self.bearer.lock().unwrap_or_else(PoisonError::into_inner) = header.clone();
        *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(header)
    }

    fn discard(&self, refreshed: Credentials) -> HeaderValue {
        if self.state.logged_out_since(self.epoch) {
            let http = self.http.clone();
            tokio::spawn(async move {
                let _ = revoke_credentials(&http, &refreshed).await;
            });
        }
        self.current_bearer()
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

mod interactive;
mod logout;

pub use interactive::{AuthenticationOutcome, authentication_error_message};
pub(crate) use interactive::{GrantUse, authenticate, authentication_target};
pub use logout::LogoutResult;
pub(crate) use logout::logout;

#[cfg(test)]
mod capture_tests;

#[cfg(test)]
mod tests;
