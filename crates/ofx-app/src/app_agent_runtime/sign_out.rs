use ofx_auth::{
    ChatGptOAuth, DeleteOutcome, GatewaySource, GrokOAuth, VercelOAuth, parse_login_provider,
};
use ofx_config::{ProfilePaths, ProviderId, clear_credential_source};
use ofx_contract::{BoxFuture, Notice, NoticeTone, UiEvent};
use ofx_trace::trace_log;

use super::{CatalogFetch, Controller, ControllerState};
use crate::app_bootstrap_runtime::{CredentialSource, Login};

const AUTH_TOPIC: &str = "auth";
const PROVIDER_TOPIC: &str = "provider";
const LOGOUT_USAGE: &str = "usage: /logout [vercel|codex|grok]";
const SIGN_OUT_BUSY: &str = "Sign out is unavailable until active and queued work finishes.";
const NO_PROVIDER: &str = "No connected provider is available. Use /provider to sign in.";
const SIGNED_OUT: &str = "Codex needs a subscription login. Run /login, open Connections, then choose Codex subscription.";
const CODEX_SIGNED_OUT: &str = "Signed out of Codex.";
const CODEX_MISSING: &str = "No Codex login session found.";
const CODEX_NOT_DURABLE: &str =
    "Signed out of Codex, but could not confirm the profile directory update.";
const CODEX_FAILED: &str = "Could not durably sign out of Codex. The current source is unchanged.";
const GROK_SIGNED_OUT: &str = "Signed out of Grok.";
const GROK_MISSING: &str = "No Grok login session found.";
const GROK_NOT_DURABLE: &str =
    "Signed out of Grok, but could not confirm the profile directory update.";
const GROK_FAILED: &str = "Could not durably sign out of Grok. The current source is unchanged.";
const GROK_NOT_REVOKED: &str =
    "The local Grok session was removed, but remote revocation could not be confirmed.";
const NO_LOGIN: &str = "No oh-fx login session found.";
const LOGIN_SIGNED_OUT: &str = "Signed out of oh-fx.";
const LOGIN_NOT_DURABLE: &str =
    "Could not confirm durable oh-fx logout. The active source was recalculated.";
const LOGIN_FAILED: &str = "Could not complete oh-fx logout. The current source is unchanged.";
const LOGIN_NOT_REVOKED: &str =
    "Warning: signed out locally, but the remote session could not be revoked.";

enum Removal {
    Removed(Vec<Notice>),
    Failed(Notice),
}

impl ControllerState {
    pub(crate) fn login_missing(&self) -> bool {
        self.setup.login() == Login::Missing
    }

    pub(crate) fn holds_prompt(&self) -> bool {
        self.login_missing() && self.worker.has_waiting_prompts()
    }

    pub(super) fn refuse_signed_out(&self) {
        self.notice(NoticeTone::Warning, AUTH_TOPIC, SIGNED_OUT);
    }

    fn logout_target(&self, requested: &str) -> Option<ProviderId> {
        let requested = requested.trim_matches([' ', '\t', '\r', '\n']);
        if requested.is_empty() {
            return Some(self.default_logout());
        }
        let target = parse_login_provider(requested);
        if target.is_none() {
            self.notice(NoticeTone::Warning, "", LOGOUT_USAGE);
        }
        target
    }

    fn default_logout(&self) -> ProviderId {
        if self.setup.provider() == ProviderId::Codex {
            return ProviderId::Codex;
        }
        if self.setup.source() == CredentialSource::Gateway(GatewaySource::FxLogin) {
            return ProviderId::Gateway;
        }
        let vercel = self
            .setup
            .vercel_oauth()
            .is_some_and(|oauth| oauth.has_saved_login());
        let codex = self
            .setup
            .chatgpt_oauth()
            .is_some_and(|oauth| oauth.has_saved_login());
        let grok = self
            .setup
            .grok_oauth()
            .is_some_and(|oauth| oauth.has_saved_login());
        match (codex, grok, vercel) {
            (true, false, false) => ProviderId::Codex,
            (false, true, false) => ProviderId::Grok,
            _ => ProviderId::Gateway,
        }
    }

    fn selected_subscription(&self, target: &ProviderId) -> bool {
        *target != ProviderId::Gateway && *target == self.setup.provider()
    }

    fn removal(&self, target: &ProviderId) -> BoxFuture<'static, Removal> {
        match target {
            ProviderId::Codex => {
                let oauth = self.setup.chatgpt_oauth();
                Box::pin(async move { remove_codex(oauth).await })
            }
            ProviderId::Grok => {
                let oauth = self.setup.grok_oauth();
                Box::pin(async move { remove_grok(oauth).await })
            }
            _ => {
                let oauth = self.setup.vercel_oauth();
                let forget = (self.setup.source()
                    == CredentialSource::Gateway(GatewaySource::FxLogin))
                .then(|| self.setup.preferences().cloned())
                .flatten();
                Box::pin(async move { remove_vercel(oauth, forget).await })
            }
        }
    }

    pub(super) fn sign_out_during_work(&self, background: &mut CatalogFetch, requested: &str) {
        if ofx_auth::host_managed_auth() {
            return self.notice(
                NoticeTone::Neutral,
                AUTH_TOPIC,
                ofx_auth::HOST_MANAGED_AUTH_MESSAGE,
            );
        }
        let Some(target) = self.logout_target(requested) else {
            return;
        };
        if self.selected_subscription(&target) {
            return self.notice(NoticeTone::Warning, AUTH_TOPIC, SIGN_OUT_BUSY);
        }
        let removal = self.removal(&target);
        background.run(Box::pin(async move {
            match removal.await {
                Removal::Removed(notices) => notices,
                Removal::Failed(notice) => vec![notice],
            }
        }));
    }
}

impl Controller {
    pub(super) async fn sign_out(&mut self, requested: &str) {
        if ofx_auth::host_managed_auth() {
            return self.state.notice(
                NoticeTone::Neutral,
                AUTH_TOPIC,
                ofx_auth::HOST_MANAGED_AUTH_MESSAGE,
            );
        }
        let Some(target) = self.state.logout_target(requested) else {
            return;
        };
        let selected = self.state.selected_subscription(&target);
        if selected && self.state.has_queued_prompts() {
            return self
                .state
                .notice(NoticeTone::Warning, AUTH_TOPIC, SIGN_OUT_BUSY);
        }
        let notices = match self.state.removal(&target).await {
            Removal::Removed(notices) => notices,
            Removal::Failed(notice) => return self.state.emit(UiEvent::Notice { notice }),
        };
        for notice in notices {
            self.state.emit(UiEvent::Notice { notice });
        }
        if selected {
            if let Some(persistence) = &mut self.persistence {
                persistence.stop_title_generation();
            }
            let route = self.state.setup.sign_out(&self.state.model);
            self.install(&target, route);
            self.state
                .notice(NoticeTone::Warning, PROVIDER_TOPIC, NO_PROVIDER);
        }
    }
}

async fn remove_codex(oauth: Option<ChatGptOAuth>) -> Removal {
    let Some(oauth) = oauth else {
        return Removal::Failed(auth(NoticeTone::Error, CODEX_FAILED));
    };
    match oauth.logout().await {
        Ok(DeleteOutcome::Deleted) => {
            Removal::Removed(vec![auth(NoticeTone::Neutral, CODEX_SIGNED_OUT)])
        }
        Ok(DeleteOutcome::Missing) => {
            Removal::Removed(vec![auth(NoticeTone::Neutral, CODEX_MISSING)])
        }
        Ok(DeleteOutcome::DeletedNotDurable) => {
            Removal::Removed(vec![auth(NoticeTone::Warning, CODEX_NOT_DURABLE)])
        }
        Err(_) => Removal::Failed(auth(NoticeTone::Error, CODEX_FAILED)),
    }
}

async fn remove_grok(oauth: Option<GrokOAuth>) -> Removal {
    let Some(oauth) = oauth else {
        return Removal::Failed(auth(NoticeTone::Error, GROK_FAILED));
    };
    let Ok(result) = oauth.logout().await else {
        return Removal::Failed(auth(NoticeTone::Error, GROK_FAILED));
    };
    let mut notices = vec![match result.deletion {
        DeleteOutcome::Deleted => auth(NoticeTone::Neutral, GROK_SIGNED_OUT),
        DeleteOutcome::Missing => auth(NoticeTone::Neutral, GROK_MISSING),
        DeleteOutcome::DeletedNotDurable => auth(NoticeTone::Warning, GROK_NOT_DURABLE),
    }];
    if result.revocation_failed {
        notices.push(auth(NoticeTone::Warning, GROK_NOT_REVOKED));
    }
    Removal::Removed(notices)
}

async fn remove_vercel(oauth: Option<VercelOAuth>, forget: Option<ProfilePaths>) -> Removal {
    let Some(oauth) = oauth else {
        return Removal::Failed(auth(NoticeTone::Error, LOGIN_FAILED));
    };
    let Ok(result) = oauth.logout().await else {
        return Removal::Failed(auth(NoticeTone::Error, LOGIN_FAILED));
    };
    if let Some(paths) = forget {
        match tokio::task::spawn_blocking(move || clear_credential_source(&paths)).await {
            Ok(Ok(())) => trace_log!(AUTH_TOPIC, "credential choice cleared"),
            Ok(Err(error)) => {
                trace_log!(AUTH_TOPIC, "credential choice not cleared err={error}");
            }
            Err(_) => trace_log!(AUTH_TOPIC, "credential choice not cleared err=JoinFailed"),
        }
    }
    let mut notices = vec![if result.local_durability_failed {
        auth(NoticeTone::Warning, LOGIN_NOT_DURABLE)
    } else if result.session_deleted {
        auth(NoticeTone::Neutral, LOGIN_SIGNED_OUT)
    } else {
        auth(NoticeTone::Neutral, NO_LOGIN)
    }];
    if result.remote_revocation_failed {
        notices.push(auth(NoticeTone::Warning, LOGIN_NOT_REVOKED));
    }
    Removal::Removed(notices)
}

fn auth(tone: NoticeTone, body: &str) -> Notice {
    Notice::new(tone, AUTH_TOPIC, body)
}
