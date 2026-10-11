use std::io::{self, Write};
use std::os::fd::AsFd;
use std::process::ExitCode;

use ofx_auth::{HOST_MANAGED_AUTH_MESSAGE, LoginTerminal, VercelError, host_managed_auth};

use crate::login_command::runtime;
use crate::provider_activation::{ActivationFailure, Profile, activate_gateway};

enum TeamsFailure {
    Switch(VercelError),
    Activation(ActivationFailure),
}

pub(crate) fn run() -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    if host_managed_auth() {
        return crate::print(
            format!("{HOST_MANAGED_AUTH_MESSAGE}\n").as_bytes(),
            crate::command_write_failure(ofx_cli::TopLevelKind::Teams),
        );
    }
    let profile = Profile::from_environment();
    profile.configure_trace();
    let switched = runtime()
        .map_err(|_| TeamsFailure::Switch(VercelError::OAuthTransportUnavailable))
        .and_then(|runtime| {
            runtime.block_on(switch_team(
                &profile,
                &mut io::stdout(),
                &io::stdin(),
                LoginTerminal::from_process(),
            ))
        });
    match switched {
        Ok(()) => ExitCode::SUCCESS,
        Err(TeamsFailure::Switch(error)) => {
            let _ = writeln!(io::stderr(), "oh-fx teams: {}", failure_detail(error));
            ExitCode::FAILURE
        }
        Err(TeamsFailure::Activation(failure)) => {
            failure.report("login");
            ExitCode::FAILURE
        }
    }
}

async fn switch_team<F: AsFd>(
    profile: &Profile,
    output: &mut dyn Write,
    input: &F,
    terminal: LoginTerminal,
) -> Result<(), TeamsFailure> {
    let oauth = profile.vercel_oauth().ok_or(TeamsFailure::Switch(
        VercelError::CredentialStorageUnavailable,
    ))?;
    oauth
        .switch_team(output, input, terminal)
        .await
        .map_err(TeamsFailure::Switch)?;
    activate_gateway(profile)
        .await
        .map_err(TeamsFailure::Activation)
}

const fn failure_detail(error: VercelError) -> &'static str {
    match error {
        VercelError::NoSession => "run oh-fx login first",
        VercelError::SessionChanged => "authentication changed; try again",
        VercelError::TeamRequestFailed => "failed to list Vercel teams",
        VercelError::InvalidTeamSelection => "no team selected",
        VercelError::AccessDenied => "authorization denied",
        _ => "failed to switch team",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_switch_failures_use_upstreams_details() {
        for (error, detail) in [
            (VercelError::NoSession, "run oh-fx login first"),
            (
                VercelError::SessionChanged,
                "authentication changed; try again",
            ),
            (
                VercelError::TeamRequestFailed,
                "failed to list Vercel teams",
            ),
            (VercelError::InvalidTeamSelection, "no team selected"),
            (VercelError::AccessDenied, "authorization denied"),
            (VercelError::NoTeams, "failed to switch team"),
            (VercelError::InvalidGrant, "failed to switch team"),
            (VercelError::InvalidTeamsResponse, "failed to switch team"),
        ] {
            assert_eq!(failure_detail(error), detail, "{error}");
        }
    }
}
