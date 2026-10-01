use std::env;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

use ofx_auth::{
    AuthMode, CHATGPT_SOURCE_LABEL, ChatGptEndpoints, ChatGptError, ChatGptOAuth, DeleteOutcome,
    HOST_MANAGED_AUTH_MESSAGE, login_failure_detail, parse_auth_mode, prepare_chatgpt_credential,
};
use ofx_config::{ProfilePaths, ProviderId};
use tokio_util::sync::CancellationToken;

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
    let signed_in = chatgpt_oauth().and_then(|oauth| {
        block_on(async {
            oauth
                .run_login(&mut io::stdout(), open_browser, &CancellationToken::new())
                .await?;
            Ok(prepare_chatgpt_credential(&oauth).await)
        })
    });
    let failure = match signed_in {
        Ok(Ok(Some(_))) => return print("Signed in with Codex.\n"),
        Ok(Ok(None)) => "Codex credential is unavailable".to_owned(),
        Ok(Err(error)) => format!("{CHATGPT_SOURCE_LABEL}: {}", error.notice()),
        Err(error) => login_failure_detail(error),
    };
    let _ = writeln!(io::stderr(), "oh-fx login: {failure}");
    ExitCode::FAILURE
}

pub(crate) fn logout(provider: Option<&ProviderId>) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    if host_managed() {
        return print(&format!("{HOST_MANAGED_AUTH_MESSAGE}\n"));
    }
    if provider != Some(&ProviderId::Codex) {
        return unavailable("logout");
    }
    let outcome = chatgpt_oauth().and_then(|oauth| block_on(async { oauth.logout().await }));
    match outcome {
        Ok(DeleteOutcome::Deleted) => print("Signed out of Codex.\n"),
        Ok(DeleteOutcome::Missing) => print("No Codex login session found.\n"),
        Ok(DeleteOutcome::DeletedNotDurable) | Err(_) => {
            let _ = io::stderr().write_all(LOGOUT_FAILURE.as_bytes());
            ExitCode::FAILURE
        }
    }
}

fn host_managed() -> bool {
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

fn chatgpt_oauth() -> Result<ChatGptOAuth, ChatGptError> {
    let paths =
        ProfilePaths::from_environment().ok_or(ChatGptError::CredentialStorageUnavailable)?;
    ChatGptOAuth::new(
        paths.data,
        &crate::user_agent(),
        ChatGptEndpoints::default(),
    )
}

fn block_on<T>(future: impl Future<Output = Result<T, ChatGptError>>) -> Result<T, ChatGptError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ChatGptError::OAuthTransportUnavailable)?
        .block_on(future)
}
