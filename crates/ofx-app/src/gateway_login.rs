use std::sync::Arc;

use ofx_auth::{GatewayKey, VercelError, VercelOAuth};
use ofx_contract::BoxFuture;
use ofx_gateway::{GatewayAccess, GatewayCredential, GatewayRefresh};
use ofx_trace::trace_log;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::codex_provider::DetachedRefreshes;

const AUTH: &str = "auth";

struct LoginRefresh {
    login: VercelOAuth,
    detached: Option<Arc<DetachedRefreshes>>,
}

impl GatewayRefresh for LoginRefresh {
    fn refresh<'a>(
        &'a self,
        forced: bool,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<GatewayAccess>, String>> {
        Box::pin(async move {
            let Some(detached) = &self.detached else {
                return access(load(&self.login, forced, cancel).await, forced);
            };
            let Some((running, stopped)) = detached.start_login() else {
                return Ok(None);
            };
            let (finished, loaded) = oneshot::channel();
            let login = self.login.clone();
            tokio::spawn(async move {
                let loading = load(&login, forced, &stopped).await;
                drop(running);
                let _ = finished.send(loading);
            });
            tokio::select! {
                biased;
                loaded = loaded => loaded.map_or(Ok(None), |loaded| access(loaded, forced)),
                () = cancel.cancelled() => Ok(None),
            }
        })
    }
}

async fn load(
    login: &VercelOAuth,
    forced: bool,
    cancel: &CancellationToken,
) -> Result<Option<GatewayKey>, VercelError> {
    if forced {
        login.refresh_credential(cancel).await
    } else {
        login.load_credential(cancel).await
    }
}

fn access(
    loaded: Result<Option<GatewayKey>, VercelError>,
    forced: bool,
) -> Result<Option<GatewayAccess>, String> {
    match loaded {
        Ok(key) => Ok(key.map(|key| {
            GatewayAccess::new(
                key.secret.to_string(),
                key.gateway_team().map(str::to_owned),
                key.refresh_after_ms.unwrap_or(i64::MAX),
            )
        })),
        Err(VercelError::Cancelled) => Ok(None),
        Err(error) => {
            trace_log!(
                AUTH,
                "credential refresh provider failed source=fx_login mode={} err={error}",
                if forced { "force" } else { "if_needed" }
            );
            Err(error.to_string())
        }
    }
}

pub(crate) fn gateway_credential(
    key: &GatewayKey,
    login: Option<VercelOAuth>,
    detached: Option<Arc<DetachedRefreshes>>,
) -> GatewayCredential {
    let credential = GatewayCredential::new(
        Some(key.secret.to_string()),
        key.gateway_team().map(str::to_owned),
    );
    match (key.source.refreshable(), key.refresh_after_ms, login) {
        (true, Some(refresh_after_ms), Some(login)) => {
            credential.refreshed_by(refresh_after_ms, Arc::new(LoginRefresh { login, detached }))
        }
        _ => credential,
    }
}

#[cfg(test)]
mod tests;
