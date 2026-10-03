use std::fmt::Write as _;
use std::io::{self, Write};
use std::process::ExitCode;

use ofx_auth::{CHATGPT_SOURCE_LABEL, prepare_chatgpt_credential};
use ofx_cli::{Command, OutputFormat, TopLevelKind, command_failure_json};
use ofx_config::Settings;
use ofx_gateway::{CatalogCredential, CatalogFailure, CodexModelCatalog};
use ofx_text::encode_terminal_safe;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::provider_activation::Profile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Listing {
    Listed,
    Failed,
    NotCodex,
}

pub(crate) fn run(format: OutputFormat) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let profile = Profile::from_environment();
    let listing = if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        runtime.block_on(list_models(
            &profile,
            format,
            crate::login_command::host_managed(),
            &mut io::stdout().lock(),
            &mut io::stderr(),
        ))
    } else {
        let _ = writeln!(io::stderr(), "oh-fx: TransportUnavailable");
        Listing::Failed
    };
    match listing {
        Listing::Listed => ExitCode::SUCCESS,
        Listing::Failed => ExitCode::FAILURE,
        Listing::NotCodex => crate::not_available(&Command::Models(format)),
    }
}

pub(crate) async fn list_models(
    profile: &Profile,
    format: OutputFormat,
    host_managed: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Listing {
    let fatal = |stderr: &mut dyn Write, text: &str| {
        let _ = writeln!(stderr, "oh-fx: {text}");
        Listing::Failed
    };
    let Ok(workspace) = &profile.workspace else {
        return fatal(stderr, "WorkspaceUnavailable");
    };
    let settings = match &profile.paths {
        Some(paths) => match Settings::load(paths, workspace) {
            Ok(settings) => settings,
            Err(error) => return fatal(stderr, &error.to_string()),
        },
        None => Settings::default(),
    };
    if settings.profile_is_unusable() {
        return fatal(stderr, "InvalidProfileConfiguration");
    }
    match settings.codex_selected(&profile.lookup) {
        Ok(true) => {}
        Ok(false) => return Listing::NotCodex,
        Err(error) => return fatal(stderr, error.code()),
    }
    if let Err(error) = settings.selected_codex_model(None, &profile.lookup) {
        return fatal(stderr, &error.to_string());
    }
    for diagnostic in settings.diagnostics() {
        let _ = writeln!(stderr, "oh-fx: {diagnostic}");
    }
    let listed = match catalog_credential(profile, host_managed).await {
        Ok(credential) => fetch(profile, credential.as_ref()).await,
        Err(failure) => Err(failure),
    };
    let written = match (listed, format) {
        (Ok(ids), OutputFormat::Text) => stdout.write_all(text_listing(&ids).as_bytes()),
        (Ok(ids), OutputFormat::Json) => stdout.write_all(json_listing(&ids).as_bytes()),
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

async fn fetch(
    profile: &Profile,
    credential: Option<&CatalogCredential>,
) -> Result<Vec<String>, CatalogFailure> {
    let catalog = CodexModelCatalog::new(
        &ofx_app::user_agent(),
        profile.endpoints.models.clone(),
        profile.paths.as_ref().map(|paths| paths.cache.clone()),
    )
    .map_err(|_| CatalogFailure::Transport)?;
    let models = catalog.fetch(credential, &CancellationToken::new()).await?;
    Ok(models.into_iter().map(|model| model.id).collect())
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

fn text_listing(ids: &[String]) -> String {
    if ids.is_empty() {
        return format!("[models] no models returned by {CHATGPT_SOURCE_LABEL}\n");
    }
    let mut text = format!("[models] {} available\n", ids.len());
    for id in ids {
        let id = encode_terminal_safe(id.as_bytes(), usize::MAX).text;
        let _ = writeln!(text, " - {id} · {CHATGPT_SOURCE_LABEL}");
    }
    text
}

fn json_listing(ids: &[String]) -> String {
    let models: Vec<_> = ids
        .iter()
        .map(|id| json!({"id": id, "source": CHATGPT_SOURCE_LABEL}))
        .collect();
    let mut line = json!({
        "kind": "models",
        "count": ids.len(),
        "shown_count": ids.len(),
        "more_count": 0,
        "private_models_hidden": false,
        "ids": ids,
        "models": models,
    })
    .to_string();
    line.push('\n');
    line
}

#[cfg(test)]
mod tests;
