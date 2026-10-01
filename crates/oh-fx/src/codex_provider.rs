use std::path::PathBuf;
use std::sync::Arc;

use ofx_auth::{
    ChatGptAccess, ChatGptEndpoints, ChatGptOAuth, PreparationError, RefreshMode,
    prepare_chatgpt_credential, refresh_chatgpt_credential,
};
use ofx_contract::BoxFuture;
use ofx_gateway::{CodexAccess, CodexCredentials, CodexEndpoints, CodexProvider, CodexRefresh};
use ofx_http::ClientError;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
pub(crate) struct SubscriptionEndpoints {
    pub(crate) chatgpt: ChatGptEndpoints,
    pub(crate) codex: CodexEndpoints,
}

#[derive(Debug)]
pub(crate) enum CodexUnavailable {
    MissingLogin,
    Preparation(PreparationError),
    Client(ClientError),
}

struct SubscriptionCredentials {
    oauth: ChatGptOAuth,
}

impl CodexCredentials for SubscriptionCredentials {
    fn refresh<'a>(
        &'a self,
        mode: CodexRefresh,
        account_id: &'a str,
    ) -> BoxFuture<'a, Option<CodexAccess>> {
        let mode = match mode {
            CodexRefresh::IfNeeded => RefreshMode::IfNeeded,
            CodexRefresh::Force => RefreshMode::Force,
        };
        Box::pin(async move {
            refresh_chatgpt_credential(&self.oauth, mode, account_id)
                .await
                .ok()
                .flatten()
                .map(codex_access)
        })
    }
}

pub(crate) async fn codex_provider(
    data_directory: Option<PathBuf>,
    user_agent: &str,
    endpoints: SubscriptionEndpoints,
    cancel: &CancellationToken,
) -> Result<CodexProvider, CodexUnavailable> {
    let data_directory = data_directory.ok_or(CodexUnavailable::Preparation(
        PreparationError::CredentialStorageUnavailable,
    ))?;
    let oauth = ChatGptOAuth::new(data_directory, user_agent, endpoints.chatgpt).map_err(|_| {
        CodexUnavailable::Preparation(PreparationError::CredentialTemporarilyUnavailable)
    })?;
    let access = prepare_chatgpt_credential(&oauth, cancel)
        .await
        .map_err(CodexUnavailable::Preparation)?
        .ok_or(CodexUnavailable::MissingLogin)?;
    let credentials = Arc::new(SubscriptionCredentials { oauth });
    CodexProvider::new(
        codex_access(access),
        credentials,
        user_agent,
        endpoints.codex,
    )
    .map_err(CodexUnavailable::Client)
}

fn codex_access(access: ChatGptAccess) -> CodexAccess {
    let account_id = access.account_id().to_owned();
    let refresh_after_ms = access.refresh_after_ms();
    CodexAccess::new(access.into_token(), account_id, refresh_after_ms)
}

#[cfg(test)]
mod tests;
