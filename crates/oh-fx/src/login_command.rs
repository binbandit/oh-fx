use std::env;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use ofx_auth::{
    AuthMode, ChatGptError, DeleteOutcome, HOST_MANAGED_AUTH_MESSAGE, login_failure_detail,
    parse_auth_mode,
};
use ofx_config::ProviderId;

use crate::provider_activation::{ActivationFailure, Caller, Profile, activate_codex, sign_in};

const NO_OPEN_BROWSER_VARIABLE: &str = "OH_FX_NO_OPEN_BROWSER";
const LOGOUT_FAILURE: &str = "oh-fx logout: failed to durably remove saved Codex login\n";

pub(crate) fn login(provider: Option<&ProviderId>) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    if host_managed() {
        return print(&format!("{HOST_MANAGED_AUTH_MESSAGE}\n"));
    }
    if !matches!(provider, Some(ProviderId::Codex | ProviderId::Grok)) {
        return unavailable("login");
    }
    let profile = Profile::from_environment();
    let signed_in = runtime()
        .map_err(|error| ActivationFailure::Detail(login_failure_detail(error)))
        .and_then(|runtime| {
            runtime.block_on(async {
                match provider {
                    Some(ProviderId::Grok) => {
                        login_grok(&profile, &mut io::stdout(), open_browser(), &io::stdin()).await
                    }
                    _ => login_codex(&profile, &mut io::stdout(), open_browser()).await,
                }
            })
        });
    match signed_in {
        Ok(()) => print(if provider == Some(&ProviderId::Grok) {
            "Signed in with Grok.\n"
        } else {
            "Signed in with Codex.\n"
        }),
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
    sign_in(profile, output, open_browser).await?;
    activate_codex(profile, Caller::Login, false)
        .await
        .map(drop)
}

pub(crate) async fn login_grok<F: AsFd>(
    profile: &Profile,
    output: &mut (dyn Write + Send),
    open_browser: bool,
    input: &F,
) -> Result<(), ActivationFailure> {
    let oauth = profile
        .grok_oauth()
        .map_err(|error| ActivationFailure::Detail(ofx_auth::grok_login_failure_detail(error)))?;
    oauth
        .run_login(
            output,
            open_browser,
            &tokio_util::sync::CancellationToken::new(),
            input,
        )
        .await
        .map_err(|error| ActivationFailure::Detail(ofx_auth::grok_login_failure_detail(error)))
}

pub(crate) fn logout(provider: Option<&ProviderId>) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    if host_managed() {
        return print(&format!("{HOST_MANAGED_AUTH_MESSAGE}\n"));
    }
    if provider == Some(&ProviderId::Grok) {
        let profile = Profile::from_environment();
        return if let Ok(runtime) = runtime() {
            runtime.block_on(logout_grok(&profile, &mut io::stdout(), &mut io::stderr()))
        } else {
            let _ = io::stderr()
                .write_all(b"oh-fx logout: failed to durably remove saved Grok login\n");
            ExitCode::FAILURE
        };
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

async fn logout_grok(
    profile: &Profile,
    output: &mut dyn Write,
    errors: &mut dyn Write,
) -> ExitCode {
    let failed = "oh-fx logout: failed to durably remove saved Grok login\n";
    let result = match profile.grok_oauth() {
        Ok(oauth) => oauth.logout().await,
        Err(error) => Err(error),
    };
    let Ok(result) = result else {
        let _ = errors.write_all(failed.as_bytes());
        return ExitCode::FAILURE;
    };
    if result.revocation_failed && errors.write_all(b"oh-fx logout: local Grok session removed, but remote revocation could not be confirmed\n").is_err() { return ExitCode::FAILURE; }
    let text = match result.deletion {
        DeleteOutcome::Deleted => "Signed out of Grok.\n",
        DeleteOutcome::Missing => "No Grok login session found.\n",
        DeleteOutcome::DeletedNotDurable => {
            let _ = errors.write_all(failed.as_bytes());
            return ExitCode::FAILURE;
        }
    };
    if output.write_all(text.as_bytes()).is_ok() {
        ExitCode::SUCCESS
    } else {
        let _ = errors.write_all(b"oh-fx: WriteFailed\n");
        ExitCode::FAILURE
    }
}

pub(crate) fn open_browser() -> bool {
    env::var_os(NO_OPEN_BROWSER_VARIABLE).is_none()
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

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, ChatGptError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ChatGptError::OAuthTransportUnavailable)
}

#[cfg(test)]
pub(crate) mod tests;
