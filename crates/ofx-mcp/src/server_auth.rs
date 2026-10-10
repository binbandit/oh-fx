use std::sync::{Mutex as StateMutex, PoisonError};

use reqwest::RequestBuilder;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use tokio::sync::Mutex;

use crate::error::McpError;
use crate::mcp_auth::{Credentials, now_ms, refresh_credentials};
use crate::mcp_auth_store::CredentialStore;
use crate::mcp_contract::{HttpHeader, McpServerConfig};
use crate::streamable_http::{HeaderError, validate_header_value, validate_static_headers};

const CREDENTIALS_EXPIRED: &str = "MCP credentials expired.";
const REFRESH_FAILED: &str = "MCP credential refresh failed.";

pub(crate) struct HttpAuth {
    headers: Vec<HttpHeader>,
    stored: Option<StoredAuth>,
}

struct StoredAuth {
    server: String,
    store: CredentialStore,
    http: reqwest::Client,
    credentials: Mutex<Credentials>,
    failure: StateMutex<Option<String>>,
}

impl HttpAuth {
    pub(crate) async fn resolve(
        config: &McpServerConfig,
        store: Option<CredentialStore>,
        http: &reqwest::Client,
        environment: &(dyn Fn(&str) -> Option<String> + Sync),
    ) -> Result<Self, McpError> {
        let loaded = match store {
            Some(store) if config.allow_stored_credentials => load_stored(config, &store)
                .await?
                .map(|credentials| (store, credentials)),
            _ => None,
        };
        let headers = resolve_headers(
            config,
            environment,
            loaded.as_ref().map(|(_, credentials)| credentials),
        )?;
        let stored = loaded.map(|(store, credentials)| StoredAuth {
            server: config.name.clone(),
            store,
            http: http.clone(),
            credentials: Mutex::new(credentials),
            failure: StateMutex::new(None),
        });
        Ok(Self { headers, stored })
    }

    pub(crate) async fn apply(&self, builder: RequestBuilder) -> Result<RequestBuilder, McpError> {
        let mut builder = builder;
        for header in &self.headers {
            builder = builder.header(header.name.as_str(), header.value.as_str());
        }
        if let Some(stored) = &self.stored {
            builder = builder.header(AUTHORIZATION, stored.authorization().await?);
        }
        Ok(builder)
    }

    pub(crate) fn failure(&self) -> Option<String> {
        self.stored.as_ref().and_then(StoredAuth::failure)
    }
}

async fn load_stored(
    config: &McpServerConfig,
    store: &CredentialStore,
) -> Result<Option<Credentials>, McpError> {
    let identity = config.name.clone();
    let endpoint = config.remote_url()?.to_owned();
    let auth = config.auth.clone().unwrap_or_default();
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        store.load(
            &identity,
            &endpoint,
            auth.resource.as_deref(),
            auth.issuer.as_deref(),
        )
    })
    .await
    .map_err(|_| McpError::Cancelled)?
}

impl StoredAuth {
    async fn authorization(&self) -> Result<HeaderValue, McpError> {
        let mut credentials = self.credentials.lock().await;
        if credentials.needs_refresh(now_ms()) {
            if credentials.refresh_token.is_none() {
                self.fail(CREDENTIALS_EXPIRED);
                return Err(McpError::McpAuthenticationRequired);
            }
            let refreshed = match refresh_credentials(&self.http, &credentials).await {
                Ok(refreshed) => refreshed,
                Err(error) => {
                    if !matches!(error, McpError::Cancelled | McpError::McpRequestTimedOut) {
                        self.fail(REFRESH_FAILED);
                    }
                    return Err(error);
                }
            };
            let store = self.store.clone();
            let server = self.server.clone();
            let saved = refreshed.clone();
            tokio::task::spawn_blocking(move || store.save(&server, &saved))
                .await
                .map_err(|_| McpError::Cancelled)??;
            *credentials = refreshed;
            *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
        bearer_header(&credentials)
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
    let authorization = match stored {
        Some(credentials) => Some(credentials.bearer().to_string()),
        None => match &config.bearer_token_env {
            Some(env_name) => Some(format!(
                "Bearer {}",
                environment(env_name).ok_or(McpError::McpBearerEnvironmentMissing)?
            )),
            None => None,
        },
    };
    let checked = authorization.map(|value| HttpHeader {
        name: "Authorization".to_owned(),
        value,
    });
    let mut all = headers.clone();
    all.extend(checked.iter().cloned());
    validate_static_headers(&all)?;
    if stored.is_none() {
        headers.extend(checked);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests;
