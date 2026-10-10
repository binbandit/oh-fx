use std::fmt;

use ofx_contract::{
    BoxFuture, Completion, FinishReason, ModelProvider, ModelRequest, ProviderError,
    ProviderErrorKind, ProviderReplay, StreamSink, ToolCall,
};
use ofx_http::{ClientError, ConnectionOptions, build_connection_client};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::chat_completions::sanitized;
use crate::client::{FailureCause, Finish, GatewayAttempt, GatewayCompletion};
use crate::vercel_protocol::{RequestError, build_request, replay_source, select_replay_parts};

const CHAT_URL: &str = "https://ai-gateway.vercel.sh/v4/ai/language-model";
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayEndpoints {
    pub chat: String,
}

impl Default for GatewayEndpoints {
    fn default() -> Self {
        Self {
            chat: CHAT_URL.to_owned(),
        }
    }
}

pub struct GatewayCredential {
    secret: Option<Zeroizing<String>>,
    team: Option<String>,
}

impl GatewayCredential {
    pub fn new(secret: Option<String>, team: Option<String>) -> Self {
        Self {
            secret: secret.map(Zeroizing::new),
            team,
        }
    }
}

impl fmt::Debug for GatewayCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayCredential")
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("team", &self.team)
            .finish()
    }
}

pub struct GatewayProvider {
    client: reqwest::Client,
    chat_url: String,
    secrets: Zeroizing<Vec<String>>,
    team: Option<String>,
    user_agent: String,
}

impl fmt::Debug for GatewayProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayProvider")
            .field("chat_url", &self.chat_url)
            .finish_non_exhaustive()
    }
}

impl GatewayProvider {
    pub fn new(
        credential: GatewayCredential,
        user_agent: &str,
        endpoints: GatewayEndpoints,
    ) -> Result<Self, ClientError> {
        let client = build_connection_client(&ConnectionOptions {
            user_agent: user_agent.to_owned(),
            follow_redirects: false,
            ..ConnectionOptions::default()
        })?;
        let GatewayCredential { secret, team } = credential;
        let secrets = secret
            .filter(|secret| !secret.is_empty())
            .map(|secret| secret.to_string())
            .into_iter()
            .collect();
        Ok(Self {
            client,
            chat_url: endpoints.chat,
            secrets: Zeroizing::new(secrets),
            team,
            user_agent: user_agent.to_owned(),
        })
    }

    async fn complete(
        &self,
        request: &ModelRequest<'_>,
        body: Option<String>,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> Result<Completion, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let payload = match body {
            Some(body) => body,
            None => build_request(request, &self.user_agent).map_err(request_failure)?,
        };
        let attempt = GatewayAttempt {
            client: &self.client,
            url: &self.chat_url,
            api_key: self.secrets.first().map(String::as_str),
            team: self.team.as_deref(),
            session_id: request.session_id,
            model: request.model,
            secrets: &self.secrets,
        };
        let completion = attempt.stream(payload, sink, cancel).await?;
        into_completion(completion, request.model, &self.secrets)
    }
}

impl ModelProvider for GatewayProvider {
    fn stream<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        Box::pin(self.complete(request, None, sink, cancel))
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        build_request(request, &self.user_agent).ok()
    }

    fn stream_body<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        body: String,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        Box::pin(self.complete(request, Some(body), sink, cancel))
    }

    fn project_replay(
        &self,
        replay: &ProviderReplay,
        calls: &[ToolCall],
        text: bool,
        reasoning: bool,
    ) -> Result<Option<ProviderReplay>, ProviderError> {
        select_replay_parts(replay, calls, text, reasoning).map_err(request_failure)
    }
}

fn request_failure(error: RequestError) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Protocol, error.to_string())
}

fn into_completion(
    completion: GatewayCompletion,
    model: &str,
    secrets: &[String],
) -> Result<Completion, ProviderError> {
    let detail = completion.failure_detail.as_deref();
    let timed_out = completion.failure_cause == Some(FailureCause::GatewayStreamTimeout);
    let finish_reason = match completion.finish {
        None => {
            let shown = detail.map_or_else(
                || {
                    format!(
                        "the stream ended before its finish event after {} events",
                        completion.events
                    )
                },
                |detail| format!("provider error: {detail}"),
            );
            let (kind, fallback) = if timed_out {
                (ProviderErrorKind::StreamStalled, "gateway_stream_timeout")
            } else {
                (ProviderErrorKind::TransportInterrupted, "StreamInterrupted")
            };
            return Err(failure(
                kind,
                "StreamInterrupted",
                shown,
                diagnostic(detail, fallback),
                secrets,
            ));
        }
        Some(Finish::Length) => {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "OutputTruncated",
            ));
        }
        Some(Finish::ContentFilter) => {
            let mut error = ProviderError::new(ProviderErrorKind::ProviderError, "ContentFiltered");
            error.diagnostic = Some(sanitized(diagnostic(detail, "content_filter"), secrets));
            return Err(error);
        }
        Some(Finish::ProviderError) => {
            let (kind, fallback) = if timed_out {
                (ProviderErrorKind::StreamStalled, "gateway_stream_timeout")
            } else {
                (ProviderErrorKind::ServerError, "provider_error")
            };
            let shown = diagnostic(detail, fallback);
            return Err(failure(
                kind,
                "ProviderError",
                format!("provider error: {shown}"),
                shown,
                secrets,
            ));
        }
        Some(finish) => match (finish, completion.tools.count()) {
            (Finish::ToolCalls, 1..) => FinishReason::ToolCalls,
            (Finish::Stop | Finish::Other, 0) => FinishReason::Stop,
            (Finish::Stop, _) if completion.tools.all_provider_executed() => FinishReason::Stop,
            _ => {
                return Err(ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "InvalidProviderCompletion",
                ));
            }
        },
    };
    completion
        .tools
        .admission()
        .map_err(|code| ProviderError::new(ProviderErrorKind::Protocol, code))?;
    let GatewayCompletion {
        content,
        usage,
        tools,
        replay,
        ..
    } = completion;
    Ok(Completion {
        content: (!content.is_empty()).then_some(content),
        tool_calls: tools.into_calls(),
        finish_reason,
        usage,
        provider_replay: replay.map(|parts_json| ProviderReplay {
            source: replay_source(model),
            parts_json,
        }),
    })
}

fn failure(
    kind: ProviderErrorKind,
    code: &str,
    detail: String,
    diagnostic: String,
    secrets: &[String],
) -> ProviderError {
    let mut error = ProviderError::new(kind, code).with_detail(sanitized(detail, secrets));
    error.diagnostic = Some(sanitized(diagnostic, secrets));
    error
}

fn diagnostic(detail: Option<&str>, fallback: &str) -> String {
    match detail.map(|detail| detail.trim_matches(TRIMMED)) {
        Some(detail) if !detail.is_empty() && has_identifier(detail) => detail.to_owned(),
        Some(detail) if !detail.is_empty() => format!("{fallback}: {detail}"),
        _ => fallback.to_owned(),
    }
}

fn has_identifier(text: &str) -> bool {
    if let Some(separator) = text.find(": ") {
        return separator > 0 && !text[..separator].contains(TRIMMED);
    }
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests;
