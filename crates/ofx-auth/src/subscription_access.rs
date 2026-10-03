use crate::secret::Secret;
use std::time::{SystemTime, UNIX_EPOCH};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionAccess {
    pub(crate) access_token: Secret,
    pub(crate) account_id: String,
    pub(crate) refresh_after_ms: i64,
}

impl SubscriptionAccess {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn refresh_after_ms(&self) -> i64 {
        self.refresh_after_ms
    }

    pub fn into_token(self) -> String {
        self.access_token.into_inner()
    }

    pub(crate) fn access_token(&self) -> &str {
        self.access_token.expose()
    }
}

pub(crate) fn refresh_deadline_ms(expires_at_ms: i64) -> i64 {
    expires_at_ms.saturating_sub(60_000).max(0)
}
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}
