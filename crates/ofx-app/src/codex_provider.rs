use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

use ofx_auth::{
    CHATGPT_REFRESH_LIMIT, ChatGptAccess, ChatGptEndpoints, ChatGptOAuth, GrokEndpoints,
    MISSING_CHATGPT_CREDENTIAL_MESSAGE, PreparationError, RefreshMode, prepare_chatgpt_credential,
    refresh_chatgpt_credential,
};
use ofx_config::ProfilePaths;
use ofx_contract::{
    BoxFuture, CapabilityLookup, CapabilityResolver, Completion, ModelCapabilities, ModelProvider,
    ModelRequest, ProviderError, ProviderErrorKind, ProviderReplay, StreamSink,
};
use ofx_gateway::{
    CatalogCredential, CatalogFailure, CodexAccess, CodexCredentials, CodexEndpoints, CodexModel,
    CodexModelCatalog, CodexModelsEndpoints, CodexProvider, CodexRefresh,
};
use ofx_http::ClientError;
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
pub struct SubscriptionEndpoints {
    pub chatgpt: ChatGptEndpoints,
    pub codex: CodexEndpoints,
    pub models: CodexModelsEndpoints,
    pub grok: GrokEndpoints,
}

pub(crate) struct SignedOutProvider;

impl ModelProvider for SignedOutProvider {
    fn stream<'a>(
        &'a self,
        _request: &'a ModelRequest<'a>,
        _sink: &'a mut dyn StreamSink,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        missing_credentials()
    }
}

#[derive(Debug, Default)]
pub(crate) struct SubscriptionLogin {
    signed_out: AtomicBool,
}

impl SubscriptionLogin {
    pub(crate) fn sign_out(&self) {
        self.signed_out.store(true, Ordering::Release);
    }

    fn signed_out(&self) -> bool {
        self.signed_out.load(Ordering::Acquire)
    }
}

pub(crate) struct SubscriptionProvider {
    codex: CodexProvider,
    login: Arc<SubscriptionLogin>,
}

impl SubscriptionProvider {
    pub(crate) fn new(codex: CodexProvider, login: Arc<SubscriptionLogin>) -> Self {
        Self { codex, login }
    }
}

impl ModelProvider for SubscriptionProvider {
    fn stream<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        if self.login.signed_out() {
            return missing_credentials();
        }
        self.codex.stream(request, sink, cancel)
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        self.codex.request_body(request)
    }

    fn stream_body<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        body: String,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        if self.login.signed_out() {
            return missing_credentials();
        }
        self.codex.stream_body(request, body, sink, cancel)
    }

    fn project_replay(
        &self,
        replay: &ProviderReplay,
        text: bool,
        reasoning: bool,
    ) -> Result<Option<ProviderReplay>, ProviderError> {
        self.codex.project_replay(replay, text, reasoning)
    }
}

fn missing_credentials<'a>() -> BoxFuture<'a, Result<Completion, ProviderError>> {
    Box::pin(async {
        Err(ProviderError::new(
            ProviderErrorKind::Unauthorized,
            "MissingCredentials",
        ))
    })
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
    pub(crate) account_id: String,
    pub(crate) login: Arc<SubscriptionLogin>,
}

pub(crate) struct CatalogCapabilities {
    user_agent: String,
    endpoints: CodexModelsEndpoints,
    cache_directory: PathBuf,
    credential: CatalogCredential,
    login: Arc<SubscriptionLogin>,
    listed: OnceLock<Vec<CodexModel>>,
    ready: Notify,
}

impl CatalogCapabilities {
    pub(crate) fn cached(&self) -> Option<&[CodexModel]> {
        self.listed.get().map(Vec::as_slice)
    }

    pub(crate) async fn ready(&self) {
        let ready = self.ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();
        if self.listed.get().is_none() {
            ready.await;
        }
    }

    pub(crate) async fn listed(
        &self,
        cancel: &CancellationToken,
    ) -> Result<&[CodexModel], CatalogFailure> {
        if let Some(listed) = self.listed.get() {
            return Ok(listed);
        }
        if self.login.signed_out() {
            return Err(CatalogFailure::Authentication);
        }
        let catalog = CodexModelCatalog::new(
            &self.user_agent,
            self.endpoints.clone(),
            Some(self.cache_directory.clone()),
        )
        .map_err(|_| CatalogFailure::Transport)?;
        let models = catalog.fetch(Some(&self.credential), cancel).await?;
        let listed = self.listed.get_or_init(|| models);
        self.ready.notify_waiters();
        Ok(listed)
    }
}

impl CapabilityResolver for CatalogCapabilities {
    fn resolve<'a>(
        &'a self,
        model: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, CapabilityLookup> {
        Box::pin(async move {
            match self.listed(cancel).await {
                Ok(listed) => CapabilityLookup::Resolved(listed_capabilities(listed, model)),
                Err(CatalogFailure::Cancellation) => CapabilityLookup::Cancelled,
                Err(_) => CapabilityLookup::CatalogUnavailable,
            }
        })
    }
}

fn listed_capabilities(listed: &[CodexModel], model: &str) -> ModelCapabilities {
    listed
        .iter()
        .find(|listed| listed.id == model)
        .map(|listed| listed.capabilities.clone())
        .unwrap_or_default()
}

#[derive(Debug, Default)]
pub(crate) struct DetachedRefreshes {
    state: Mutex<Refreshes>,
    drained: Condvar,
    settled: Notify,
}

#[derive(Debug, Default)]
struct Refreshes {
    running: usize,
    latest_start: Option<Instant>,
    closed: bool,
}

impl DetachedRefreshes {
    fn state(&self) -> MutexGuard<'_, Refreshes> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start(self: &Arc<Self>) -> Option<RunningRefresh> {
        let mut state = self.state();
        if state.closed {
            return None;
        }
        state.running += 1;
        state.latest_start = Some(Instant::now());
        Some(RunningRefresh(Arc::clone(self)))
    }

    pub(crate) fn pending(&self) -> bool {
        self.state().running > 0
    }

    pub(crate) fn close(&self) {
        self.state().closed = true;
    }

    pub(crate) fn wait_for_running(&self) -> bool {
        let mut state = self.state();
        let Some(deadline) = state
            .latest_start
            .map(|started| started + CHATGPT_REFRESH_LIMIT)
        else {
            return false;
        };
        let mut waited = false;
        while state.running > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            waited = true;
            state = self
                .drained
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        waited
    }

    pub(crate) async fn settle(&self) {
        loop {
            let settled = self.settled.notified();
            if !self.pending() {
                return;
            }
            settled.await;
        }
    }
}

struct RunningRefresh(Arc<DetachedRefreshes>);

impl Drop for RunningRefresh {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.running -= 1;
        if state.running == 0 {
            self.0.drained.notify_all();
            self.0.settled.notify_waiters();
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
            let running = detached.start()?;
            let (finished, refreshed) = oneshot::channel();
            let oauth = Arc::clone(&self.oauth);
            let account_id = account_id.to_owned();
            tokio::spawn(async move {
                let never = CancellationToken::new();
                let access = refresh_chatgpt_credential(&oauth, mode, &account_id, &never).await;
                drop(running);
                let _ = finished.send(access.ok().flatten());
            });
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
    let login = Arc::new(SubscriptionLogin::default());
    let capabilities = CatalogCapabilities {
        user_agent: user_agent.to_owned(),
        endpoints: endpoints.models,
        cache_directory: paths.cache.clone(),
        credential: CatalogCredential::new(token.clone(), account_id.clone()),
        login: Arc::clone(&login),
        listed: OnceLock::new(),
        ready: Notify::new(),
    };
    let access = CodexAccess::new(token, account_id.clone(), refresh_after_ms);
    let credentials = Arc::new(SubscriptionCredentials {
        oauth: Arc::new(oauth),
        detached,
    });
    let provider = CodexProvider::new(access, credentials, user_agent, endpoints.codex)
        .map_err(CodexUnavailable::Client)?;
    Ok(CodexSubscription {
        provider,
        capabilities,
        account_id,
        login,
    })
}

fn codex_access(access: ChatGptAccess) -> CodexAccess {
    let account_id = access.account_id().to_owned();
    let refresh_after_ms = access.refresh_after_ms();
    CodexAccess::new(access.into_token(), account_id, refresh_after_ms)
}

#[cfg(test)]
mod tests;
