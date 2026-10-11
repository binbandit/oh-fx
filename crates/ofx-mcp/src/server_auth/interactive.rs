use std::borrow::Cow;
use std::sync::Arc;

use ofx_http::ConnectionOptions;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::load_stored;
use crate::auth_state::AuthState;
use crate::error::McpError;
use crate::mcp_auth::{AuthorizationResult, ClientConfig, IssuerMismatch, authorize_interactive};
use crate::mcp_auth_store::{CredentialStore, GrantLookup};
use crate::mcp_contract::{ConfigSource, McpServerConfig, TransportType, WorkspaceAdmission};
use crate::server_transport::ConnectOptions;

const CALLBACK_TIMED_OUT: &str = "Authorization timed out. Run the connection command again and finish authorization in your browser while oh-fx stays open";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthenticationOutcome {
    Authenticated { repaired_entries: usize },
    IssuerMismatch(IssuerMismatch),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantUse {
    LaterSessions,
    ThisSession,
}

#[must_use]
pub fn authentication_error_message(error: &McpError) -> Cow<'static, str> {
    match error {
        McpError::McpAuthorizationCallbackTimedOut => Cow::Borrowed(CALLBACK_TIMED_OUT),
        _ => Cow::Owned(error.to_string()),
    }
}

pub(crate) fn authentication_target(config: &McpServerConfig) -> Result<(), McpError> {
    if config.transport == TransportType::Stdio {
        return Err(McpError::McpAuthenticationNotRemote);
    }
    if config.source == ConfigSource::Workspace
        && config.workspace_admission != Some(WorkspaceAdmission::Approved)
    {
        return Err(McpError::McpWorkspaceApprovalRequired);
    }
    Ok(())
}

pub(crate) async fn authenticate(
    config: &McpServerConfig,
    options: &ConnectOptions,
    state: &Arc<AuthState>,
    grant_use: GrantUse,
    open_url: &(dyn Fn(&str) -> bool + Sync),
    cancel: &CancellationToken,
    environment: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<AuthenticationOutcome, McpError> {
    authentication_target(config)?;
    if grant_use == GrantUse::LaterSessions && !config.allow_stored_credentials {
        return Err(McpError::McpStoredCredentialsNotAllowed);
    }
    let store = options
        .profile_data
        .as_deref()
        .map(CredentialStore::new)
        .ok_or(McpError::HomeNotSet)?;
    let lookup = GrantLookup::for_server(config)?;
    let previous = match state.grant() {
        Some(credentials) => Some(credentials),
        None if config.allow_stored_credentials => load_stored(lookup.clone(), &store).await?,
        None => None,
    };
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
    let generation = state.generation();
    let challenge = state.pending();
    let credentials = match authorize_interactive(
        &http,
        config.remote_url()?,
        &client,
        &challenge,
        previous_scope,
        open_url,
        cancel,
    )
    .await?
    {
        AuthorizationResult::Credentials(credentials) => *credentials,
        AuthorizationResult::IssuerMismatch(mismatch) => {
            return Ok(AuthenticationOutcome::IssuerMismatch(mismatch));
        }
    };
    let publishing = Arc::clone(state);
    let stop = cancel.clone();
    let saved = tokio::task::spawn_blocking(move || {
        let lock = store.lock_for_save()?;
        publishing.publish(generation, &stop, credentials, |grant| {
            lock.save(&lookup, grant)
        })
    })
    .await
    .map_err(|_| McpError::Cancelled)??;
    Ok(AuthenticationOutcome::Authenticated {
        repaired_entries: saved.repaired_entries,
    })
}

#[cfg(test)]
mod tests;
