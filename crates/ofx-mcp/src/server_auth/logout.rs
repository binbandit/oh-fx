use std::sync::Arc;

use tokio::sync::OwnedMutexGuard;

use super::interactive::oauth_client;
use super::load_stored;
use crate::error::McpError;
use crate::mcp_auth::{Credentials, revoke_credentials};
use crate::mcp_auth_store::{CredentialStore, DeleteResult, GrantLookup};
use crate::mcp_contract::{ConfigSource, TransportType, WorkspaceAdmission};
use crate::server_lifecycle::{Lifecycle, Server};
use crate::server_transport::ConnectOptions;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogoutResult {
    pub removed: bool,
    pub revocation_failed: bool,
    pub repaired_entries: usize,
    pub local_only: bool,
}

pub(crate) async fn logout(
    server: &Arc<Server>,
    options: &ConnectOptions,
    reloads: OwnedMutexGuard<()>,
) -> Result<LogoutResult, McpError> {
    let config = &server.config;
    if matches!(server.lifecycle(), Lifecycle::Starting) {
        return Err(McpError::McpDiscoveryInProgress);
    }
    if config.transport == TransportType::Stdio {
        return Err(McpError::McpAuthenticationNotRemote);
    }
    let store = options
        .profile_data
        .as_deref()
        .map(CredentialStore::new)
        .ok_or(McpError::HomeNotSet)?;
    let lookup = GrantLookup::for_server(config)?;
    if config.source == ConfigSource::Workspace
        && config.workspace_admission != Some(WorkspaceAdmission::Approved)
    {
        let deleted = delete(&store, &lookup).await?;
        return Ok(LogoutResult {
            removed: deleted.removed > 0,
            repaired_entries: deleted.repaired_entries,
            local_only: true,
            ..LogoutResult::default()
        });
    }
    let stored = if server.auth.grant().is_none() && config.allow_stored_credentials {
        load_stored(lookup.clone(), &store)
            .await
            .unwrap_or_else(|error| {
                ofx_trace::log(
                    "mcp",
                    format_args!(
                        "stored credential load skipped during logout server={} err={error}",
                        config.name
                    ),
                );
                None
            })
    } else {
        None
    };
    let fenced = tokio::spawn(log_out(
        Arc::clone(server),
        store,
        lookup,
        stored,
        options.clone(),
        reloads,
    ));
    fenced.await.map_err(|_| McpError::Cancelled)?
}

async fn log_out(
    server: Arc<Server>,
    store: CredentialStore,
    lookup: GrantLookup,
    stored: Option<Credentials>,
    options: ConnectOptions,
    reloads: OwnedMutexGuard<()>,
) -> Result<LogoutResult, McpError> {
    let detached = server.auth.begin_logout().await?;
    let deleted = match delete(&store, &lookup).await {
        Ok(deleted) => deleted,
        Err(error) => {
            server.auth.restore_after_logout(detached);
            return Err(error);
        }
    };
    let grant = detached.grant().cloned().or(stored);
    if deleted.removed == 0 && deleted.repaired_entries == 0 && grant.is_none() {
        server.auth.restore_after_logout(detached);
        return Ok(LogoutResult::default());
    }
    drop(detached);
    let discarded = server.auth.commit_logout();
    server.log_out().await;
    server.auth.finish_logout();
    drop(reloads);
    let mut revocation_failed = false;
    for revoked in grant.iter().chain(&discarded) {
        revocation_failed |= revoke(revoked, &options).await;
    }
    Ok(LogoutResult {
        removed: true,
        revocation_failed,
        repaired_entries: deleted.repaired_entries,
        local_only: false,
    })
}

async fn delete(store: &CredentialStore, lookup: &GrantLookup) -> Result<DeleteResult, McpError> {
    let store = store.clone();
    let lookup = lookup.clone();
    tokio::task::spawn_blocking(move || store.delete(&lookup))
        .await
        .map_err(|_| McpError::Cancelled)?
}

async fn revoke(grant: &Credentials, options: &ConnectOptions) -> bool {
    if grant.revocation_endpoint.is_none() {
        return false;
    }
    match oauth_client(options) {
        Ok(http) => revoke_credentials(&http, grant).await.is_err(),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests;
