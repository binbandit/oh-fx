use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ofx_auth::{
    CHATGPT_SOURCE_LABEL, ChatGptAccess, ChatGptError, ChatGptOAuth, PreparationError,
    prepare_chatgpt_credential,
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

pub(crate) async fn activate_codex(profile: &Profile) -> Result<(), ActivationFailure> {
    let workspace = profile
        .workspace
        .as_ref()
        .map_err(|_| ActivationFailure::Fatal("WorkspaceUnavailable"))?;
    let paths = profile
        .paths
        .as_ref()
        .ok_or_else(|| detail(SETTINGS_UNAVAILABLE))?;
    let settings = load_settings(paths, workspace, &profile.lookup)?;
    let access = prepared_credential(profile)
        .await?
        .ok_or_else(|| detail("Codex credential is unavailable"))?;
    let models = fetch_catalog(profile, paths, access).await?;
    let model = select_catalog_model(&models, settings.saved_codex_model())
        .ok_or_else(|| detail("target model catalog is empty"))?;
    save_selection(paths, model).await
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
    access: ChatGptAccess,
) -> Result<Vec<String>, ActivationFailure> {
    let catalog = CodexModelCatalog::new(
        &crate::user_agent(),
        profile.endpoints.models.clone(),
        Some(paths.cache.clone()),
    )
    .map_err(|_| detail("Codex model catalog is unavailable"))?;
    let account_id = access.account_id().to_owned();
    let credential = CatalogCredential::new(access.into_token(), account_id);
    catalog
        .fetch(Some(&credential), &CancellationToken::new())
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
