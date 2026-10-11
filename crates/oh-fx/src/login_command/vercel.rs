use std::io::Write;
use std::process::ExitCode;

use ofx_auth::LogoutResult;
use ofx_config::clear_credential_source;
use ofx_trace::trace_log;

use super::runtime;
use crate::provider_activation::Profile;

const AUTH: &str = "auth";
const DELETE_FAILED: &str = "oh-fx logout: failed to durably remove saved oh-fx login\n";
const SELECTION_NOT_CLEARED: &str =
    "oh-fx logout: signed out, but failed to clear the saved oh-fx login selection\n";
const REVOCATION_WARNING: &str =
    "Warning: signed out locally, but the remote session could not be revoked.\n";

pub(super) fn logout(
    profile: &Profile,
    output: &mut dyn Write,
    errors: &mut dyn Write,
) -> ExitCode {
    let result = profile.vercel_oauth().and_then(|oauth| {
        runtime()
            .ok()
            .and_then(|runtime| runtime.block_on(oauth.logout()).ok())
    });
    let Some(result) = result else {
        let _ = errors.write_all(DELETE_FAILED.as_bytes());
        return ExitCode::FAILURE;
    };
    report(profile, result, output, errors)
}

fn report(
    profile: &Profile,
    result: LogoutResult,
    output: &mut dyn Write,
    errors: &mut dyn Write,
) -> ExitCode {
    if !result.local_durability_failed {
        let cleared = profile
            .paths
            .as_ref()
            .map_or(Ok(()), clear_credential_source);
        if let Err(error) = cleared {
            trace_log!(
                AUTH,
                "logout credential preference clear failed err={error}"
            );
            let _ = errors.write_all(SELECTION_NOT_CLEARED.as_bytes());
            return ExitCode::FAILURE;
        }
    }
    if result.local_durability_failed {
        let _ = errors.write_all(DELETE_FAILED.as_bytes());
    } else {
        let text: &[u8] = if result.session_deleted {
            b"Signed out of oh-fx.\n"
        } else {
            b"No oh-fx login session found.\n"
        };
        if output.write_all(text).is_err() {
            let _ = errors.write_all(b"oh-fx: WriteFailed\n");
            return ExitCode::FAILURE;
        }
    }
    if result.remote_revocation_failed {
        let _ = errors.write_all(REVOCATION_WARNING.as_bytes());
    }
    if result.local_durability_failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests;
