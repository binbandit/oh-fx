use std::path::PathBuf;
use std::sync::Arc;

use ofx_auth::{
    ChatGptAccess, ChatGptEndpoints, ChatGptOAuth, PreparationError, RefreshMode,
    prepare_chatgpt_credential, refresh_chatgpt_credential,
};
use ofx_config::ProfilePaths;
use ofx_contract::{BoxFuture, CapabilityLookup, CapabilityResolver};
use ofx_gateway::{
    CatalogCredential, CatalogFailure, CodexAccess, CodexCredentials, CodexEndpoints,
    CodexModelCatalog, CodexModelsEndpoints, CodexProvider, CodexRefresh,
};
use ofx_http::ClientError;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
pub(crate) struct SubscriptionEndpoints {
    pub(crate) chatgpt: ChatGptEndpoints,
    pub(crate) codex: CodexEndpoints,
    pub(crate) models: CodexModelsEndpoints,
}

#[derive(Debug)]
pub(crate) enum CodexUnavailable {
    MissingLogin,
    Preparation(PreparationError),
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

struct SubscriptionCredentials {
    oauth: ChatGptOAuth,
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
            refresh_chatgpt_credential(&self.oauth, mode, account_id, cancel)
                .await
                .ok()
                .flatten()
                .map(codex_access)
        })
    }
}

pub(crate) async fn codex_subscription(
    paths: Option<&ProfilePaths>,
    user_agent: &str,
    endpoints: SubscriptionEndpoints,
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
    let credentials = Arc::new(SubscriptionCredentials { oauth });
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
