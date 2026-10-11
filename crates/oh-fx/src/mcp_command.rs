use std::fmt::Write as _;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Arc;

use ofx_app::Profile;
use ofx_mcp::{AuthenticationOutcome, McpError, McpRuntime};
use ofx_text::encode_terminal_safe;
use tokio_util::sync::CancellationToken;

const NAME_BYTES: usize = 160;
const CALLBACK_TIMED_OUT: &str = "Authorization timed out. Run the connection command again and finish authorization in your browser while oh-fx stays open";

pub(crate) fn auth(name: &str) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let runtime = match load_runtime() {
        Ok(runtime) => runtime,
        Err(error) => return failed(&error),
    };
    let Ok(tokio) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return failed("RuntimeUnavailable");
    };
    let open = |url: &str| {
        let shown = encode_terminal_safe(url.as_bytes(), usize::MAX).text;
        let _ = crate::write_stdout(&format!(
            "Open this URL to authenticate the MCP server:\n{shown}\n\nWaiting for browser authorization...\n"
        ));
        if ofx_auth::browser_allowed() {
            ofx_auth::open_url(url);
        }
        true
    };
    let cancel = CancellationToken::new();
    match tokio.block_on(runtime.authenticate_server(name, &open, &cancel)) {
        Ok(AuthenticationOutcome::Authenticated { repaired_entries }) => {
            let shown = encode_terminal_safe(name.as_bytes(), NAME_BYTES).text;
            let mut text = format!("Authenticated MCP server '{shown}'.");
            if repaired_entries > 0 {
                let noun = if repaired_entries == 1 {
                    "entry"
                } else {
                    "entries"
                };
                let _ = write!(
                    text,
                    " Removed {repaired_entries} unreadable MCP credential {noun}."
                );
            }
            text.push('\n');
            match crate::write_stdout(&text) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => crate::write_failed(),
            }
        }
        Ok(AuthenticationOutcome::IssuerMismatch(_)) => {
            failed(&McpError::McpAuthorizationIssuerMismatch.to_string())
        }
        Err(error) => failed(&message(&error)),
    }
}

fn load_runtime() -> Result<Arc<McpRuntime>, String> {
    let profile = Profile::load().map_err(|error| error.to_string())?;
    let runtime = profile
        .mcp_command_runtime()
        .map_err(|error| error.to_string())?;
    let mut stderr = io::stderr().lock();
    for diagnostic in profile.settings().diagnostics() {
        let _ = writeln!(stderr, "oh-fx: {diagnostic}");
    }
    drop(stderr);
    runtime.ok_or_else(|| McpError::McpServerNotFound.to_string())
}

fn message(error: &McpError) -> String {
    match error {
        McpError::McpAuthorizationCallbackTimedOut => CALLBACK_TIMED_OUT.to_owned(),
        _ => error.to_string(),
    }
}

fn failed(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "oh-fx mcp auth failed: {message}.");
    ExitCode::FAILURE
}
