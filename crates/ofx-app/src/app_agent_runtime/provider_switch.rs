use std::sync::Arc;

use ofx_auth::{ChatGptError, SignInFailure, sign_in_failure};
use ofx_config::{ProviderId, save_provider_model};
use ofx_contract::{BoxFuture, CapabilityResolver, Notice, NoticeTone, UiEvent};
use tokio_util::sync::CancellationToken;

use super::{Controller, ControllerState};
use crate::app_bootstrap_runtime::{Intent, Login, Refusal, Route, provider_label};
use crate::app_session_runtime::session_route;
use crate::user_settings;

const PROVIDER_TOPIC: &str = "provider";
const AUTH_TOPIC: &str = "auth";
const PROVIDER_BUSY: &str =
    "Provider switching is unavailable until active and queued work finishes.";
const PROVIDER_MISSING: &str =
    "The target provider catalog is unavailable. The current provider is unchanged.";
const UNSAVED: &str = "Provider switched for this run, but the selection could not be saved.";
const LOGIN_UNSAVED: &str =
    "Signed in to Codex for this run, but the saved session could not record it.";
const SIGN_IN_DENIED: &str = "Codex sign-in was denied. The current credential is unchanged.";
const SIGN_IN_EXPIRED: &str =
    "Codex sign-in expired. The current credential is unchanged; run /login to try again.";
const SIGN_IN_FAILED: &str = "Codex sign-in failed. The current credential is unchanged.";
const SIGN_IN_BUSY: &str = "Codex sign-in is unavailable until active and queued work finishes.";

pub(super) struct PendingSignIn {
    finish: BoxFuture<'static, Result<(), ChatGptError>>,
}

pub(super) struct SignInControl {
    cancel: CancellationToken,
    url: String,
}

impl SignInControl {
    pub(super) fn cancel(&self) {
        self.cancel.cancel();
    }

    pub(super) fn reopen(&self) {
        ofx_auth::open_url(&self.url);
    }
}

pub(super) async fn signed_in(slot: &mut Option<PendingSignIn>) -> Result<(), ChatGptError> {
    match slot {
        Some(pending) => (&mut pending.finish).await,
        None => std::future::pending().await,
    }
}

impl ControllerState {
    pub(crate) fn provider_busy(&self) {
        self.notice(NoticeTone::Neutral, PROVIDER_TOPIC, PROVIDER_BUSY);
    }

    pub(super) fn steer_sign_in(&self, steer: impl FnOnce(&SignInControl)) {
        if let Some(control) = &self.sign_in {
            steer(control);
        }
    }

    pub(crate) fn sign_in_busy(&self, provider: &str) {
        if ProviderId::parse(provider) == Some(ProviderId::Codex) {
            self.notice(NoticeTone::Warning, AUTH_TOPIC, SIGN_IN_BUSY);
        } else {
            self.provider_busy();
        }
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
        self.switch(target, Intent::Manual).await;
    }

    pub(super) async fn choose_login(&mut self, name: &str) {
        if ProviderId::parse(name) == Some(ProviderId::Codex) {
            self.begin_sign_in().await;
        } else {
            self.select_provider(name).await;
        }
    }

    pub(super) fn signing_in(&self) -> bool {
        self.sign_in.is_some()
    }

    pub(super) async fn finish_sign_in(&mut self, result: Result<(), ChatGptError>) {
        self.sign_in = None;
        self.state.sign_in = None;
        self.state.emit(UiEvent::SignInEnded);
        match result {
            Ok(()) => {
                let current = (self.state.setup.provider() == ProviderId::Codex)
                    .then(|| self.state.model.clone());
                self.switch(ProviderId::Codex, Intent::AfterSignIn(current.as_deref()))
                    .await;
                if self.state.setup.provider() != ProviderId::Codex {
                    self.drop_waiting_prompts();
                }
            }
            Err(ChatGptError::Cancelled) => self.drop_waiting_prompts(),
            Err(error) => {
                self.drop_waiting_prompts();
                self.state.emit(UiEvent::Notice {
                    notice: sign_in_notice(error),
                });
            }
        }
    }

    pub(super) fn drop_held_prompts(&mut self) {
        if self.state.setup.login() == Login::Missing {
            self.drop_waiting_prompts();
        }
    }

    fn drop_waiting_prompts(&mut self) {
        if self.state.worker.has_waiting_prompts() {
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
        let current = self.state.model.clone();
        let intent = Intent::AfterSignIn(Some(&current));
        match self
            .state
            .setup
            .route_for(&switch, intent, &CancellationToken::new())
            .await
        {
            Ok(route) => {
                self.install(&ProviderId::Codex, route);
                if self.state.model != current {
                    self.state.emit(UiEvent::ModelSelected {
                        model: self.state.model.clone(),
                    });
                }
                if !self.bind_session() {
                    self.state
                        .notice(NoticeTone::Warning, PROVIDER_TOPIC, LOGIN_UNSAVED);
                }
                true
            }
            Err(_) => false,
        }
    }

    async fn switch(&mut self, target: ProviderId, intent: Intent<'_>) {
        let switch = match self.state.setup.switch_target(target) {
            Ok(switch) => switch,
            Err(notice) => return self.state.emit(UiEvent::Notice { notice }),
        };
        let preparing = format!("Preparing {}.", provider_label(switch.provider()));
        self.state
            .notice(NoticeTone::Neutral, PROVIDER_TOPIC, &preparing);
        let routed = self
            .state
            .setup
            .route_for(&switch, intent, &CancellationToken::new())
            .await;
        match routed {
            Ok(route) => self.adopt(switch.provider(), route),
            Err(Refusal::Notice(notice)) => self.state.emit(UiEvent::Notice { notice }),
            Err(Refusal::SignIn) => self.begin_sign_in().await,
        }
    }

    async fn begin_sign_in(&mut self) {
        let Some(oauth) = self.state.setup.chatgpt_oauth() else {
            return self.state.emit(UiEvent::Notice {
                notice: sign_in_notice(ChatGptError::CredentialStorageUnavailable),
            });
        };
        let sign_in = match oauth.start_sign_in().await {
            Ok(sign_in) => sign_in,
            Err(error) => {
                return self.state.emit(UiEvent::Notice {
                    notice: sign_in_notice(error),
                });
            }
        };
        let url = sign_in.authorization_url().to_owned();
        self.state.emit(UiEvent::SignInStarted { url: url.clone() });
        if self.state.setup.opens_browser() {
            ofx_auth::open_url(&url);
        }
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        self.state.sign_in = Some(SignInControl { cancel, url });
        self.sign_in = Some(PendingSignIn {
            finish: Box::pin(async move { sign_in.finish(&stop).await }),
        });
    }

    pub(super) fn install(&mut self, target: &ProviderId, route: Route) {
        let (provider, models) = self.state.setup.adopt(route);
        let resolver: Arc<dyn CapabilityResolver> = Arc::new(models.clone());
        self.agent.set_provider(provider, Some(resolver));
        self.catalog.retarget(models, target.label());
        self.state.setup.model().clone_into(&mut self.state.model);
        self.reconfigure();
        self.state.emit(UiEvent::LoginChanged {
            missing: self.state.login_missing(),
        });
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
        let session = self.bind_session();
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

    fn bind_session(&mut self) -> bool {
        match (&mut self.persistence, session_route(&self.state.setup)) {
            (Some(persistence), Ok(route)) => persistence
                .select_provider(&mut self.agent, route, &self.state.model)
                .is_ok(),
            (None, _) => true,
            (Some(_), Err(_)) => false,
        }
    }
}

fn sign_in_notice(error: ChatGptError) -> Notice {
    let (tone, body) = match sign_in_failure(error) {
        SignInFailure::Storage(body) => (NoticeTone::Error, body),
        SignInFailure::Denied => (NoticeTone::Error, SIGN_IN_DENIED.to_owned()),
        SignInFailure::Expired => (NoticeTone::Warning, SIGN_IN_EXPIRED.to_owned()),
        SignInFailure::Failed => (NoticeTone::Error, SIGN_IN_FAILED.to_owned()),
    };
    Notice::new(tone, AUTH_TOPIC, body)
}
