use std::io::{self, Write};
use std::process::ExitCode;

use ofx_auth::login_failure_detail;
use ofx_cli::Command;
use ofx_config::ProviderId;

use crate::login_command::{host_managed, open_browser, runtime};
use crate::provider_activation::{Activation, ActivationFailure, Caller, Profile, activate_codex};

pub(crate) fn run(target: ProviderId) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    if target != ProviderId::Codex {
        return crate::not_available(&Command::Provider(target));
    }
    let profile = Profile::from_environment();
    let mut stdout = io::stdout();
    let activated = runtime()
        .map_err(|error| ActivationFailure::Detail(login_failure_detail(error)))
        .and_then(|runtime| {
            runtime.block_on(select_codex(
                &profile,
                &mut stdout,
                open_browser(),
                host_managed(),
            ))
        });
    match activated {
        Ok(text) => match crate::write_stdout(&text) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => crate::write_failed(),
        },
        Err(failure) => {
            failure.report("provider");
            ExitCode::FAILURE
        }
    }
}

pub(crate) async fn select_codex(
    profile: &Profile,
    output: &mut (dyn Write + Send),
    open_browser: bool,
    host_managed: bool,
) -> Result<String, ActivationFailure> {
    let caller = Caller::ProviderCommand {
        output,
        open_browser,
    };
    Ok(match activate_codex(profile, caller, host_managed).await? {
        Activation::AlreadySelected => "Codex is already selected.\n".to_owned(),
        Activation::Selected { signed_in: true } => {
            "Signed in with Codex.\nProvider set to Codex.\n".to_owned()
        }
        Activation::Selected { signed_in: false } => "Provider set to Codex.\n".to_owned(),
    })
}

#[cfg(test)]
mod tests;
