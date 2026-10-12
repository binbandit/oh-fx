use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_contract::{
    BoxFuture, Completion, FinishReason, ModelProvider, ModelRequest, ProviderError,
    ProviderErrorKind, ProviderReplay, StreamEvent, StreamSink, ToolCall,
};
use ofx_http::{ClientError, ConnectionOptions, build_connection_client};
use ofx_images::AttachmentDimensionCache;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::chat_completions::sanitized;
use crate::client::{FailureCause, Finish, GatewayAttempt, GatewayCompletion};
use crate::vercel_protocol::{
    RequestError, build_request, build_review_request, replay_source, select_replay_parts,
};

pub const GATEWAY_TITLE_MODEL: &str = "openai/gpt-5.6-luna";
const CHAT_URL: &str = "https://ai-gateway.vercel.sh/v4/ai/language-model";
const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];
const REFRESHED_SOURCE: &str = "fx_login";
const AUTHORITY_CHANGED: &str = "CredentialAuthorityChanged";

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

pub struct GatewayAccess {
    token: Zeroizing<String>,
    team: Option<String>,
    refresh_after_ms: i64,
}

impl GatewayAccess {
    pub fn new(token: String, team: Option<String>, refresh_after_ms: i64) -> Self {
        Self {
            token: Zeroizing::new(token),
            team,
            refresh_after_ms,
        }
    }

    pub fn token(&self) -> &Zeroizing<String> {
        &self.token
    }

    pub fn team(&self) -> Option<&str> {
        self.team.as_deref()
    }

    pub const fn due_at(&self, now_ms: i64) -> bool {
        self.refresh_after_ms <= now_ms
    }
}

impl fmt::Debug for GatewayAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayAccess")
            .field("token", &"<redacted>")
            .field("team", &self.team)
            .field("refresh_after_ms", &self.refresh_after_ms)
            .finish()
    }
}

pub trait GatewayRefresh: Send + Sync {
    fn refresh<'a>(
        &'a self,
        forced: bool,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<GatewayAccess>, String>>;
}

fn rejected(error: &ProviderError) -> bool {
    error.kind == ProviderErrorKind::Unauthorized && error.status == Some(401)
}

pub struct GatewayCredential {
    secret: Option<Zeroizing<String>>,
    team: Option<String>,
    refresh: Option<(i64, Arc<dyn GatewayRefresh>)>,
}

impl GatewayCredential {
    pub fn new(secret: Option<String>, team: Option<String>) -> Self {
        Self {
            secret: secret.map(Zeroizing::new),
            team,
            refresh: None,
        }
    }

    #[must_use]
    pub fn refreshed_by(mut self, refresh_after_ms: i64, refresh: Arc<dyn GatewayRefresh>) -> Self {
        self.refresh = Some((refresh_after_ms, refresh));
        self
    }
}

impl fmt::Debug for GatewayCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayCredential")
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("team", &self.team)
            .field("refreshable", &self.refresh.is_some())
            .finish()
    }
}

struct Current {
    secret: Option<Zeroizing<String>>,
    team: Option<String>,
    refresh_after_ms: Option<i64>,
}

pub struct GatewayProvider {
    client: reqwest::Client,
    chat_url: String,
    current: Mutex<Current>,
    secrets: Mutex<Zeroizing<Vec<String>>>,
    refresh: Option<Arc<dyn GatewayRefresh>>,
    user_agent: String,
    attachments: AttachmentDimensionCache,
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
        let GatewayCredential {
            secret,
            team,
            refresh,
        } = credential;
        let secret = secret.filter(|secret| !secret.is_empty());
        let secrets = secret
            .iter()
            .map(|secret| secret.to_string())
            .collect::<Vec<_>>();
        let (refresh_after_ms, refresh) = refresh.unzip();
        Ok(Self {
            client,
            chat_url: endpoints.chat,
            current: Mutex::new(Current {
                secret,
                team,
                refresh_after_ms,
            }),
            secrets: Mutex::new(Zeroizing::new(secrets)),
            refresh,
            user_agent: user_agent.to_owned(),
            attachments: AttachmentDimensionCache::default(),
        })
    }

    fn masked(&self) -> Zeroizing<Vec<String>> {
        lock(&self.secrets).clone()
    }

    pub async fn current_access(&self, cancel: &CancellationToken) -> Option<GatewayAccess> {
        self.refresh_if_due(&mut |_: StreamEvent| {}, cancel).await;
        let current = lock(&self.current);
        Some(GatewayAccess {
            token: current.secret.clone()?,
            team: current.team.clone(),
            refresh_after_ms: current.refresh_after_ms.unwrap_or(i64::MAX),
        })
    }

    async fn refresh_if_due(&self, sink: &mut dyn StreamSink, cancel: &CancellationToken) {
        let due = lock(&self.current)
            .refresh_after_ms
            .is_some_and(|refresh_after| refresh_after <= now_ms());
        if due {
            self.replace_access(false, sink, cancel).await;
        }
    }

    async fn replace_access(
        &self,
        forced: bool,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> bool {
        let Some(refresh) = &self.refresh else {
            return false;
        };
        let team = lock(&self.current).team.clone();
        let fresh = match refresh.refresh(forced, cancel).await {
            Ok(Some(fresh)) if fresh.team == team => fresh,
            Ok(None) => return false,
            Ok(Some(_)) => {
                sink.emit(StreamEvent::CredentialRefreshFailed {
                    source: REFRESHED_SOURCE,
                    forced,
                    error: AUTHORITY_CHANGED.to_owned(),
                });
                return false;
            }
            Err(error) => {
                sink.emit(StreamEvent::CredentialRefreshFailed {
                    source: REFRESHED_SOURCE,
                    forced,
                    error,
                });
                return false;
            }
        };
        let token = fresh.token.to_string();
        let mut secrets = lock(&self.secrets);
        if !token.is_empty() && !secrets.contains(&token) {
            secrets.push(token);
        }
        drop(secrets);
        *lock(&self.current) = Current {
            secret: Some(fresh.token).filter(|secret| !secret.is_empty()),
            team: fresh.team,
            refresh_after_ms: Some(fresh.refresh_after_ms),
        };
        sink.emit(StreamEvent::CredentialRefreshed {
            source: REFRESHED_SOURCE,
            forced,
        });
        true
    }

    fn build(&self, request: &ModelRequest<'_>) -> Result<String, RequestError> {
        match self.attachments.withhold_oversized_images(request.messages) {
            Some(messages) => build_request(
                &ModelRequest {
                    messages: &messages,
                    ..*request
                },
                &self.user_agent,
            ),
            None => build_request(request, &self.user_agent),
        }
    }

    pub fn review_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        build_review_request(request).ok()
    }

    pub async fn review(
        &self,
        request: &ModelRequest<'_>,
        body: String,
        cancel: &CancellationToken,
    ) -> Result<Completion, ProviderError> {
        let mut discarded = |_: StreamEvent| {};
        let completion = self
            .attempt(request, Some(body), &mut discarded, cancel, false)
            .await?;
        into_review_completion(completion, request.model, &self.masked())
    }

    async fn complete(
        &self,
        request: &ModelRequest<'_>,
        body: Option<String>,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> Result<Completion, ProviderError> {
        let completion = self.attempt(request, body, sink, cancel, true).await?;
        into_completion(completion, request.model, &self.masked())
    }

    async fn attempt(
        &self,
        request: &ModelRequest<'_>,
        body: Option<String>,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
        replayable: bool,
    ) -> Result<GatewayCompletion, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let payload = match body {
            Some(body) => body,
            None => self.build(request).map_err(request_failure)?,
        };
        self.refresh_if_due(sink, cancel).await;
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let error = match self.send(request, payload, sink, cancel, true).await {
            Err(error) if replayable && self.refresh.is_some() && rejected(&error) => error,
            sent => return sent,
        };
        let renewed = self.replace_access(true, sink, cancel).await;
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        if !renewed {
            return Err(error);
        }
        let Ok(payload) = self.build(request) else {
            return Err(error);
        };
        let replayed = self.send(request, payload, sink, cancel, false).await;
        sink.emit(StreamEvent::RequestReplayed);
        replayed
    }

    async fn send(
        &self,
        request: &ModelRequest<'_>,
        payload: String,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
        admits: bool,
    ) -> Result<GatewayCompletion, ProviderError> {
        let (secret, team) = {
            let current = lock(&self.current);
            (current.secret.clone(), current.team.clone())
        };
        let secrets = self.masked();
        let attempt = GatewayAttempt {
            client: &self.client,
            url: &self.chat_url,
            api_key: secret.as_deref().map(String::as_str),
            team: team.as_deref(),
            session_id: request.session_id,
            model: request.model,
            secrets: &secrets,
            admits,
        };
        attempt.stream(payload, sink, cancel).await
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
        self.build(request).ok()
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

fn request_failure(error: RequestError) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Protocol, error.to_string())
}

fn into_review_completion(
    completion: GatewayCompletion,
    model: &str,
    secrets: &[String],
) -> Result<Completion, ProviderError> {
    let timed_out = completion.failure_cause == Some(FailureCause::GatewayStreamTimeout);
    if timed_out
        || matches!(
            completion.finish,
            Some(Finish::ProviderError | Finish::ContentFilter)
        )
    {
        return into_completion(completion, model, secrets);
    }
    completion
        .tools
        .admission()
        .map_err(|code| ProviderError::new(ProviderErrorKind::Protocol, code))?;
    let GatewayCompletion {
        content,
        usage,
        tools,
        ..
    } = completion;
    let tool_calls = tools.into_calls();
    Ok(Completion {
        content: (!content.is_empty()).then_some(content),
        finish_reason: if tool_calls.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        },
        tool_calls,
        usage,
        billing: None,
        provider_replay: None,
    })
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
        billing,
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
        billing: billing.map(Box::new),
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
