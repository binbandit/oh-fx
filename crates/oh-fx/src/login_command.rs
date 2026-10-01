use std::env;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use ofx_auth::{
    AuthMode, ChatGptError, DeleteOutcome, HOST_MANAGED_AUTH_MESSAGE, login_failure_detail,
    parse_auth_mode,
};
use ofx_config::ProviderId;
use tokio_util::sync::CancellationToken;

use crate::provider_activation::{ActivationFailure, Profile, activate_codex};

const NO_OPEN_BROWSER_VARIABLE: &str = "OH_FX_NO_OPEN_BROWSER";
const LOGOUT_FAILURE: &str = "oh-fx logout: failed to durably remove saved Codex login\n";

pub(crate) fn login(provider: Option<&ProviderId>) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    if host_managed() {
        return print(&format!("{HOST_MANAGED_AUTH_MESSAGE}\n"));
    }
    if provider != Some(&ProviderId::Codex) {
        return unavailable("login");
    }
    let open_browser = env::var_os(NO_OPEN_BROWSER_VARIABLE).is_none();
    let profile = Profile::from_environment();
    let signed_in = runtime()
        .map_err(|error| ActivationFailure::Detail(login_failure_detail(error)))
        .and_then(|runtime| {
            runtime.block_on(login_codex(&profile, &mut io::stdout(), open_browser))
        });
    match signed_in {
        Ok(()) => print("Signed in with Codex.\n"),
        Err(failure) => {
            failure.report("login");
            ExitCode::FAILURE
        }
    }
}

pub(crate) async fn login_codex(
    profile: &Profile,
    output: &mut (dyn Write + Send),
    open_browser: bool,
) -> Result<(), ActivationFailure> {
    let oauth = profile
        .chatgpt_oauth()
        .map_err(|error| ActivationFailure::Detail(login_failure_detail(error)))?;
    oauth
        .run_login(output, open_browser, &CancellationToken::new())
        .await
        .map_err(|error| ActivationFailure::Detail(login_failure_detail(error)))?;
    activate_codex(profile).await
}

pub(crate) fn logout(provider: Option<&ProviderId>) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    if host_managed() {
        return print(&format!("{HOST_MANAGED_AUTH_MESSAGE}\n"));
    }
    if provider != Some(&ProviderId::Codex) {
        return unavailable("logout");
    }
    let profile = Profile::from_environment();
    let outcome = profile.chatgpt_oauth().and_then(|oauth| {
        runtime().and_then(|runtime| runtime.block_on(async { oauth.logout().await }))
    });
    match outcome {
        Ok(DeleteOutcome::Deleted) => print("Signed out of Codex.\n"),
        Ok(DeleteOutcome::Missing) => print("No Codex login session found.\n"),
        Ok(DeleteOutcome::DeletedNotDurable) | Err(_) => {
            let _ = io::stderr().write_all(LOGOUT_FAILURE.as_bytes());
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn host_managed() -> bool {
    let mode = env::var_os(crate::AUTH_MODE_VARIABLE);
    parse_auth_mode(mode.as_deref().map(OsStrExt::as_bytes)) == Some(AuthMode::HostManaged)
}

fn unavailable(command: &str) -> ExitCode {
    crate::write_unavailable(command);
    ExitCode::FAILURE
}

fn print(text: &str) -> ExitCode {
    match crate::write_stdout(text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => crate::write_failed(),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, ChatGptError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ChatGptError::OAuthTransportUnavailable)
}

#[cfg(test)]
mod tests;
