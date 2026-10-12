use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_auth::{GatewayKey, GatewaySource, valid_gateway_team};
use ofx_contract::{CapabilityLookup, CatalogRetry, ModelCatalog, ModelCatalogSource, ModelOption};
use ofx_gateway::{
    AccessMetadata, CatalogAccess, CatalogFailed, CatalogFailure, CatalogFetch, CatalogView,
    CreditsSnapshot, GatewayCreditsEndpoints, GatewayModel, GatewayModelCatalog,
    GatewayModelsEndpoints, GatewayProvider, PublicOnlyReason, fetch_credits,
};
use ofx_http::ClientError;
use ofx_trace::trace_log;
use tokio::sync::{Notify, OnceCell};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

const GATEWAY: &str = "gateway";

pub fn gateway_catalog_access(
    key: Option<&GatewayKey>,
    explicit: bool,
    now_ms: i64,
) -> CatalogAccess {
    let Some(key) = key else {
        return CatalogAccess::PublicOnly(PublicOnlyReason::NoCredential);
    };
    credential_access(
        key.source,
        &key.secret,
        key.gateway_team(),
        key.needs_refresh_at(now_ms),
        explicit,
    )
}

fn credential_access(
    source: GatewaySource,
    secret: &Zeroizing<String>,
    team: Option<&str>,
    due: bool,
    explicit: bool,
) -> CatalogAccess {
    let login = source == GatewaySource::FxLogin;
    if login && due {
        return CatalogAccess::PublicOnly(PublicOnlyReason::LoginRefreshRequired);
    }
    let login_team = match team {
        Some(team) if login && valid_gateway_team(team) => Some(team.to_owned()),
        _ if login => return CatalogAccess::PublicOnly(PublicOnlyReason::LoginTeamRequired),
        _ => None,
    };
    CatalogAccess::Authenticated {
        source: source.name(),
        credential: secret.clone(),
        login_team,
        explicit,
    }
}

pub(crate) enum CatalogCredential {
    HostManaged,
    Route {
        provider: Arc<GatewayProvider>,
        source: GatewaySource,
        explicit: bool,
    },
}

impl CatalogCredential {
    async fn access(&self, cancel: &CancellationToken) -> CatalogAccess {
        let Self::Route {
            provider,
            source,
            explicit,
        } = self
        else {
            return CatalogAccess::HostManaged;
        };
        match provider.current_access(cancel).await {
            Some(access) => credential_access(
                *source,
                access.token(),
                access.team(),
                access.due_at(now_ms()),
                *explicit,
            ),
            None => CatalogAccess::PublicOnly(PublicOnlyReason::NoCredential),
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

pub(crate) struct GatewayCatalog {
    catalog: GatewayModelCatalog,
    credits: GatewayCreditsEndpoints,
    user_agent: String,
    credential: CatalogCredential,
    listed: OnceCell<Listed>,
    ready: Notify,
}

pub(crate) struct Listed {
    models: Vec<GatewayModel>,
    note: &'static str,
}

impl GatewayCatalog {
    pub(crate) fn new(
        user_agent: &str,
        (endpoints, credits): (GatewayModelsEndpoints, GatewayCreditsEndpoints),
        credential: CatalogCredential,
    ) -> Result<Self, ClientError> {
        Ok(Self {
            catalog: GatewayModelCatalog::new(user_agent, endpoints)?,
            credits,
            user_agent: user_agent.to_owned(),
            credential,
            listed: OnceCell::new(),
            ready: Notify::new(),
        })
    }

    pub(crate) async fn credits(&self, cancel: &CancellationToken) -> CreditsSnapshot {
        let (credential, team) = match &self.credential {
            CatalogCredential::HostManaged => (None, None),
            CatalogCredential::Route {
                provider, source, ..
            } => match provider.current_access(cancel).await {
                Some(access) => {
                    let team = (*source == GatewaySource::FxLogin && !access.due_at(now_ms()))
                        .then(|| access.team().map(str::to_owned))
                        .flatten();
                    (Some(access.token().clone()), team)
                }
                None => (None, None),
            },
        };
        fetch_credits(
            &self.user_agent,
            &self.credits,
            credential.as_deref().map(String::as_str),
            team.as_deref(),
            cancel,
        )
        .await
    }

    pub(crate) fn cached(&self) -> Option<ModelCatalog> {
        self.listed.get().map(Listed::catalog)
    }

    pub(crate) async fn ready(&self) {
        let ready = self.ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();
        if self.listed.get().is_none() {
            ready.await;
        }
    }

    pub(crate) async fn catalog(&self) -> ModelCatalog {
        match self.listed(&CancellationToken::new()).await {
            Ok(listed) => listed.catalog(),
            Err(failed) => ModelCatalog::Failed {
                retry: retry(failed),
            },
        }
    }

    pub(crate) async fn resolve(
        &self,
        model: &str,
        cancel: &CancellationToken,
    ) -> CapabilityLookup {
        match self.listed(cancel).await {
            Ok(listed) => {
                let entry = listed.models.iter().find(|entry| entry.id == model);
                trace_log!(
                    GATEWAY,
                    "interactive model catalog lookup outcome={} model={model}",
                    if entry.is_some() {
                        "ready_hit"
                    } else {
                        "missing_entry"
                    }
                );
                CapabilityLookup::Resolved(
                    entry.map(GatewayModel::capabilities).unwrap_or_default(),
                )
            }
            Err(failed) if failed.failure == CatalogFailure::Cancellation => {
                trace_log!(
                    GATEWAY,
                    "interactive model catalog lookup outcome=cancelled model={model}"
                );
                CapabilityLookup::Cancelled
            }
            Err(_) => {
                trace_log!(
                    GATEWAY,
                    "interactive model catalog lookup outcome=cache_failed model={model}"
                );
                CapabilityLookup::CatalogUnavailable
            }
        }
    }

    async fn listed(&self, cancel: &CancellationToken) -> Result<&Listed, CatalogFailed> {
        let loading = self.listed.get_or_try_init(|| async {
            let access = self.credential.access(cancel).await;
            match self
                .catalog
                .load(&access, CatalogView::Picker, cancel)
                .await
            {
                CatalogFetch::Loaded { models, access, .. } => Ok(Listed {
                    models,
                    note: note(access),
                }),
                CatalogFetch::Failed { failure, .. } => Err(failure),
            }
        });
        let listed = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(CatalogFailed {
                failure: CatalogFailure::Cancellation,
                http_status: None,
                retryable: false,
            }),
            listed = loading => listed,
        }?;
        self.ready.notify_waiters();
        Ok(listed)
    }
}

impl Listed {
    fn catalog(&self) -> ModelCatalog {
        ModelCatalog::Listed {
            models: self
                .models
                .iter()
                .map(|model| ModelOption {
                    id: model.id.clone(),
                    capabilities: model.capabilities(),
                    max_output_tokens: model.max_output_tokens(),
                })
                .collect(),
            source: ModelCatalogSource::Gateway { note: self.note },
        }
    }
}

fn retry(failed: CatalogFailed) -> Option<CatalogRetry> {
    match failed.failure {
        CatalogFailure::RateLimited => Some(CatalogRetry::RateLimited),
        CatalogFailure::GatewayUnavailable { .. } | CatalogFailure::Transport
            if failed.retryable =>
        {
            Some(CatalogRetry::Unreachable)
        }
        _ => None,
    }
}

fn note(access: AccessMetadata) -> &'static str {
    if let Some(reason) = access.public_only {
        return match reason {
            PublicOnlyReason::NoCredential => {
                "Using the public model catalog; sign in or use an API key for team-private models."
            }
            PublicOnlyReason::LoginTeamRequired => {
                "Choose a Vercel team to load its private models."
            }
            PublicOnlyReason::LoginRefreshRequired => {
                "Vercel sign-in must refresh before team-private models can load."
            }
            PublicOnlyReason::CredentialRejected(_) => {
                "Your Gateway credential was rejected; using the public model catalog."
            }
        };
    }
    match access.source {
        Some("fx_login") => "Gateway catalog: authenticated with oh-fx login.",
        Some("ai_gateway_api_key") => "Note: Gateway catalog is authenticated with an API key",
        Some("vercel_oidc_token") => "Gateway catalog: authenticated with the Vercel session.",
        Some("stored_key") => "Gateway catalog: authenticated with the stored API key.",
        Some("host_managed") => "Provider catalog: authentication is managed by the host.",
        _ => "Using an authenticated AI Gateway catalog.",
    }
}

#[cfg(test)]
mod tests;
