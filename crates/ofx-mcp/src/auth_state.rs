use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::health::AuthenticationState;
use crate::mcp_auth::Challenge;
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

    pub(crate) fn credentials_installed(&self) {
        let mut inner = self.lock();
        inner.generation = inner.generation.saturating_add(1);
        inner.credentials_present = true;
        inner.challenge_present = false;
    }

    pub(crate) fn set_credentials_loaded(&self, loaded: bool) {
        self.lock().credentials_present = loaded;
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
        state.credentials_installed();
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
    fn a_newer_auth_generation_suppresses_a_stale_reauthentication_mark() {
        let state = AuthState::default();
        state.set_credentials_loaded(true);
        let stale = state.generation();
        state.credentials_installed();
        state.mark_reauthentication_required(stale);
        assert_eq!(
            state.authentication(&config(true)),
            AuthenticationState::Authenticated
        );
    }
}
