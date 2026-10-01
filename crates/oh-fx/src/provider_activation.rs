use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ofx_auth::{
    CHATGPT_SOURCE_LABEL, ChatGptAccess, ChatGptError, ChatGptOAuth, PreparationError,
    login_failure_detail, prepare_chatgpt_credential,
};
use ofx_config::{ProfilePaths, Settings, save_codex_model};
use ofx_gateway::{CatalogCredential, CodexModelCatalog};
use tokio_util::sync::CancellationToken;

use crate::codex_provider::SubscriptionEndpoints;

const SETTINGS_UNAVAILABLE: &str = "could not load settings";

pub(crate) struct Profile {
    pub(crate) paths: Option<ProfilePaths>,
    pub(crate) workspace: io::Result<PathBuf>,
    pub(crate) endpoints: SubscriptionEndpoints,
    pub(crate) lookup: fn(&str) -> Option<String>,
}

impl Profile {
    pub(crate) fn from_environment() -> Self {
        Self {
            paths: ProfilePaths::from_environment(),
            workspace: env::current_dir().and_then(fs::canonicalize),
            endpoints: SubscriptionEndpoints::default(),
            lookup: |name| env::var(name).ok(),
        }
    }

    pub(crate) fn chatgpt_oauth(&self) -> Result<ChatGptOAuth, ChatGptError> {
        let paths = self
            .paths
            .as_ref()
            .ok_or(ChatGptError::CredentialStorageUnavailable)?;
        ChatGptOAuth::new(
            paths.data.clone(),
            &crate::user_agent(),
            self.endpoints.chatgpt.clone(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActivationFailure {
    Detail(String),
    Fatal(&'static str),
}

impl ActivationFailure {
    pub(crate) fn report(&self, command: &str) {
        let mut stderr = io::stderr().lock();
        let _ = match self {
            Self::Detail(detail) => writeln!(stderr, "oh-fx {command}: {detail}"),
            Self::Fatal(code) => writeln!(stderr, "oh-fx: {code}"),
        };
    }
}

fn detail(text: impl Into<String>) -> ActivationFailure {
    ActivationFailure::Detail(text.into())
}

pub(crate) enum Caller<'a> {
    Login,
    ProviderCommand {
        output: &'a mut (dyn Write + Send),
        open_browser: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Activation {
    AlreadySelected,
    Selected { signed_in: bool },
}

pub(crate) async fn sign_in(
    profile: &Profile,
    output: &mut (dyn Write + Send),
    open_browser: bool,
) -> Result<(), ActivationFailure> {
    let failed = |error| ActivationFailure::Detail(login_failure_detail(error));
    let oauth = profile.chatgpt_oauth().map_err(failed)?;
    oauth
        .run_login(output, open_browser, &CancellationToken::new())
        .await
        .map_err(failed)
}

pub(crate) async fn activate_codex(
    profile: &Profile,
    caller: Caller<'_>,
    host_managed: bool,
) -> Result<Activation, ActivationFailure> {
    let workspace = profile
        .workspace
        .as_ref()
        .map_err(|_| ActivationFailure::Fatal("WorkspaceUnavailable"))?;
    let paths = profile
        .paths
        .as_ref()
        .ok_or_else(|| detail(SETTINGS_UNAVAILABLE))?;
    let settings = load_settings(paths, workspace, &profile.lookup)?;
    let mut access = if host_managed {
        None
    } else {
        prepared_credential(profile).await?
    };
    let selected = settings.codex_selected(&profile.lookup) == Ok(true);
    let has_model = settings.saved_codex_model().is_some();
    let mut signed_in = false;
    if let Caller::ProviderCommand {
        output,
        open_browser,
    } = caller
    {
        if selected && has_model && (host_managed || access.is_some()) {
            return Ok(Activation::AlreadySelected);
        }
        if !host_managed && access.is_none() {
            sign_in(profile, output, open_browser).await?;
            signed_in = true;
            access = prepared_credential(profile).await?;
        }
    }
    let credential = if host_managed {
        None
    } else {
        let access = access.ok_or_else(|| detail("Codex credential is unavailable"))?;
        let account_id = access.account_id().to_owned();
        Some(CatalogCredential::new(access.into_token(), account_id))
    };
    let models = fetch_catalog(profile, paths, credential.as_ref()).await?;
    let model = select_catalog_model(&models, settings.saved_codex_model())
        .ok_or_else(|| detail("target model catalog is empty"))?;
    save_selection(paths, model).await?;
    Ok(Activation::Selected { signed_in })
}

fn load_settings(
    paths: &ProfilePaths,
    workspace: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Settings, ActivationFailure> {
    let settings = Settings::load(paths, workspace)
        .ok()
        .filter(|settings| !settings.profile_is_unusable())
        .ok_or_else(|| detail(SETTINGS_UNAVAILABLE))?;
    settings
        .codex_selected(lookup)
        .map_err(|_| detail(SETTINGS_UNAVAILABLE))?;
    Ok(settings)
}

async fn prepared_credential(
    profile: &Profile,
) -> Result<Option<ChatGptAccess>, ActivationFailure> {
    let unavailable =
        |error: PreparationError| detail(format!("{CHATGPT_SOURCE_LABEL}: {}", error.notice()));
    let oauth = profile
        .chatgpt_oauth()
        .map_err(|_| unavailable(PreparationError::CredentialTemporarilyUnavailable))?;
    prepare_chatgpt_credential(&oauth, &CancellationToken::new())
        .await
        .map_err(unavailable)
}

async fn fetch_catalog(
    profile: &Profile,
    paths: &ProfilePaths,
    credential: Option<&CatalogCredential>,
) -> Result<Vec<String>, ActivationFailure> {
    let catalog = CodexModelCatalog::new(
        &crate::user_agent(),
        profile.endpoints.models.clone(),
        Some(paths.cache.clone()),
    )
    .map_err(|_| detail("Codex model catalog is unavailable"))?;
    catalog
        .fetch(credential, &CancellationToken::new())
        .await
        .map_err(|failure| {
            detail(format!(
                "could not load the target model catalog ({})",
                failure.label()
            ))
        })
}

async fn save_selection(paths: &ProfilePaths, model: &str) -> Result<(), ActivationFailure> {
    let paths = paths.clone();
    let model = model.to_owned();
    tokio::task::spawn_blocking(move || save_codex_model(&paths, &model))
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| detail("failed to save provider selection"))
}

fn select_catalog_model<'a>(models: &'a [String], saved: Option<&str>) -> Option<&'a str> {
    saved
        .and_then(|saved| models.iter().find(|model| *model == saved))
        .or_else(|| models.first())
        .map(String::as_str)
}

#[cfg(test)]
pub(crate) mod tests;
