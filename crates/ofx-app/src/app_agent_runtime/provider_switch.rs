use std::sync::Arc;

use ofx_config::{ProviderId, save_provider_model};
use ofx_contract::{CapabilityResolver, Notice, NoticeTone, UiEvent};
use tokio_util::sync::CancellationToken;

use super::{Controller, ControllerState};
use crate::app_bootstrap_runtime::{Login, Route, provider_label};
use crate::app_session_runtime::session_route;
use crate::user_settings;

const PROVIDER_TOPIC: &str = "provider";
const PROVIDER_BUSY: &str =
    "Provider switching is unavailable until active and queued work finishes.";
const PROVIDER_MISSING: &str =
    "The target provider catalog is unavailable. The current provider is unchanged.";
const UNSAVED: &str = "Provider switched for this run, but the selection could not be saved.";

impl ControllerState {
    pub(crate) fn provider_busy(&self) {
        self.notice(NoticeTone::Neutral, PROVIDER_TOPIC, PROVIDER_BUSY);
    }

    pub(crate) fn open_provider_picker(&self, prefix: &str) {
        self.emit(UiEvent::ProviderPicker {
            prefix: prefix.to_owned(),
            providers: self.setup.listed_providers(),
        });
    }
}

impl Controller {
    pub(super) async fn select_provider(&mut self, name: &str) {
        let Some(target) = ProviderId::parse(name) else {
            return self
                .state
                .notice(NoticeTone::Error, PROVIDER_TOPIC, PROVIDER_MISSING);
        };
        let label = provider_label(&target).to_owned();
        if target == self.state.setup.provider() && self.state.setup.login() == Login::Ready {
            let body = format!("Already using {label}.");
            return self
                .state
                .notice(NoticeTone::Neutral, PROVIDER_TOPIC, &body);
        }
        let switch = match self.state.setup.switch_target(target) {
            Ok(switch) => switch,
            Err(notice) => return self.state.emit(UiEvent::Notice { notice }),
        };
        let preparing = format!("Preparing {label}.");
        self.state
            .notice(NoticeTone::Neutral, PROVIDER_TOPIC, &preparing);
        let route = match self
            .state
            .setup
            .route_for(&switch, &CancellationToken::new())
            .await
        {
            Ok(route) => route,
            Err(notice) => return self.state.emit(UiEvent::Notice { notice }),
        };
        self.adopt(switch.provider(), route);
    }

    pub(super) fn drop_held_prompts(&mut self) {
        if self.state.setup.login() == Login::Missing && self.state.worker.has_waiting_prompts() {
            self.state.worker.clear();
            self.state.emit(UiEvent::HeldPromptDropped);
        }
    }

    pub(super) async fn retry_held_prompt(&mut self) {
        if self.state.setup.login() == Login::Ready || !self.restore_login().await {
            self.state.refuse_signed_out();
        }
    }

    async fn restore_login(&mut self) -> bool {
        let saved = self
            .state
            .setup
            .chatgpt_oauth()
            .is_some_and(|oauth| oauth.has_saved_login());
        let Some(switch) = saved
            .then(|| self.state.setup.switch_target(ProviderId::Codex).ok())
            .flatten()
        else {
            return false;
        };
        match self
            .state
            .setup
            .route_for(&switch, &CancellationToken::new())
            .await
        {
            Ok(route) => {
                self.install(&ProviderId::Codex, route);
                true
            }
            Err(_) => false,
        }
    }

    fn install(&mut self, target: &ProviderId, route: Route) {
        let (provider, models) = self.state.setup.adopt(route);
        let resolver: Arc<dyn CapabilityResolver> = Arc::new(models.clone());
        self.agent.set_provider(provider, Some(resolver));
        self.catalog.retarget(models, target.label());
        self.state.setup.model().clone_into(&mut self.state.model);
        self.reconfigure();
        self.state.emit(UiEvent::LoginChanged { missing: false });
    }

    fn adopt(&mut self, target: &ProviderId, route: Route) {
        let label = provider_label(target).to_owned();
        self.install(target, route);
        self.state.emit(UiEvent::ProviderSelected {
            provider: target.label().to_owned(),
        });
        self.state.emit(UiEvent::ModelSelected {
            model: self.state.model.clone(),
        });
        let session = match (&mut self.persistence, session_route(&self.state.setup)) {
            (Some(persistence), Ok(route)) => persistence
                .select_provider(&mut self.agent, route, &self.state.model)
                .is_ok(),
            (None, _) => true,
            (Some(_), Err(_)) => false,
        };
        let state = &self.state;
        let settings = user_settings::save(state.setup.preferences(), |paths| {
            save_provider_model(paths, target, &state.model).map_err(Into::into)
        });
        let notice = if session && settings.is_ok() {
            let body = format!("Switched to {label} with {}.", state.model);
            Notice::new(NoticeTone::Neutral, PROVIDER_TOPIC, body)
        } else {
            Notice::new(NoticeTone::Warning, PROVIDER_TOPIC, UNSAVED)
        };
        state.emit(UiEvent::Notice { notice });
    }
}
