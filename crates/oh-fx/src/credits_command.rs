use std::fmt::Write as _;
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_auth::{GatewaySource, GatewaySources, StoredKeyStore};
use ofx_cli::OutputFormat;
use ofx_config::{ProviderId, Settings};
use ofx_gateway::{CreditsSnapshot, fetch_credits};
use ofx_text::encode_terminal_safe;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::provider_activation::Profile;

const UNAVAILABLE: &str = "Credits are unavailable for the selected provider.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Credits {
    Gateway(Option<GatewaySource>),
    Unavailable,
}
const NO_DATA: &str = "no data returned by gateway";

pub(crate) fn run(format: OutputFormat) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let profile = Profile::from_environment();
    if let Ok(workspace) = &profile.workspace {
        ofx_trace::configure_from_env(
            workspace,
            profile.paths.as_ref().map(|paths| paths.state.as_path()),
        );
    }
    let selected = match selection(&profile, &mut io::stderr()) {
        Ok(selected) => selected,
        Err(code) => return code,
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        let _ = writeln!(io::stderr(), "oh-fx: TransportUnavailable");
        return ExitCode::FAILURE;
    };
    let snapshot = runtime.block_on(credits(&profile, selected, ofx_auth::host_managed_auth()));
    report(
        &snapshot,
        format,
        &mut io::stdout().lock(),
        &mut io::stderr(),
    )
}

fn selection(profile: &Profile, stderr: &mut dyn Write) -> Result<Credits, ExitCode> {
    let mut fatal = |text: &str| {
        let _ = writeln!(stderr, "oh-fx: {text}");
        ExitCode::FAILURE
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
    let selected = match settings.selected_provider(&profile.lookup) {
        Ok(ProviderId::Gateway) => {
            Credits::Gateway(settings.credential_source().and_then(GatewaySource::parse))
        }
        Ok(_) => Credits::Unavailable,
        Err(error) => return Err(fatal(error.code())),
    };
    for diagnostic in settings.diagnostics() {
        let _ = writeln!(stderr, "oh-fx: {diagnostic}");
    }
    Ok(selected)
}

pub(crate) async fn credits(
    profile: &Profile,
    selected: Credits,
    host_managed: bool,
) -> CreditsSnapshot {
    let Credits::Gateway(preferred) = selected else {
        return CreditsSnapshot::failed(UNAVAILABLE);
    };
    let (credential, team) = if host_managed {
        (None, None)
    } else {
        credential(profile, preferred).await
    };
    fetch_credits(
        &ofx_app::user_agent(),
        &profile.endpoints.gateway_credits,
        credential.as_deref().map(String::as_str),
        team.as_deref(),
        &CancellationToken::new(),
    )
    .await
}

async fn credential(
    profile: &Profile,
    preferred: Option<GatewaySource>,
) -> (Option<Zeroizing<String>>, Option<String>) {
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
    let Some(key) = resolution.key() else {
        return (None, None);
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0);
    let team = (!key.needs_refresh_at(now_ms))
        .then(|| key.gateway_team().map(str::to_owned))
        .flatten();
    (Some(key.secret.clone()), team)
}

pub(crate) fn report(
    snapshot: &CreditsSnapshot,
    format: OutputFormat,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    let rendered = match format {
        OutputFormat::Text => text(snapshot),
        OutputFormat::Json => json_line(snapshot),
    };
    let failed = snapshot.error.is_some();
    let written = if failed && matches!(format, OutputFormat::Text) {
        stderr.write_all(rendered.as_bytes())
    } else {
        stdout
            .write_all(rendered.as_bytes())
            .and_then(|()| stdout.flush())
    };
    if written.is_err() {
        let _ = writeln!(stderr, "oh-fx: WriteFailed");
        return ExitCode::FAILURE;
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn text(snapshot: &CreditsSnapshot) -> String {
    let safe = |value: &str| encode_terminal_safe(value.as_bytes(), usize::MAX).text;
    if let Some(error) = &snapshot.error {
        return format!("[credits] error: {}\n", safe(error));
    }
    let mut text = String::new();
    for (name, value) in [
        ("balance", &snapshot.balance),
        ("used", &snapshot.used),
        ("plan", &snapshot.plan),
    ] {
        if let Some(value) = value {
            let _ = writeln!(text, "[credits] {name}={}", safe(value));
        }
    }
    if text.is_empty() {
        let _ = writeln!(text, "[credits] {NO_DATA}");
    }
    text
}

fn json_line(snapshot: &CreditsSnapshot) -> String {
    let value = match &snapshot.error {
        Some(error) => json!({"kind": "credits", "error": error}),
        None => json!({
            "kind": "credits",
            "balance": snapshot.balance,
            "used": snapshot.used,
            "plan": snapshot.plan,
        }),
    };
    let mut line = value.to_string();
    line.push('\n');
    line
}

#[cfg(test)]
mod tests;
