use std::fmt::Write;

use ofx_text::{encode_terminal_safe, mask_secrets};
use serde_json::{Map, Value};

const MAX_PUBLISHED_ERROR_BYTES: usize = 1024;
const MAX_RECOVERY_DIAGNOSTIC_BYTES: usize = 600;
const PROVIDERS_CONSIDERED: &str = "Providers considered: ";

pub(crate) fn format_http_error_message(status: u16, detail: &str) -> String {
    let title = if status == 401 || status == 403 {
        "API access denied"
    } else {
        "API request failed"
    };
    format_http_diagnostic(status, detail, Some(title), MAX_PUBLISHED_ERROR_BYTES)
}

pub(crate) fn format_http_recovery_diagnostic(status: u16, detail: &str) -> String {
    format_http_diagnostic(status, detail, None, MAX_RECOVERY_DIAGNOSTIC_BYTES)
}

pub(crate) fn sanitize_external_text(raw: &str, max_bytes: usize) -> String {
    encode_terminal_safe(mask_secrets(raw).as_bytes(), max_bytes).text
}

fn format_http_diagnostic(
    status: u16,
    detail: &str,
    title: Option<&str>,
    max_bytes: usize,
) -> String {
    if detail.is_empty() {
        return format!("HTTP {status}");
    }
    let parsed: Option<Value> = serde_json::from_str(detail).ok();
    let Some(failure) = parsed
        .as_ref()
        .and_then(Value::as_object)
        .and_then(failure_fields)
    else {
        let line = format!("HTTP {status}: {}", mask_secrets(detail));
        return encode_terminal_safe(line.as_bytes(), max_bytes).text;
    };
    let provider = failure
        .provider
        .or_else(|| failure.message.and_then(provider_from_message))
        .map(mask_secrets);
    let code = failure.code.map(mask_secrets);
    let message = failure.message.map(mask_secrets);
    let mut line = String::new();
    if let Some(title) = title {
        let _ = write!(line, "{title} · ");
    }
    let _ = write!(line, "HTTP {status}");
    if let Some(provider) = provider {
        let _ = write!(line, " · Provider: {provider}");
    }
    match (code, message) {
        (Some(code), Some(message)) if code != message => {
            let _ = write!(line, " · {code}: {message}");
        }
        (Some(code), _) => {
            let _ = write!(line, " · {code}");
        }
        (None, Some(message)) => {
            let _ = write!(line, " · {message}");
        }
        (None, None) => {}
    }
    encode_terminal_safe(line.as_bytes(), max_bytes).text
}

struct FailureFields<'a> {
    code: Option<&'a str>,
    message: Option<&'a str>,
    provider: Option<&'a str>,
}

fn failure_fields(root: &Map<String, Value>) -> Option<FailureFields<'_>> {
    if let Some(error) = root.get("error").and_then(Value::as_object) {
        let param = error.get("param").and_then(Value::as_object);
        return Some(FailureFields {
            code: string_field(error, "code")
                .or_else(|| string_field(error, "type"))
                .or_else(|| param.and_then(|param| string_field(param, "name"))),
            message: string_field(error, "message")
                .or_else(|| param.and_then(|param| string_field(param, "message"))),
            provider: string_field(error, "provider"),
        });
    }
    let message = ["message", "detail"]
        .into_iter()
        .find_map(|key| string_field(root, key).filter(|text| !text.trim().is_empty()))?;
    Some(FailureFields {
        code: None,
        message: Some(message),
        provider: None,
    })
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn provider_from_message(message: &str) -> Option<&str> {
    let start = message.find(PROVIDERS_CONSIDERED)? + PROVIDERS_CONSIDERED.len();
    let rest = message[start..].trim_start_matches([' ', '\t', '\r', '\n']);
    let end = rest.find(['.', '\n', '\r', '\t']).unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_http_error_message_renders_restricted_provider_details_without_raw_json() {
        let detail = r#"{"error":{"code":"RestrictedProvidersError","message":"demo account is restricted from provider wafer","provider":"wafer"}}"#;
        assert_eq!(
            format_http_error_message(403, detail),
            "API access denied · HTTP 403 · Provider: wafer · RestrictedProvidersError: demo account is restricted from provider wafer"
        );
    }

    #[test]
    fn format_http_error_message_renders_live_restricted_provider_body_shape() {
        let detail = r#"{"error":{"message":"Your team has restricted access to this provider. Contact the owner of the account for more details. Providers considered: wafer","type":"no_providers_available","param":{"name":"RestrictedProvidersError","message":"Your team has restricted access to this provider. Contact the owner of the account for more details. Providers considered: wafer"}},"providerMetadata":{"gateway":{"routing":{}}}}"#;
        assert_eq!(
            format_http_error_message(403, detail),
            "API access denied · HTTP 403 · Provider: wafer · no_providers_available: Your team has restricted access to this provider. Contact the owner of the account for more details. Providers considered: wafer"
        );
    }

    #[test]
    fn format_http_error_message_renders_api_key_and_credits_setup_bodies() {
        assert_eq!(
            format_http_error_message(
                401,
                r#"{"error":{"code":"api_key_required","message":"Set AI_GATEWAY_API_KEY to use this endpoint."}}"#
            ),
            "API access denied · HTTP 401 · api_key_required: Set AI_GATEWAY_API_KEY to use this endpoint."
        );
        assert_eq!(
            format_http_error_message(
                403,
                r#"{"error":{"code":"credit_card_required","message":"Buy credits to use AI Gateway."}}"#
            ),
            "API access denied · HTTP 403 · credit_card_required: Buy credits to use AI Gateway."
        );
    }

    #[test]
    fn plain_bodies_and_empty_bodies_keep_the_status() {
        assert_eq!(format_http_error_message(502, ""), "HTTP 502");
        assert_eq!(
            format_http_error_message(502, "upstream\nfailed"),
            "HTTP 502: upstream\\x0afailed"
        );
        assert_eq!(
            format_http_error_message(500, r#"{"error":{"message":"boom"}}"#),
            "API request failed · HTTP 500 · boom"
        );
        let long = format_http_error_message(500, &"x".repeat(5000));
        assert_eq!(long.len(), MAX_PUBLISHED_ERROR_BYTES);
        assert!(long.ends_with("..."));
    }

    #[test]
    fn top_level_detail_and_message_bodies_render_like_error_objects() {
        assert_eq!(
            format_http_error_message(
                400,
                r#"{"detail":"The 'gpt-5.4' model is not supported when using Codex with a ChatGPT account."}"#
            ),
            "API request failed · HTTP 400 · The 'gpt-5.4' model is not supported when using Codex with a ChatGPT account."
        );
        assert_eq!(
            format_http_error_message(403, r#"{"message":"Forbidden"}"#),
            "API access denied · HTTP 403 · Forbidden"
        );
        assert_eq!(
            format_http_error_message(400, r#"{"message":"used","detail":"ignored"}"#),
            "API request failed · HTTP 400 · used"
        );
        assert_eq!(
            format_http_recovery_diagnostic(
                503,
                r#"{"detail":"Service temporarily unavailable. Providers considered: wafer"}"#
            ),
            "HTTP 503 · Provider: wafer · Service temporarily unavailable. Providers considered: wafer"
        );
        let masked = format_http_error_message(
            401,
            r#"{"detail":"Bad key sk-proj-abcdefghijklmnopqrstuvwxyz0123 \u001b[31m"}"#,
        );
        assert_eq!(
            masked,
            "API access denied · HTTP 401 · Bad key [redacted] \\x1b[31m"
        );
        for raw in [
            r#"{"detail":[{"loc":["body","model"],"msg":"field required"}]}"#,
            r#"{"detail":"  "}"#,
            r#"{"error":"Unauthorized"}"#,
            r#"["detail"]"#,
        ] {
            assert_eq!(
                format_http_error_message(422, raw),
                format!("HTTP 422: {raw}")
            );
        }
    }

    #[test]
    fn recovery_diagnostics_drop_the_title_and_use_the_smaller_bound() {
        assert_eq!(
            format_http_recovery_diagnostic(500, r#"{"error":{"message":"boom"}}"#),
            "HTTP 500 · boom"
        );
        assert_eq!(
            format_http_recovery_diagnostic(429, r#"{"error":{"message":"slow down"}}"#),
            "HTTP 429 · slow down"
        );
        assert_eq!(format_http_recovery_diagnostic(503, ""), "HTTP 503");
        let long = format_http_recovery_diagnostic(502, &"x".repeat(5000));
        assert_eq!(long.len(), MAX_RECOVERY_DIAGNOSTIC_BYTES);
    }

    #[test]
    fn provider_details_are_masked_and_made_terminal_safe() {
        let detail = r#"{"error":{"message":"Incorrect API key provided: sk-proj-abcdefghijklmnopqrstuvwxyz0123 \u202e\u001b[31mred"}}"#;
        let message = format_http_error_message(401, detail);
        assert!(
            !message.contains("sk-proj-abcdefghijklmnopqrstuvwxyz0123"),
            "{message}"
        );
        assert!(message.contains("[redacted]"), "{message}");
        assert!(
            !message.contains('\u{1b}') && !message.contains('\u{202e}'),
            "{message}"
        );
        assert!(
            message.contains("\\x1b") && message.contains("\\u{202e}"),
            "{message}"
        );
        let html = format_http_error_message(404, "<html>\n<title>Sign in</title>");
        assert_eq!(html, "HTTP 404: <html>\\x0a<title>Sign in</title>");
    }
}
