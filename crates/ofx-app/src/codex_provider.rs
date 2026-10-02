use std::mem;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use ofx_auth::{
    ChatGptAccess, ChatGptEndpoints, ChatGptOAuth, MISSING_CHATGPT_CREDENTIAL_MESSAGE,
    PreparationError, RefreshMode, prepare_chatgpt_credential, refresh_chatgpt_credential,
};
use ofx_config::ProfilePaths;
use ofx_contract::{BoxFuture, CapabilityLookup, CapabilityResolver};
use ofx_gateway::{
    CatalogCredential, CatalogFailure, CodexAccess, CodexCredentials, CodexEndpoints,
    CodexModelCatalog, CodexModelsEndpoints, CodexProvider, CodexRefresh,
};
use ofx_http::ClientError;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
pub struct SubscriptionEndpoints {
    pub chatgpt: ChatGptEndpoints,
    pub codex: CodexEndpoints,
    pub models: CodexModelsEndpoints,
}

#[derive(Debug, thiserror::Error)]
pub enum CodexUnavailable {
    #[error("{MISSING_CHATGPT_CREDENTIAL_MESSAGE}")]
    MissingLogin,
    #[error("{}", .0.notice())]
    Preparation(PreparationError),
    #[error("{0}")]
    Client(ClientError),
}

pub(crate) struct CodexSubscription {
    pub(crate) provider: CodexProvider,
    pub(crate) capabilities: CatalogCapabilities,
}

pub(crate) struct CatalogCapabilities {
    user_agent: String,
    endpoints: CodexModelsEndpoints,
    cache_directory: PathBuf,
    credential: CatalogCredential,
}

impl CapabilityResolver for CatalogCapabilities {
    fn resolve<'a>(
        &'a self,
        model: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, CapabilityLookup> {
        Box::pin(async move {
            let Ok(catalog) = CodexModelCatalog::new(
                &self.user_agent,
                self.endpoints.clone(),
                Some(self.cache_directory.clone()),
            ) else {
                return CapabilityLookup::CatalogUnavailable;
            };
            match catalog.fetch(Some(&self.credential), cancel).await {
                Ok(models) => CapabilityLookup::Resolved(
                    models
                        .into_iter()
                        .find(|listed| listed.id == model)
                        .map(|listed| listed.capabilities)
                        .unwrap_or_default(),
                ),
                Err(CatalogFailure::Cancellation) => CapabilityLookup::Cancelled,
                Err(_) => CapabilityLookup::CatalogUnavailable,
            }
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct DetachedRefreshes {
    running: Mutex<Vec<JoinHandle<()>>>,
}

impl DetachedRefreshes {
    fn track(&self, refresh: JoinHandle<()>) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.retain(|running| !running.is_finished());
        running.push(refresh);
    }

    pub(crate) async fn settle(&self) {
        let running = mem::take(&mut *self.running.lock().unwrap_or_else(PoisonError::into_inner));
        for refresh in running {
            let _ = refresh.await;
        }
    }
}

struct SubscriptionCredentials {
    oauth: Arc<ChatGptOAuth>,
    detached: Option<Arc<DetachedRefreshes>>,
}

impl CodexCredentials for SubscriptionCredentials {
    fn refresh<'a>(
        &'a self,
        mode: CodexRefresh,
        account_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<CodexAccess>> {
        let mode = match mode {
            CodexRefresh::IfNeeded => RefreshMode::IfNeeded,
            CodexRefresh::Force => RefreshMode::Force,
        };
        Box::pin(async move {
            let Some(detached) = &self.detached else {
                return refresh_chatgpt_credential(&self.oauth, mode, account_id, cancel)
                    .await
                    .ok()
                    .flatten()
                    .map(codex_access);
            };
            let (finished, refreshed) = oneshot::channel();
            let oauth = Arc::clone(&self.oauth);
            let account_id = account_id.to_owned();
            detached.track(tokio::spawn(async move {
                let never = CancellationToken::new();
                let access = refresh_chatgpt_credential(&oauth, mode, &account_id, &never).await;
                let _ = finished.send(access.ok().flatten());
            }));
            tokio::select! {
                biased;
                refreshed = refreshed => refreshed.ok().flatten().map(codex_access),
                () = cancel.cancelled() => None,
            }
        })
    }
}

pub(crate) async fn codex_subscription(
    paths: Option<&ProfilePaths>,
    user_agent: &str,
    endpoints: SubscriptionEndpoints,
    detached: Option<Arc<DetachedRefreshes>>,
    cancel: &CancellationToken,
) -> Result<CodexSubscription, CodexUnavailable> {
    let paths = paths.ok_or(CodexUnavailable::Preparation(
        PreparationError::CredentialStorageUnavailable,
    ))?;
    let oauth =
        ChatGptOAuth::new(paths.data.clone(), user_agent, endpoints.chatgpt).map_err(|_| {
            CodexUnavailable::Preparation(PreparationError::CredentialTemporarilyUnavailable)
        })?;
    let access = prepare_chatgpt_credential(&oauth, cancel)
        .await
        .map_err(CodexUnavailable::Preparation)?
        .ok_or(CodexUnavailable::MissingLogin)?;
    let account_id = access.account_id().to_owned();
    let refresh_after_ms = access.refresh_after_ms();
    let token = access.into_token();
    let capabilities = CatalogCapabilities {
        user_agent: user_agent.to_owned(),
        endpoints: endpoints.models,
        cache_directory: paths.cache.clone(),
        credential: CatalogCredential::new(token.clone(), account_id.clone()),
    };
    let access = CodexAccess::new(token, account_id, refresh_after_ms);
    let credentials = Arc::new(SubscriptionCredentials {
        oauth: Arc::new(oauth),
        detached,
    });
    let provider = CodexProvider::new(access, credentials, user_agent, endpoints.codex)
        .map_err(CodexUnavailable::Client)?;
    Ok(CodexSubscription {
        provider,
        capabilities,
    })
}

fn codex_access(access: ChatGptAccess) -> CodexAccess {
    let account_id = access.account_id().to_owned();
    let refresh_after_ms = access.refresh_after_ms();
    CodexAccess::new(access.into_token(), account_id, refresh_after_ms)
}

#[cfg(test)]
mod tests;
