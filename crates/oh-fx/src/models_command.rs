use std::fmt::Write as _;
use std::io::{self, Write};
use std::process::ExitCode;

use std::time::{SystemTime, UNIX_EPOCH};

use ofx_auth::{
    CHATGPT_SOURCE_LABEL, GatewaySource, GatewaySources, StoredKeyStore, prepare_chatgpt_credential,
};
use ofx_cli::{Command, OutputFormat, TopLevelKind, command_failure_json};
use ofx_config::{ProviderId, Settings};
use ofx_gateway::{
    CatalogAccess, CatalogCredential, CatalogFailure, CatalogFetch, CatalogView, CodexModelCatalog,
    GatewayModelCatalog, PublicOnlyReason,
};
use ofx_text::encode_terminal_safe;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::provider_activation::Profile;

const GATEWAY: &str = "gateway";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Listing {
    Listed,
    Failed,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Catalog {
    Codex,
    Gateway(Option<GatewaySource>),
}

struct Models {
    ids: Vec<String>,
    source: Option<&'static str>,
    provider: &'static str,
    public_only: Option<PublicOnlyReason>,
}

pub(crate) fn run(format: OutputFormat) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let profile = Profile::from_environment();
    if let Ok(workspace) = &profile.workspace {
        ofx_trace::configure_from_env(
            workspace,
            profile.paths.as_ref().map(|paths| paths.state.as_path()),
        );
    }
    let listing = match selection(&profile, &mut io::stderr()) {
        Ok(catalog) => listed(&profile, catalog, format),
        Err(listing) => listing,
    };
    match listing {
        Listing::Listed => ExitCode::SUCCESS,
        Listing::Failed => ExitCode::FAILURE,
        Listing::Unsupported => crate::not_available(&Command::Models(format)),
    }
}

fn listed(profile: &Profile, catalog: Catalog, format: OutputFormat) -> Listing {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        let _ = writeln!(io::stderr(), "oh-fx: TransportUnavailable");
        return Listing::Failed;
    };
    runtime.block_on(list_models(
        profile,
        catalog,
        format,
        ofx_auth::host_managed_auth(),
        &mut io::stdout().lock(),
        &mut io::stderr(),
    ))
}

fn selection(profile: &Profile, stderr: &mut dyn Write) -> Result<Catalog, Listing> {
    let mut fatal = |text: &str| {
        let _ = writeln!(stderr, "oh-fx: {text}");
        Listing::Failed
    };
    let Ok(workspace) = &profile.workspace else {
        return Err(fatal("WorkspaceUnavailable"));
    };
    let settings = match &profile.paths {
        Some(paths) => match Settings::load(paths, workspace) {
            Ok(settings) => settings,
            Err(error) => return Err(fatal(&error.to_string())),
        },
        None => Settings::default(),
    };
    if settings.profile_is_unusable() {
        return Err(fatal("InvalidProfileConfiguration"));
    }
    let catalog = match settings.selected_provider(&profile.lookup) {
        Ok(ProviderId::Gateway) => {
            Catalog::Gateway(settings.credential_source().and_then(GatewaySource::parse))
        }
        Ok(ProviderId::Codex) => Catalog::Codex,
        Ok(ProviderId::Grok | ProviderId::Configured(_)) => return Err(Listing::Unsupported),
        Err(error) => return Err(fatal(error.code())),
    };
    if catalog == Catalog::Codex
        && let Err(error) = settings.selected_codex_model(None, &profile.lookup)
    {
        return Err(fatal(&error.to_string()));
    }
    for diagnostic in settings.diagnostics() {
        let _ = writeln!(stderr, "oh-fx: {diagnostic}");
    }
    Ok(catalog)
}

pub(crate) async fn list_models(
    profile: &Profile,
    catalog: Catalog,
    format: OutputFormat,
    host_managed: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Listing {
    let fatal = |stderr: &mut dyn Write, text: &str| {
        let _ = writeln!(stderr, "oh-fx: {text}");
        Listing::Failed
    };
    let listed = match catalog {
        Catalog::Codex => codex_models(profile, host_managed).await,
        Catalog::Gateway(preferred) => gateway_models(profile, preferred, host_managed).await,
    };
    let written = match (listed, format) {
        (Ok(models), OutputFormat::Text) => stdout.write_all(text_listing(&models).as_bytes()),
        (Ok(models), OutputFormat::Json) => stdout.write_all(json_listing(&models).as_bytes()),
        (Err(failure), OutputFormat::Text) => {
            let _ = writeln!(stderr, "oh-fx models: {}", failure_message(failure));
            return Listing::Failed;
        }
        (Err(failure), OutputFormat::Json) => {
            let line = command_failure_json(
                TopLevelKind::Models,
                &failure_message(failure),
                failure.error_name(),
            );
            return match stdout
                .write_all(line.as_bytes())
                .and_then(|()| stdout.flush())
            {
                Ok(()) => Listing::Failed,
                Err(_) => fatal(stderr, "WriteFailed"),
            };
        }
    };
    match written.and_then(|()| stdout.flush()) {
        Ok(()) => Listing::Listed,
        Err(_) => fatal(stderr, "WriteFailed"),
    }
}

async fn codex_models(profile: &Profile, host_managed: bool) -> Result<Models, CatalogFailure> {
    let credential = catalog_credential(profile, host_managed).await?;
    let catalog = CodexModelCatalog::new(
        &ofx_app::user_agent(),
        profile.endpoints.models.clone(),
        profile.paths.as_ref().map(|paths| paths.cache.clone()),
    )
    .map_err(|_| CatalogFailure::Transport)?;
    let models = catalog
        .fetch(credential.as_ref(), &CancellationToken::new())
        .await?;
    Ok(Models {
        ids: models.into_iter().map(|model| model.id).collect(),
        source: Some(CHATGPT_SOURCE_LABEL),
        provider: CHATGPT_SOURCE_LABEL,
        public_only: None,
    })
}

async fn catalog_credential(
    profile: &Profile,
    host_managed: bool,
) -> Result<Option<CatalogCredential>, CatalogFailure> {
    if host_managed {
        return Ok(None);
    }
    let oauth = profile
        .chatgpt_oauth()
        .map_err(|_| CatalogFailure::Authentication)?;
    let access = prepare_chatgpt_credential(&oauth, &CancellationToken::new())
        .await
        .ok()
        .flatten()
        .ok_or(CatalogFailure::Authentication)?;
    let account_id = access.account_id().to_owned();
    Ok(Some(CatalogCredential::new(
        access.into_token(),
        account_id,
    )))
}

async fn gateway_models(
    profile: &Profile,
    preferred: Option<GatewaySource>,
    host_managed: bool,
) -> Result<Models, CatalogFailure> {
    let catalog = GatewayModelCatalog::new(
        &ofx_app::user_agent(),
        profile.endpoints.gateway_models.clone(),
    )
    .map_err(|_| CatalogFailure::Transport)?;
    let access = gateway_access(profile, preferred, host_managed).await;
    match catalog
        .load(&access, CatalogView::Full, &CancellationToken::new())
        .await
    {
        CatalogFetch::Loaded { models, access, .. } => Ok(Models {
            ids: models.into_iter().map(|model| model.id).collect(),
            source: None,
            provider: GATEWAY,
            public_only: access.public_only,
        }),
        CatalogFetch::Failed { failure, .. } => Err(failure.failure),
    }
}

async fn gateway_access(
    profile: &Profile,
    preferred: Option<GatewaySource>,
    host_managed: bool,
) -> CatalogAccess {
    if host_managed {
        return CatalogAccess::HostManaged;
    }
    let store = StoredKeyStore::from_environment(
        profile.paths.as_ref().map(|paths| paths.data.as_path()),
        &profile.lookup,
    );
    let login = profile.vercel_oauth();
    let resolution = GatewaySources {
        lookup: &profile.lookup,
        store: &store,
        login: login.as_ref(),
        preferred,
    }
    .refreshed()
    .await;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0);
    ofx_app::gateway_catalog_access(resolution.key(), preferred.is_some(), now_ms)
}

fn failure_message(failure: CatalogFailure) -> String {
    let detail = match failure {
        CatalogFailure::Authentication => "AuthenticationRejected",
        CatalogFailure::Cancellation => "the request was cancelled",
        CatalogFailure::MalformedResponse => "MalformedResponse",
        CatalogFailure::RateLimited
        | CatalogFailure::GatewayUnavailable { .. }
        | CatalogFailure::Transport
        | CatalogFailure::HttpStatus => "Unavailable",
    };
    format!("could not list models: {detail}")
}

const fn explanation(reason: PublicOnlyReason) -> &'static str {
    match reason {
        PublicOnlyReason::NoCredential => {
            "Using the public model catalog; sign in with Vercel or use an AI Gateway API key for team-private models."
        }
        PublicOnlyReason::LoginTeamRequired => "Choose a Vercel team to load its private models.",
        PublicOnlyReason::LoginRefreshRequired => {
            "Vercel sign-in must refresh before team-private models can load."
        }
        PublicOnlyReason::CredentialRejected(_) => {
            "Your Gateway credential was rejected; using the public model catalog."
        }
    }
}

fn text_listing(models: &Models) -> String {
    let mut text = if models.ids.is_empty() {
        format!("[models] no models returned by {}\n", models.provider)
    } else {
        format!("[models] {} available\n", models.ids.len())
    };
    for id in &models.ids {
        let id = encode_terminal_safe(id.as_bytes(), usize::MAX).text;
        let _ = match models.source {
            Some(source) => writeln!(text, " - {id} · {source}"),
            None => writeln!(text, " - {id}"),
        };
    }
    if let Some(reason) = models.public_only {
        let _ = writeln!(text, "[models] {}", explanation(reason));
    }
    text
}

fn json_listing(models: &Models) -> String {
    let ids = &models.ids;
    let mut listing = json!({
        "kind": "models",
        "count": ids.len(),
        "shown_count": ids.len(),
        "more_count": 0,
        "private_models_hidden": models.public_only.is_some(),
        "ids": ids,
    });
    if let Some(source) = models.source {
        let entries: Vec<_> = ids
            .iter()
            .map(|id| json!({"id": id, "source": source}))
            .collect();
        listing["models"] = json!(entries);
    }
    let mut line = listing.to_string();
    line.push('\n');
    line
}

#[cfg(test)]
mod tests;
