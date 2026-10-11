use std::fmt::Write as _;
use std::path::Path;

use ofx_contract::{Notice, NoticeTone};
use ofx_mcp::{
    AuthenticationOutcome, IssuerMismatch, IssuerMismatchSource, LogoutResult, McpError,
    authentication_error_message,
};
use ofx_text::encode_terminal_safe;

use crate::app_mcp_runtime::TOPIC;

const ISSUER_BYTES: usize = 1024;

pub(crate) fn waiting(name: &str) -> String {
    format!(
        "Waiting for MCP authentication for '{name}'. You can continue using oh-fx while the browser flow completes."
    )
}

pub(crate) fn failed(name: &str, error: &McpError) -> String {
    format!("MCP authentication for '{name}' failed: {error}.")
}

pub(crate) fn busy(name: &str) -> String {
    format!(
        "MCP authentication for '{name}' is already in progress or MCP configuration is reloading."
    )
}

pub(crate) fn logout_busy(name: &str) -> String {
    format!(
        "MCP authentication for '{name}' is still in progress. Wait for it to finish before logging out."
    )
}

pub(crate) fn logout_completion(name: &str, result: &Result<LogoutResult, McpError>) -> String {
    let result = match result {
        Ok(result) => result,
        Err(error) => return format!("MCP logout for '{name}' failed: {error}."),
    };
    if !result.removed {
        return format!("No stored MCP credentials found for '{name}'.");
    }
    let mut body = if result.revocation_failed {
        format!("Logged out of MCP server '{name}' locally; remote revocation failed.")
    } else {
        format!("Logged out of MCP server '{name}'.")
    };
    if result.repaired_entries > 0 {
        let noun = if result.repaired_entries == 1 {
            "entry"
        } else {
            "entries"
        };
        let _ = write!(
            body,
            " Removed {} unreadable MCP credential {noun}.",
            result.repaired_entries
        );
    }
    body
}

pub(crate) fn completion(
    name: &str,
    result: Result<AuthenticationOutcome, McpError>,
    reconnect: Option<McpError>,
    profile_path: Option<&Path>,
) -> Notice {
    let (tone, body) = match result {
        Ok(AuthenticationOutcome::Authenticated { repaired_entries }) => {
            let mut success = format!("Authenticated MCP server '{name}'.");
            if repaired_entries > 0 {
                let noun = if repaired_entries == 1 {
                    "entry"
                } else {
                    "entries"
                };
                let _ = write!(
                    success,
                    "\nRemoved {repaired_entries} unreadable MCP credential {noun}."
                );
            }
            match reconnect {
                Some(error) => (
                    NoticeTone::Warning,
                    format!(
                        "{success}\nThe server could not reconnect: {error}. Check /mcp list for details."
                    ),
                ),
                None => (NoticeTone::Neutral, success),
            }
        }
        Ok(AuthenticationOutcome::IssuerMismatch(mismatch)) => (
            NoticeTone::Warning,
            issuer_mismatch(name, &mismatch, profile_path),
        ),
        Err(McpError::Cancelled) => (
            NoticeTone::Warning,
            format!("MCP authentication for '{name}' was cancelled."),
        ),
        Err(error) => (
            NoticeTone::Warning,
            format!(
                "MCP authentication for '{name}' failed: {}.",
                authentication_error_message(&error)
            ),
        ),
    };
    Notice::new(tone, TOPIC, body)
}

fn issuer_mismatch(name: &str, mismatch: &IssuerMismatch, profile_path: Option<&Path>) -> String {
    let expected = quoted(&mismatch.expected);
    let returned = quoted(&mismatch.returned);
    match mismatch.source {
        IssuerMismatchSource::AuthorizationMetadata => {
            let profile = profile_path
                .map_or_else(|| "mcp.json".to_owned(), |path| path.display().to_string());
            format!(
                "MCP authentication for '{name}' was rejected: expected issuer {expected} but metadata returned {returned}. Add \"oauth\":{{\"issuer\":{returned}}} to this server's entry in {profile} and retry."
            )
        }
        IssuerMismatchSource::AuthorizationResponse => format!(
            "MCP authentication for '{name}' was rejected: expected issuer {expected} but the authorization response returned issuer {returned}. oh-fx stopped before token exchange. Contact the MCP server provider; changing oauth.issuer is not a safe workaround."
        ),
    }
}

fn quoted(issuer: &str) -> String {
    let safe = encode_terminal_safe(issuer.as_bytes(), ISSUER_BYTES).text;
    serde_json::to_string(&safe).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(
        result: Result<AuthenticationOutcome, McpError>,
        reconnect: Option<McpError>,
    ) -> (NoticeTone, String) {
        let notice = completion(
            "docs",
            result,
            reconnect,
            Some(Path::new("/home/me/.config/oh-fx/mcp.json")),
        );
        assert_eq!(notice.topic, TOPIC);
        (notice.tone, notice.body)
    }

    #[test]
    fn completions_follow_upstreams_wording_with_oh_fx_names() {
        let authenticated =
            |repaired_entries| Ok(AuthenticationOutcome::Authenticated { repaired_entries });
        assert_eq!(
            body(authenticated(0), None),
            (
                NoticeTone::Neutral,
                "Authenticated MCP server 'docs'.".to_owned()
            )
        );
        assert_eq!(
            body(authenticated(1), None).1,
            "Authenticated MCP server 'docs'.\nRemoved 1 unreadable MCP credential entry."
        );
        assert_eq!(
            body(authenticated(2), Some(McpError::McpConnectionClosed)),
            (
                NoticeTone::Warning,
                "Authenticated MCP server 'docs'.\nRemoved 2 unreadable MCP credential entries.\nThe server could not reconnect: McpConnectionClosed. Check /mcp list for details.".to_owned()
            )
        );
        assert_eq!(
            body(Err(McpError::Cancelled), None),
            (
                NoticeTone::Warning,
                "MCP authentication for 'docs' was cancelled.".to_owned()
            )
        );
        assert_eq!(
            body(Err(McpError::McpAuthorizationCallbackTimedOut), None).1,
            "MCP authentication for 'docs' failed: Authorization timed out. Run the connection command again and finish authorization in your browser while oh-fx stays open."
        );
        assert_eq!(
            body(Err(McpError::McpAuthorityChanged), None).1,
            "MCP authentication for 'docs' failed: McpAuthorityChanged."
        );
    }

    #[test]
    fn issuer_mismatches_quote_terminal_safe_issuers() {
        let mismatch = |source| {
            Ok(AuthenticationOutcome::IssuerMismatch(IssuerMismatch {
                source,
                expected: "https://issuer.example".to_owned(),
                returned: "https://evil.example/\u{1b}[2J\"x".to_owned(),
            }))
        };
        let (tone, metadata) = body(mismatch(IssuerMismatchSource::AuthorizationMetadata), None);
        assert_eq!(tone, NoticeTone::Warning);
        let returned = quoted("https://evil.example/\u{1b}[2J\"x");
        assert_eq!(returned, r#""https://evil.example/\\x1b[2J\"x""#);
        assert_eq!(
            metadata,
            format!(
                "MCP authentication for 'docs' was rejected: expected issuer \"https://issuer.example\" but metadata returned {returned}. Add \"oauth\":{{\"issuer\":{returned}}} to this server's entry in /home/me/.config/oh-fx/mcp.json and retry."
            )
        );
        assert_eq!(
            body(mismatch(IssuerMismatchSource::AuthorizationResponse), None).1,
            format!(
                "MCP authentication for 'docs' was rejected: expected issuer \"https://issuer.example\" but the authorization response returned issuer {returned}. oh-fx stopped before token exchange. Contact the MCP server provider; changing oauth.issuer is not a safe workaround."
            )
        );
    }

    #[test]
    fn logout_notices_follow_upstreams_wording() {
        let removed = LogoutResult {
            removed: true,
            ..LogoutResult::default()
        };
        let cases = [
            (
                Ok(LogoutResult::default()),
                "No stored MCP credentials found for 'docs'.",
            ),
            (Ok(removed), "Logged out of MCP server 'docs'."),
            (
                Ok(LogoutResult {
                    local_only: true,
                    ..removed
                }),
                "Logged out of MCP server 'docs'.",
            ),
            (
                Ok(LogoutResult {
                    revocation_failed: true,
                    ..removed
                }),
                "Logged out of MCP server 'docs' locally; remote revocation failed.",
            ),
            (
                Ok(LogoutResult {
                    repaired_entries: 1,
                    ..removed
                }),
                "Logged out of MCP server 'docs'. Removed 1 unreadable MCP credential entry.",
            ),
            (
                Ok(LogoutResult {
                    repaired_entries: 2,
                    revocation_failed: true,
                    ..removed
                }),
                "Logged out of MCP server 'docs' locally; remote revocation failed. Removed 2 unreadable MCP credential entries.",
            ),
            (
                Err(McpError::McpAuthorityChanged),
                "MCP logout for 'docs' failed: McpAuthorityChanged.",
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(logout_completion("docs", &result), expected);
        }
        assert_eq!(
            logout_busy("docs"),
            "MCP authentication for 'docs' is still in progress. Wait for it to finish before logging out."
        );
    }
}
