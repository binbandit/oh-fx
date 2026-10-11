use super::AuthState;
use crate::error::McpError;
use crate::mcp_auth::{Challenge, Credentials};

pub(crate) struct DetachedAuth {
    grant: Option<Credentials>,
    pending: Option<Challenge>,
    challenge_present: bool,
    credentials_present: bool,
}

impl DetachedAuth {
    pub(crate) fn grant(&self) -> Option<&Credentials> {
        self.grant.as_ref()
    }
}

impl AuthState {
    pub(crate) async fn begin_logout(&self) -> Result<DetachedAuth, McpError> {
        let _publishing = self.publication().await;
        let mut inner = self.lock();
        if inner.logging_out {
            return Err(McpError::McpAuthorityChanged);
        }
        inner.logging_out = true;
        let detached = DetachedAuth {
            grant: inner.grant.take(),
            pending: inner.pending.take(),
            challenge_present: inner.challenge_present,
            credentials_present: inner.credentials_present,
        };
        inner.challenge_present = false;
        inner.credentials_present = false;
        Ok(detached)
    }

    pub(crate) fn restore_after_logout(&self, detached: DetachedAuth) {
        let mut inner = self.lock();
        inner.grant = detached.grant;
        inner.pending = detached.pending;
        inner.challenge_present = detached.challenge_present;
        inner.credentials_present = detached.credentials_present;
        inner.discarded.clear();
        inner.logging_out = false;
    }

    pub(crate) fn commit_logout(&self) -> Vec<Credentials> {
        let mut inner = self.lock();
        inner.generation = inner.generation.saturating_add(1);
        inner.logout_epoch = inner.logout_epoch.saturating_add(1);
        std::mem::take(&mut inner.discarded)
    }

    pub(crate) fn finish_logout(&self) {
        self.lock().logging_out = false;
    }
}
