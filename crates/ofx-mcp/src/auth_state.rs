use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio_util::sync::CancellationToken;

use crate::error::McpError;
use crate::health::AuthenticationState;
use crate::mcp_auth::{Challenge, Credentials};
use crate::mcp_contract::McpServerConfig;

#[derive(Debug, Default)]
pub(crate) struct AuthState {
    inner: Mutex<AuthInner>,
}

#[derive(Debug, Default)]
struct AuthInner {
    generation: u64,
    pending: Option<Challenge>,
    challenge_present: bool,
    credentials_present: bool,
    grant: Option<Credentials>,
}

impl AuthState {
    pub(crate) fn store_pending(&self, challenge: Challenge) {
        let mut inner = self.lock();
        inner.generation = inner.generation.saturating_add(1);
        inner.pending = Some(challenge);
        inner.challenge_present = true;
    }

    pub(crate) fn mark_reauthentication_required(&self, expected_generation: u64) {
        let mut inner = self.lock();
        if inner.generation != expected_generation {
            return;
        }
        inner.generation = inner.generation.saturating_add(1);
        inner.challenge_present = true;
        inner.credentials_present = false;
    }

    pub(crate) fn refreshed(&self, expected_generation: u64, credentials: &Credentials) -> bool {
        let mut inner = self.lock();
        if inner.generation != expected_generation {
            return false;
        }
        inner.credentials_present = true;
        inner.challenge_present = false;
        inner.grant = Some(credentials.clone());
        true
    }

    pub(crate) fn publish<T>(
        &self,
        generation: u64,
        cancel: &CancellationToken,
        credentials: Credentials,
        save: impl FnOnce(&Credentials) -> Result<T, McpError>,
    ) -> Result<T, McpError> {
        let mut inner = self.lock();
        if cancel.is_cancelled() {
            return Err(McpError::Cancelled);
        }
        if inner.generation != generation {
            return Err(McpError::McpAuthorityChanged);
        }
        let saved = save(&credentials)?;
        inner.generation = inner.generation.saturating_add(1);
        inner.pending = None;
        inner.challenge_present = false;
        inner.credentials_present = true;
        inner.grant = Some(credentials);
        Ok(saved)
    }

    pub(crate) fn grant(&self) -> Option<Credentials> {
        self.lock().grant.clone()
    }

    pub(crate) fn set_credentials_loaded(&self, loaded: bool) -> u64 {
        let mut inner = self.lock();
        if loaded {
            inner.generation = inner.generation.saturating_add(1);
        }
        inner.credentials_present = loaded;
        inner.generation
    }

    pub(crate) fn pending(&self) -> Challenge {
        self.lock().pending.clone().unwrap_or_default()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.lock().generation
    }

    pub(crate) fn authentication(&self, config: &McpServerConfig) -> AuthenticationState {
        let inner = self.lock();
        if inner.challenge_present {
            AuthenticationState::Required
        } else if inner.credentials_present {
            AuthenticationState::Authenticated
        } else if config.auth.is_some()
            || config.bearer_token_env.is_some()
            || !config.header_env.is_empty()
        {
            AuthenticationState::Configured
        } else {
            AuthenticationState::None
        }
    }

    fn lock(&self) -> MutexGuard<'_, AuthInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_contract::{McpAuthConfig, TransportType};

    fn grant() -> Credentials {
        Credentials {
            endpoint: "https://mcp.example/mcp".to_owned(),
            resource: "https://mcp.example/mcp".to_owned(),
            issuer: "https://issuer.example".to_owned(),
            client_id: "client".to_owned(),
            client_secret: None,
            access_token: zeroize::Zeroizing::new("access".to_owned()),
            refresh_token: None,
            scope: String::new(),
            token_type: "Bearer".to_owned(),
            token_endpoint_auth_method: "none".to_owned(),
            expires_at_ms: i64::MAX,
            authorization_endpoint: "https://issuer.example/authorize".to_owned(),
            token_endpoint: "https://issuer.example/token".to_owned(),
            revocation_endpoint: None,
        }
    }

    fn config(auth: bool) -> McpServerConfig {
        McpServerConfig {
            auth: auth.then(McpAuthConfig::default),
            ..McpServerConfig::remote("plain", TransportType::Http, "https://mcp.example/mcp")
        }
    }

    #[test]
    fn a_pending_challenge_overrides_stored_credentials() {
        let state = AuthState::default();
        assert_eq!(
            state.authentication(&config(false)),
            AuthenticationState::None
        );
        assert_eq!(
            state.authentication(&config(true)),
            AuthenticationState::Configured
        );
        state.set_credentials_loaded(true);
        assert_eq!(
            state.authentication(&config(false)),
            AuthenticationState::Authenticated
        );
        state.store_pending(Challenge {
            scope: Some("tools.call".to_owned()),
            ..Challenge::default()
        });
        assert_eq!(
            state.authentication(&config(false)),
            AuthenticationState::Required
        );
        assert_eq!(state.pending().scope.as_deref(), Some("tools.call"));
        assert!(state.refreshed(state.generation(), &grant()));
        assert_eq!(
            state.authentication(&config(false)),
            AuthenticationState::Authenticated
        );
    }

    #[test]
    fn rejected_credentials_mark_reauthentication_required() {
        let state = AuthState::default();
        state.set_credentials_loaded(true);
        let generation = state.generation();
        state.mark_reauthentication_required(generation);
        assert_eq!(
            state.authentication(&config(true)),
            AuthenticationState::Required
        );
        assert!(state.generation() > generation);
    }

    #[test]
    fn a_refresh_keeps_the_generation_and_a_newer_one_fences_stale_results() {
        let state = AuthState::default();
        state.set_credentials_loaded(true);
        let earlier = state.generation();
        assert!(state.refreshed(earlier, &grant()));
        assert_eq!(state.generation(), earlier);
        state.store_pending(Challenge::default());
        assert!(!state.refreshed(earlier, &grant()));
        assert_eq!(
            state.authentication(&config(true)),
            AuthenticationState::Required
        );
        assert!(state.refreshed(state.generation(), &grant()));
        state.mark_reauthentication_required(earlier);
        assert_eq!(
            state.authentication(&config(true)),
            AuthenticationState::Authenticated
        );
    }
}
