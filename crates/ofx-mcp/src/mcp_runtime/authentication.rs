use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::McpRuntime;
use crate::error::McpError;
use crate::mcp_auth_store::{CredentialStore, GrantLookup};
use crate::mcp_contract::TransportType;
use crate::server_auth::{
    AuthenticationOutcome, GrantUse, LogoutResult, authenticate, authentication_target,
    load_stored, logout,
};
use crate::server_lifecycle::{Lifecycle, Server};

impl McpRuntime {
    pub fn validate_authentication_server(&self, name: &str) -> Result<(), McpError> {
        self.authentication_server(name).map(drop)
    }

    pub async fn authenticate_server(
        &self,
        name: &str,
        open_url: &(dyn Fn(&str) -> bool + Sync),
        cancel: &CancellationToken,
    ) -> Result<AuthenticationOutcome, McpError> {
        self.authenticate_for(name, GrantUse::ThisSession, open_url, cancel)
            .await
    }

    pub async fn authenticate_server_for_later_sessions(
        &self,
        name: &str,
        open_url: &(dyn Fn(&str) -> bool + Sync),
        cancel: &CancellationToken,
    ) -> Result<AuthenticationOutcome, McpError> {
        self.authenticate_for(name, GrantUse::LaterSessions, open_url, cancel)
            .await
    }

    pub async fn reconnect_authenticated_server(
        &self,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<(), McpError> {
        let server = self.authentication_server(name)?;
        cancel
            .run_until_cancelled(server.reconnect())
            .await
            .unwrap_or(Err(McpError::Cancelled))
    }

    async fn authenticate_for(
        &self,
        name: &str,
        grant_use: GrantUse,
        open_url: &(dyn Fn(&str) -> bool + Sync),
        cancel: &CancellationToken,
    ) -> Result<AuthenticationOutcome, McpError> {
        let server = self.authentication_server(name)?;
        authenticate(
            &server.config,
            &self.options,
            &server.auth,
            grant_use,
            open_url,
            cancel,
            &|variable| std::env::var(variable).ok(),
        )
        .await
    }

    pub async fn load_stored_credentials_for_health(&self) -> Result<(), McpError> {
        let Some(store) = self
            .options
            .profile_data
            .as_deref()
            .map(CredentialStore::new)
        else {
            return Ok(());
        };
        for server in self.current() {
            let config = &server.config;
            if config.transport == TransportType::Stdio || !config.allow_stored_credentials {
                continue;
            }
            let stored = load_stored(GrantLookup::for_server(config)?, &store).await?;
            server.auth.note_stored_credentials(stored.is_some());
        }
        Ok(())
    }

    pub async fn logout_server(&self, name: &str) -> Result<LogoutResult, McpError> {
        let reloads = Arc::clone(&self.reloading).lock_owned().await;
        let server = self.named(name)?;
        logout(&server, &self.options, reloads).await
    }

    fn named(&self, name: &str) -> Result<Arc<Server>, McpError> {
        self.current()
            .into_iter()
            .find(|server| server.config.name == name)
            .ok_or(McpError::McpServerNotFound)
    }

    fn authentication_server(&self, name: &str) -> Result<Arc<Server>, McpError> {
        let server = self.named(name)?;
        if matches!(server.lifecycle(), Lifecycle::Starting) {
            return Err(McpError::McpDiscoveryInProgress);
        }
        authentication_target(&server.config)?;
        Ok(server)
    }
}
