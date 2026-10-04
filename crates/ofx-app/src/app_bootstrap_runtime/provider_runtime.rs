use std::env;
use std::sync::Arc;

use ofx_auth::{ChatGptOAuth, GrokOAuth};
use ofx_config::{ProviderDefinition, ProviderId, Settings};
use ofx_contract::{ModelCatalog, ModelProvider, Notice, NoticeTone};
use ofx_gateway::{CODEX_TITLE_MODEL, CodexReviewTransport};
use ofx_permissions::{DEFAULT_REVIEW_TIMEOUT, Reviewer};
use tokio_util::sync::CancellationToken;

use super::{
    AgentSetup, ConnectError, CredentialSource, Login, Profile, Route, Switchboard,
    connection_route, user_agent,
};
use crate::app_subagent_runtime::ChildRoute;
use crate::codex_provider::{CodexUnavailable, SignedOutProvider, SubscriptionEndpoints};
use crate::model_cache_runtime::ModelSource;

const PROVIDER_TOPIC: &str = "provider";
const AUTH_TOPIC: &str = "auth";
const CODEX_LABEL: &str = "Codex subscription";
const SWITCH_UNAVAILABLE: &str = "Provider switching is unavailable in this host.";
const SETTINGS_UNAVAILABLE: &str =
    "Could not load the saved provider selection. The current provider is unchanged.";
const PROVIDER_MISSING: &str =
    "The target provider catalog is unavailable. The current provider is unchanged.";
const CATALOG_UNAVAILABLE: &str =
    "Could not load the target provider catalog. The current provider is unchanged.";
const NO_MODELS: &str =
    "The target provider returned no supported models. The current provider is unchanged.";
const CODEX_LOGIN: &str = "Run oh-fx login codex, then try switching again.";

pub(crate) struct SwitchTarget {
    provider: ProviderId,
    profile: Profile,
}

pub(crate) fn provider_label(provider: &ProviderId) -> &str {
    match provider {
        ProviderId::Codex => CODEX_LABEL,
        provider => provider.label(),
    }
}

pub(crate) fn provider_names(settings: &Settings) -> Vec<String> {
    std::iter::once(ProviderId::Codex.label())
        .chain(settings.connections().iter().map(ProviderDefinition::id))
        .map(str::to_owned)
        .collect()
}

impl AgentSetup {
    pub(crate) fn chatgpt_oauth(&self) -> Option<ChatGptOAuth> {
        let endpoints = self.endpoints().chatgpt;
        ChatGptOAuth::new(
            self.preferences.as_ref()?.data.clone(),
            &user_agent(),
            endpoints,
        )
        .ok()
    }

    pub(crate) fn grok_oauth(&self) -> Option<GrokOAuth> {
        let endpoints = self.endpoints().grok;
        GrokOAuth::new(
            self.preferences.as_ref()?.data.clone(),
            &user_agent(),
            endpoints,
        )
        .ok()
    }

    fn endpoints(&self) -> SubscriptionEndpoints {
        self.switchboard
            .as_ref()
            .map(|switchboard| switchboard.endpoints.clone())
            .unwrap_or_default()
    }

    pub(crate) fn listed_providers(&self) -> Vec<String> {
        let Some(switchboard) = &self.switchboard else {
            return Vec::new();
        };
        match switchboard.profile.reloaded() {
            Some(profile) => provider_names(&profile.settings),
            None => provider_names(&switchboard.profile.settings),
        }
    }

    pub(crate) fn switch_target(&self, provider: ProviderId) -> Result<SwitchTarget, Notice> {
        let switchboard = self
            .switchboard
            .as_ref()
            .ok_or_else(|| Notice::new(NoticeTone::Warning, PROVIDER_TOPIC, SWITCH_UNAVAILABLE))?;
        let profile = switchboard
            .profile
            .reloaded()
            .ok_or_else(|| refused(SETTINGS_UNAVAILABLE))?;
        let available = match &provider {
            ProviderId::Codex => true,
            ProviderId::Configured(id) => profile.settings.connection(id).is_some(),
            ProviderId::Gateway | ProviderId::Grok => false,
        };
        if !available {
            return Err(refused(PROVIDER_MISSING));
        }
        Ok(SwitchTarget { provider, profile })
    }

    pub(crate) async fn route_for(
        &self,
        target: &SwitchTarget,
        keep: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Route, Notice> {
        let lookup = |name: &str| env::var(name).ok();
        let settings = &target.profile.settings;
        let ProviderId::Configured(id) = &target.provider else {
            let preferred = settings.selected_codex_model(None, &lookup).ok();
            let endpoints = self.endpoints();
            let mut route = target
                .profile
                .subscription_route(
                    String::new(),
                    preferred.clone(),
                    endpoints,
                    self.refreshes.clone(),
                    cancel,
                )
                .await
                .map_err(|error| failure(&error))?;
            let ModelCatalog::Listed { models, .. } = route.models.catalog().await else {
                return Err(refused(CATALOG_UNAVAILABLE));
            };
            let ids: Vec<String> = models.into_iter().map(|option| option.id).collect();
            route.model = kept_model(&ids, keep, preferred.as_deref())
                .filter(|_| !ids.is_empty())
                .ok_or_else(|| refused(NO_MODELS))?;
            return Ok(route);
        };
        let connection = settings
            .connection(id)
            .ok_or_else(|| refused(PROVIDER_MISSING))?;
        let preferred = settings.selected_model(connection, None, &lookup).ok();
        let model = kept_model(connection.models(), keep, preferred.as_deref())
            .ok_or_else(|| refused(NO_MODELS))?;
        connection_route(connection, Ok(model), preferred).map_err(|error| failure(&error))
    }

    pub(crate) fn sign_out(&self, model: &str) -> Route {
        if let Some(login) = &self.subscription {
            login.sign_out();
        }
        Route::signed_out(model.to_owned(), self.configured_model.clone())
    }

    pub(crate) fn adopt(&mut self, route: Route) -> (Arc<dyn ModelProvider>, ModelSource) {
        if route.uses_tls {
            ofx_http::warm_tls_roots();
        }
        self.delegation.children.reroute(route.children());
        self.permissions
            .set_reviewer(Reviewer::new(route.reviewer, DEFAULT_REVIEW_TIMEOUT));
        self.provider = route.provider;
        self.title_model = route.title_model;
        self.login = route.login;
        self.models = route.models;
        self.connection = route.connection;
        self.source = route.source;
        self.account_id = route.account_id;
        self.subscription = route.subscription;
        self.configured_model = route.configured_model;
        self.config.model = route.model;
        (Arc::clone(&self.provider), self.models.clone())
    }
}

impl Route {
    pub(super) fn signed_out(model: String, configured_model: Option<String>) -> Self {
        let provider: Arc<dyn ModelProvider> = Arc::new(SignedOutProvider);
        Self {
            reviewer: Arc::new(CodexReviewTransport::new(Arc::clone(&provider))),
            title_model: Some(CODEX_TITLE_MODEL),
            provider,
            models: ModelSource::Unavailable,
            connection: None,
            model,
            configured_model,
            source: CredentialSource::Codex,
            account_id: None,
            subscription: None,
            uses_tls: false,
            login: Login::Missing,
        }
    }

    pub(super) fn children(&self) -> ChildRoute {
        ChildRoute {
            provider: Arc::clone(&self.provider),
            capabilities: Arc::new(self.models.clone()),
            connection: self.connection.clone(),
            reviewer: Arc::clone(&self.reviewer),
        }
    }
}

impl SwitchTarget {
    pub(crate) fn provider(&self) -> &ProviderId {
        &self.provider
    }
}

impl Profile {
    pub(super) fn switchboard(&self, endpoints: SubscriptionEndpoints) -> Switchboard {
        Switchboard {
            profile: self.clone(),
            endpoints,
        }
    }

    fn reloaded(&self) -> Option<Self> {
        let Some(paths) = &self.paths else {
            return Some(self.clone());
        };
        let settings = Settings::load(paths, &self.workspace_root).ok()?;
        Self::new(
            self.workspace_root.clone(),
            self.home.clone(),
            self.paths.clone(),
            settings,
        )
        .ok()
    }
}

fn listed_model(listed: &[String], preferred: Option<&str>) -> Option<String> {
    match preferred {
        Some(model) if listed.is_empty() || listed.iter().any(|id| id == model) => {
            Some(model.to_owned())
        }
        _ => listed.first().cloned(),
    }
}

fn kept_model(listed: &[String], keep: Option<&str>, preferred: Option<&str>) -> Option<String> {
    keep.filter(|model| listed.iter().any(|id| id == model))
        .map(str::to_owned)
        .or_else(|| listed_model(listed, preferred))
}

fn refused(body: &str) -> Notice {
    Notice::new(NoticeTone::Error, PROVIDER_TOPIC, body)
}

fn failure(error: &ConnectError) -> Notice {
    match error {
        ConnectError::Codex(CodexUnavailable::MissingLogin) => {
            Notice::new(NoticeTone::Warning, PROVIDER_TOPIC, CODEX_LOGIN)
        }
        ConnectError::Codex(_) | ConnectError::Connection(_) => {
            Notice::new(NoticeTone::Error, AUTH_TOPIC, error.to_string())
        }
        error => refused(&error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(models: &[&str]) -> Vec<String> {
        models.iter().map(|model| (*model).to_owned()).collect()
    }

    #[test]
    fn a_switch_keeps_the_saved_model_when_the_target_lists_it_and_else_its_first() {
        let listed = ids(&["a", "b"]);
        assert_eq!(listed_model(&listed, Some("b")), Some("b".to_owned()));
        assert_eq!(listed_model(&listed, Some("gone")), Some("a".to_owned()));
        assert_eq!(listed_model(&listed, None), Some("a".to_owned()));
        assert_eq!(listed_model(&[], Some("own")), Some("own".to_owned()));
        assert_eq!(listed_model(&[], None), None);
    }

    #[test]
    fn a_restored_login_keeps_a_listed_session_model_and_else_falls_back_as_a_switch_does() {
        let listed = ids(&["a", "b", "c"]);
        assert_eq!(
            kept_model(&listed, Some("c"), Some("b")),
            Some("c".to_owned())
        );
        assert_eq!(
            kept_model(&listed, Some("gone"), Some("b")),
            Some("b".to_owned())
        );
        assert_eq!(kept_model(&listed, None, Some("b")), Some("b".to_owned()));
        assert_eq!(
            kept_model(&listed, Some("gone"), None),
            Some("a".to_owned())
        );
    }

    #[test]
    fn a_missing_codex_login_names_its_repair_and_providers_name_themselves() {
        let codex = failure(&ConnectError::Codex(CodexUnavailable::MissingLogin));
        assert_eq!(
            (codex.tone, codex.topic.as_str(), codex.body.as_str()),
            (NoticeTone::Warning, "provider", CODEX_LOGIN)
        );
        assert_eq!(provider_label(&ProviderId::Codex), "Codex subscription");
        assert_eq!(
            provider_label(&ProviderId::Configured("portkey".to_owned())),
            "portkey"
        );
    }
}
