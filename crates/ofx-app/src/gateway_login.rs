use std::sync::Arc;

use ofx_auth::{GatewayKey, VercelOAuth};
use ofx_contract::BoxFuture;
use ofx_gateway::{GatewayAccess, GatewayCredential, GatewayRefresh};
use ofx_trace::trace_log;

const AUTH: &str = "auth";

struct LoginRefresh {
    login: VercelOAuth,
}

impl GatewayRefresh for LoginRefresh {
    fn refresh(&self) -> BoxFuture<'_, Result<Option<GatewayAccess>, String>> {
        Box::pin(async move {
            match self.login.load_credential().await {
                Ok(key) => Ok(key.map(|key| {
                    GatewayAccess::new(
                        key.secret.to_string(),
                        key.gateway_team().map(str::to_owned),
                        key.refresh_after_ms.unwrap_or(i64::MAX),
                    )
                })),
                Err(error) => {
                    trace_log!(
                        AUTH,
                        "credential refresh provider failed source=fx_login mode=if_needed err={error}"
                    );
                    Err(error.to_string())
                }
            }
        })
    }
}

pub(crate) fn gateway_credential(
    key: &GatewayKey,
    login: Option<VercelOAuth>,
) -> GatewayCredential {
    let credential = GatewayCredential::new(
        Some(key.secret.to_string()),
        key.gateway_team().map(str::to_owned),
    );
    match (key.source.refreshable(), key.refresh_after_ms, login) {
        (true, Some(refresh_after_ms), Some(login)) => {
            credential.refreshed_by(refresh_after_ms, Arc::new(LoginRefresh { login }))
        }
        _ => credential,
    }
}
