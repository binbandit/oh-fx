use std::time::Duration;

use ofx_contract::parse_strict_json_value;
use ofx_http::{ConnectionOptions, build_connection_client};
use ofx_trace::trace_log;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{BoundedFailure, bounded_get};
use crate::gateway_error_format::format_http_error_message;

pub const GATEWAY_CREDITS_PATH: &str = "/coding-agent/v1/credits";
const CREDITS_URL: &str = "https://ai-gateway.vercel.sh/coding-agent/v1/credits";
const CREDITS: &str = "credits";
const MAX_CREDITS_BYTES: usize = 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const FETCH_FAILED: &str = "failed to fetch credits from gateway";
const INVALID_JSON: &str = "invalid JSON response from gateway";
const UNEXPECTED_FORMAT: &str = "unexpected response format from gateway";
const MAX_TEAM_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayCreditsEndpoints {
    pub credits: String,
}

impl Default for GatewayCreditsEndpoints {
    fn default() -> Self {
        Self {
            credits: CREDITS_URL.to_owned(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreditsSnapshot {
    pub balance: Option<String>,
    pub used: Option<String>,
    pub plan: Option<String>,
    pub error: Option<String>,
}

impl CreditsSnapshot {
    pub fn failed(message: &str) -> Self {
        Self {
            error: Some(message.to_owned()),
            ..Self::default()
        }
    }
}

pub async fn fetch_credits(
    user_agent: &str,
    endpoints: &GatewayCreditsEndpoints,
    credential: Option<&str>,
    login_team: Option<&str>,
    cancel: &CancellationToken,
) -> CreditsSnapshot {
    let Ok(client) = build_connection_client(&ConnectionOptions {
        user_agent: user_agent.to_owned(),
        follow_redirects: false,
        ..ConnectionOptions::default()
    }) else {
        return CreditsSnapshot::failed(FETCH_FAILED);
    };
    let url = match login_team {
        Some(team) if url_safe_team(team) => {
            let separator = if endpoints.credits.contains('?') {
                '&'
            } else {
                '?'
            };
            format!("{}{separator}teamId={team}", endpoints.credits)
        }
        Some(team) => {
            trace_log!(
                CREDITS,
                "team omitted from query; not url safe len={}",
                team.len()
            );
            endpoints.credits.clone()
        }
        None => endpoints.credits.clone(),
    };
    let mut request = client.get(url);
    if let Some(credential) = credential {
        request = request.bearer_auth(credential);
    }
    let deadline = Instant::now() + FETCH_TIMEOUT;
    match bounded_get(request, MAX_CREDITS_BYTES, deadline, cancel).await {
        Ok((StatusCode::OK, body)) => snapshot(&body),
        Ok((status, body)) => {
            let secrets: Vec<String> = credential.map(str::to_owned).into_iter().collect();
            CreditsSnapshot::failed(&format_http_error_message(
                status.as_u16(),
                &String::from_utf8_lossy(&body),
                &secrets,
            ))
        }
        Err(BoundedFailure::Cancelled | BoundedFailure::Failed(_)) => {
            CreditsSnapshot::failed(FETCH_FAILED)
        }
    }
}

fn url_safe_team(team: &str) -> bool {
    !team.is_empty()
        && team.len() <= MAX_TEAM_BYTES
        && team
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn snapshot(body: &[u8]) -> CreditsSnapshot {
    let Ok(value) = parse_strict_json_value(body) else {
        return CreditsSnapshot::failed(INVALID_JSON);
    };
    let Value::Object(object) = value else {
        return CreditsSnapshot::failed(UNEXPECTED_FORMAT);
    };
    let field = |name: &str| object.get(name).and_then(Value::as_str).map(str::to_owned);
    CreditsSnapshot {
        balance: field("balance"),
        used: field("used"),
        plan: field("plan"),
        error: None,
    }
}

#[cfg(test)]
mod tests;
