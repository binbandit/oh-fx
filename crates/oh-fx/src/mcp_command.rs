use std::fmt::Write as _;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Arc;

use ofx_app::Profile;
use ofx_mcp::{
    AuthenticationOutcome, LogoutResult, McpError, McpRuntime, ShutdownMode, StartupPhase,
    authentication_error_message,
};
use ofx_text::encode_terminal_safe;
use tokio_util::sync::CancellationToken;

const NAME_BYTES: usize = 160;
const NO_SERVERS: &str = "No MCP servers configured.\n";

pub(crate) fn auth(name: &str) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let runtime = match load_runtime() {
        Ok(runtime) => runtime,
        Err(error) => return failed("auth", &error),
    };
    let Ok(tokio) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return failed("auth", "RuntimeUnavailable");
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
    match tokio.block_on(runtime.authenticate_server_for_later_sessions(name, &open, &cancel)) {
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
        Ok(AuthenticationOutcome::IssuerMismatch(_)) => failed(
            "auth",
            &McpError::McpAuthorizationIssuerMismatch.to_string(),
        ),
        Err(error) => failed("auth", &authentication_error_message(&error)),
    }
}

pub(crate) fn list(connect: bool) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let runtime = match load_command_runtime() {
        Ok(Some(runtime)) => runtime,
        Ok(None) => return written(NO_SERVERS),
        Err(error) => return failed("list", &error),
    };
    let Ok(tokio) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return failed("list", "RuntimeUnavailable");
    };
    let listing = tokio.block_on(async {
        let listed = if connect {
            runtime.connect(StartupPhase::All).await;
            Ok(runtime.render_health())
        } else {
            runtime
                .load_stored_credentials_for_health()
                .await
                .map(|()| runtime.render_health())
        };
        runtime.shutdown(ShutdownMode::Immediate).await;
        listed
    });
    match listing {
        Ok(listing) => written(&listing),
        Err(error) => failed("list", &error.to_string()),
    }
}

fn load_runtime() -> Result<Arc<McpRuntime>, String> {
    load_command_runtime()?.ok_or_else(|| McpError::McpServerNotFound.to_string())
}

fn load_command_runtime() -> Result<Option<Arc<McpRuntime>>, String> {
    let profile = Profile::load().map_err(|error| error.to_string())?;
    let runtime = profile
        .mcp_command_runtime()
        .map_err(|error| error.to_string())?;
    let mut stderr = io::stderr().lock();
    for diagnostic in profile.settings().diagnostics() {
        let _ = writeln!(stderr, "oh-fx: {diagnostic}");
    }
    drop(stderr);
    Ok(runtime)
}

fn written(text: &str) -> ExitCode {
    match crate::write_stdout(text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => crate::write_failed(),
    }
}

pub(crate) fn logout(name: &str) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    let runtime = match load_runtime() {
        Ok(runtime) => runtime,
        Err(error) => return failed("logout", &error),
    };
    let Ok(tokio) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return failed("logout", "RuntimeUnavailable");
    };
    match tokio.block_on(runtime.logout_server(name)) {
        Ok(result) => match crate::write_stdout(&logged_out(name, result)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => crate::write_failed(),
        },
        Err(error) => failed("logout", &authentication_error_message(&error)),
    }
}

fn logged_out(name: &str, result: LogoutResult) -> String {
    let shown = encode_terminal_safe(name.as_bytes(), NAME_BYTES).text;
    if !result.removed {
        format!("No stored MCP credentials found for '{shown}'.\n")
    } else if result.local_only {
        format!("Logged out of MCP server '{shown}' locally.\n")
    } else if result.revocation_failed {
        format!("Logged out of MCP server '{shown}' locally; remote revocation failed.\n")
    } else {
        format!("Logged out of MCP server '{shown}'.\n")
    }
}

fn failed(operation: &str, message: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "oh-fx mcp {operation} failed: {message}.");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logout_lines_follow_upstreams_precedence_with_a_terminal_safe_name() {
        let removed = LogoutResult {
            removed: true,
            ..LogoutResult::default()
        };
        assert_eq!(
            logged_out("docs", LogoutResult::default()),
            "No stored MCP credentials found for 'docs'.\n"
        );
        assert_eq!(
            logged_out(
                "docs",
                LogoutResult {
                    local_only: true,
                    revocation_failed: true,
                    repaired_entries: 2,
                    ..removed
                }
            ),
            "Logged out of MCP server 'docs' locally.\n"
        );
        assert_eq!(
            logged_out(
                "docs",
                LogoutResult {
                    revocation_failed: true,
                    ..removed
                }
            ),
            "Logged out of MCP server 'docs' locally; remote revocation failed.\n"
        );
        assert_eq!(
            logged_out(
                "docs",
                LogoutResult {
                    repaired_entries: 3,
                    ..removed
                }
            ),
            "Logged out of MCP server 'docs'.\n"
        );
        assert_eq!(
            logged_out("a\u{1b}[2Jb", removed),
            "Logged out of MCP server 'a\\x1b[2Jb'.\n"
        );
        assert_eq!(
            logged_out(&"x".repeat(200), removed),
            format!("Logged out of MCP server '{}...'.\n", "x".repeat(157))
        );
    }
}
