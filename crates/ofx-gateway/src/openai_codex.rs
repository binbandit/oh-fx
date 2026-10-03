use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_contract::{
    BoxFuture, CODEX_ORIGINATOR, ChatMessage, Completion, FinishReason, ModelProvider,
    ModelRequest, ProviderError, ProviderErrorKind, ProviderReplay, ReplaySource, StreamEvent,
    StreamSink, valid_credential_account_id,
};
use ofx_http::{ClientError, ConnectionOptions, SseDecoder, build_connection_client};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Response, StatusCode};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::chat_completions::{
    ChunkSource, SendFailure, http_failure, sanitized, send, transport_failure,
};
use crate::chat_completions_protocol::mask_configured_secrets;
use crate::responses_protocol::{
    Delta, FailureCause, Reducer, ReplayLimits, ResponsesCompletion, ResponsesError,
    ResponsesFinish, StreamLimits, push_json_string, select_replay_parts, write_input, write_tools,
};

const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const REPLAY_PROVIDER: &str = "codex";
const EVENT_STREAM: &str = "text/event-stream";
const DEFAULT_INSTRUCTIONS: &str = "You are a helpful assistant.";
const MAX_MODEL_BYTES: usize = 1024;
const MAX_EVENT_TYPE_BYTES: usize = 64;
const MAX_SSE_EVENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOOL_CALLS: usize = 128;
const MAX_TOOL_IDENTITY_BYTES: usize = 1024;
const MAX_TOOL_ARGUMENTS_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROVIDER_STATE_BYTES: usize = 4 * 1024 * 1024;
const REPLAY_LIMITS: ReplayLimits = ReplayLimits {
    tool_calls: MAX_TOOL_CALLS,
    tool_identity_bytes: MAX_TOOL_IDENTITY_BYTES,
    tool_arguments_bytes: MAX_TOOL_ARGUMENTS_BYTES,
    provider_state_bytes: MAX_PROVIDER_STATE_BYTES,
};
const STREAM_LIMITS: StreamLimits = StreamLimits {
    aggregate_bytes: 64 * 1024 * 1024,
    events: 100_000,
    tool_calls: MAX_TOOL_CALLS,
    tool_identity_bytes: MAX_TOOL_IDENTITY_BYTES,
    tool_arguments_bytes: MAX_TOOL_ARGUMENTS_BYTES,
    provider_state_bytes: MAX_PROVIDER_STATE_BYTES,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexEndpoints {
    pub responses: String,
}

impl Default for CodexEndpoints {
    fn default() -> Self {
        Self {
            responses: RESPONSES_URL.to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexRefresh {
    IfNeeded,
    Force,
}

pub struct CodexAccess {
    token: Zeroizing<String>,
    account_id: String,
    refresh_after_ms: i64,
}

impl CodexAccess {
    pub fn new(token: String, account_id: String, refresh_after_ms: i64) -> Self {
        Self {
            token: Zeroizing::new(token),
            account_id,
            refresh_after_ms,
        }
    }
}

impl fmt::Debug for CodexAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexAccess")
            .field("token", &"<redacted>")
            .field("account_id", &self.account_id)
            .field("refresh_after_ms", &self.refresh_after_ms)
            .finish()
    }
}

pub trait CodexCredentials: Send + Sync {
    fn refresh<'a>(
        &'a self,
        mode: CodexRefresh,
        account_id: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<CodexAccess>>;
}

pub struct CodexProvider {
    client: reqwest::Client,
    responses_url: String,
    credentials: Arc<dyn CodexCredentials>,
    access: Mutex<CodexAccess>,
}

impl fmt::Debug for CodexProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexProvider")
            .field("responses_url", &self.responses_url)
            .finish_non_exhaustive()
    }
}

impl CodexProvider {
    pub fn new(
        access: CodexAccess,
        credentials: Arc<dyn CodexCredentials>,
        user_agent: &str,
        endpoints: CodexEndpoints,
    ) -> Result<Self, ClientError> {
        let client = build_connection_client(&ConnectionOptions {
            user_agent: user_agent.to_owned(),
            follow_redirects: false,
            ..ConnectionOptions::default()
        })?;
        Ok(Self {
            client,
            responses_url: endpoints.responses,
            credentials,
            access: Mutex::new(access),
        })
    }

    async fn complete(
        &self,
        request: &ModelRequest<'_>,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> Result<Completion, ProviderError> {
        let body = build_request(request, &replay_parts(request)).map_err(codex_failure)?;
        self.complete_body(request, body, sink, cancel).await
    }

    async fn complete_body(
        &self,
        request: &ModelRequest<'_>,
        body: String,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> Result<Completion, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        self.refresh_if_due(cancel).await;
        let mut sent = Vec::new();
        let mut response = self
            .post(&body, request.session_id, &mut sent, cancel)
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED
            && self.replace_access(CodexRefresh::Force, cancel).await
        {
            response = self
                .post(&body, request.session_id, &mut sent, cancel)
                .await?;
        }
        self.receive(response, &sent, sink, cancel, request.model)
            .await
    }

    async fn receive(
        &self,
        mut response: Response,
        sent: &[Zeroizing<String>],
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
        model: &str,
    ) -> Result<Completion, ProviderError> {
        let secrets = self.secrets(sent);
        if response.status() != StatusCode::OK {
            return Err(http_failure(response, cancel, &secrets, None).await);
        }
        let completion =
            consume_stream(&mut response, sink, cancel, STREAM_LIMITS, &secrets).await?;
        into_completion(completion, &secrets, model)
    }

    fn secrets(&self, sent: &[Zeroizing<String>]) -> Zeroizing<Vec<String>> {
        let current = Zeroizing::new(lock(&self.access).token.to_string());
        let mut secrets = Zeroizing::new(Vec::with_capacity(sent.len() + 1));
        for token in sent.iter().chain([&current]) {
            if !secrets.contains(&**token) {
                secrets.push(token.to_string());
            }
        }
        secrets
    }

    async fn refresh_if_due(&self, cancel: &CancellationToken) {
        let due = lock(&self.access).refresh_after_ms <= now_ms();
        if due {
            self.replace_access(CodexRefresh::IfNeeded, cancel).await;
        }
    }

    async fn replace_access(&self, mode: CodexRefresh, cancel: &CancellationToken) -> bool {
        let account_id = lock(&self.access).account_id.clone();
        let Some(fresh) = self.credentials.refresh(mode, &account_id, cancel).await else {
            return false;
        };
        if fresh.account_id != account_id {
            return false;
        }
        *lock(&self.access) = fresh;
        true
    }

    async fn post(
        &self,
        body: &str,
        session_id: Option<&str>,
        sent: &mut Vec<Zeroizing<String>>,
        cancel: &CancellationToken,
    ) -> Result<Response, ProviderError> {
        let (token, account_id) = {
            let access = lock(&self.access);
            (
                Zeroizing::new(access.token.to_string()),
                access.account_id.clone(),
            )
        };
        if !valid_credential_account_id(&account_id) {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "InvalidChatGptSubscriptionAccount",
            ));
        }
        let mut builder = self
            .client
            .post(&self.responses_url)
            .header(CONTENT_TYPE, "application/json")
            .bearer_auth(token.as_str())
            .header("chatgpt-account-id", account_id)
            .header("originator", CODEX_ORIGINATOR)
            .header("OpenAI-Beta", "responses=experimental")
            .header(ACCEPT, EVENT_STREAM);
        if let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) {
            builder = builder
                .header("session-id", session_id)
                .header("x-client-request-id", session_id);
        }
        let builder = builder.body(body.to_owned());
        sent.push(token);
        match send(builder, cancel).await {
            Ok(response) => Ok(response),
            Err(SendFailure::Cancelled) => Err(ProviderError::cancelled()),
            Err(SendFailure::Timeout) => {
                Err(ProviderError::new(ProviderErrorKind::Timeout, "Timeout"))
            }
            Err(SendFailure::Transport(error)) => {
                Err(transport_failure(&error, &self.secrets(sent)))
            }
        }
    }
}

impl ModelProvider for CodexProvider {
    fn stream<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        Box::pin(self.complete(request, sink, cancel))
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        build_request(request, &replay_parts(request)).ok()
    }

    fn stream_body<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        body: String,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        Box::pin(self.complete_body(request, body, sink, cancel))
    }

    fn project_replay(
        &self,
        replay: &ProviderReplay,
        text: bool,
        reasoning: bool,
    ) -> Result<Option<ProviderReplay>, ProviderError> {
        let parts = select_replay_parts(
            &replay.parts_json,
            MAX_PROVIDER_STATE_BYTES,
            text,
            reasoning,
        )
        .map_err(|error| ProviderError::new(ProviderErrorKind::Protocol, error.to_string()))?;
        Ok(parts.map(|parts_json| ProviderReplay {
            source: replay.source.clone(),
            parts_json,
        }))
    }
}

fn replay_source(model: &str) -> ReplaySource {
    ReplaySource {
        provider: REPLAY_PROVIDER.to_owned(),
        model: model.to_owned(),
    }
}

fn replay_parts<'a>(request: &ModelRequest<'a>) -> Vec<Option<&'a str>> {
    let source = replay_source(request.model);
    request
        .messages
        .iter()
        .map(|message| match message {
            ChatMessage::Assistant {
                provider_replay: Some(replay),
                ..
            } if replay.matches(&source) => Some(replay.parts_json.as_str()),
            _ => None,
        })
        .collect()
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

fn validate_model(model: &str) -> Result<(), ResponsesError> {
    if model.is_empty()
        || model.len() > MAX_MODEL_BYTES
        || model.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
    {
        return Err(ResponsesError::InvalidModel);
    }
    Ok(())
}

pub(crate) fn build_request(
    request: &ModelRequest<'_>,
    replays: &[Option<&str>],
) -> Result<String, ResponsesError> {
    if request
        .messages
        .iter()
        .any(|message| matches!(message, ChatMessage::System { .. }))
    {
        return Err(ResponsesError::InvalidProviderPrompt);
    }
    validate_model(request.model)?;
    let mut instructions = String::new();
    for text in request.instructions.iter().filter(|text| !text.is_empty()) {
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(text);
    }
    if instructions.is_empty() {
        instructions.push_str(DEFAULT_INSTRUCTIONS);
    }
    let mut out = String::from("{\"model\":");
    push_json_string(&mut out, request.model);
    out.push_str(",\"store\":false,\"stream\":true,\"instructions\":");
    push_json_string(&mut out, &instructions);
    out.push_str(",\"input\":[");
    write_input(&mut out, request.messages, replays, REPLAY_LIMITS)?;
    out.push(']');
    write_tools(&mut out, request.tools)?;
    out.push_str(",\"tool_choice\":");
    push_json_string(&mut out, request.tool_choice.as_str());
    out.push_str(",\"parallel_tool_calls\":true,\"include\":[\"reasoning.encrypted_content\"]");
    if request.provider_options.fast {
        out.push_str(",\"service_tier\":\"priority\"");
    }
    out.push_str(",\"text\":{\"verbosity\":\"low\"}");
    if let Some(effort) = request.provider_options.reasoning_effort {
        out.push_str(",\"reasoning\":{\"effort\":");
        push_json_string(&mut out, if effort == "minimal" { "low" } else { effort });
        out.push_str(",\"summary\":\"auto\"}");
    }
    out.push('}');
    Ok(out)
}

async fn consume_stream<S: ChunkSource + Send>(
    source: &mut S,
    sink: &mut dyn StreamSink,
    cancel: &CancellationToken,
    limits: StreamLimits,
    secrets: &[String],
) -> Result<ResponsesCompletion, ProviderError> {
    let mut reducer = Reducer::new(limits);
    let mut decoder = SseDecoder::new(MAX_SSE_EVENT_BYTES);
    let mut events: usize = 0;
    'stream: loop {
        loop {
            let data = match decoder.next_event() {
                Ok(Some(data)) => data,
                Ok(None) => break,
                Err(_) => return Err(codex_failure(ResponsesError::EventTooLarge)),
            };
            if data == b"[DONE]" {
                break 'stream;
            }
            events += 1;
            let (result, deltas) = reducer.apply(data, cancel.is_cancelled());
            for delta in deltas {
                if cancel.is_cancelled() {
                    return Err(ProviderError::cancelled());
                }
                sink.emit(match delta {
                    Delta::Text(text) => StreamEvent::TextDelta { text },
                    Delta::Reasoning(text) => StreamEvent::ReasoningDelta { text },
                    Delta::ToolCallStarted { call_id, tool_name } => {
                        StreamEvent::ToolCallStarted { call_id, tool_name }
                    }
                });
            }
            match result {
                Ok(true) => break 'stream,
                Ok(false) => {}
                Err(rejection) => {
                    let failure = codex_failure(rejection.error);
                    if failure.kind == ProviderErrorKind::Cancelled {
                        return Err(failure);
                    }
                    return Err(failure.with_detail(rejected_event_detail(
                        events,
                        data.len(),
                        rejection.event_type.as_deref(),
                        secrets,
                    )));
                }
            }
        }
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ProviderError::cancelled()),
            chunk = source.next_chunk() => chunk,
        };
        match chunk {
            Ok(Some(bytes)) => decoder.push(bytes.as_ref()),
            Ok(None) => break,
            Err(detail) => {
                return Err(ProviderError::new(
                    ProviderErrorKind::TransportInterrupted,
                    "ReadFailed",
                )
                .with_detail(sanitized(detail, secrets)));
            }
        }
    }
    reducer.finish(cancel.is_cancelled()).map_err(codex_failure)
}

fn rejected_event_detail(
    position: usize,
    size: usize,
    event_type: Option<&str>,
    secrets: &[String],
) -> String {
    match event_type.and_then(|kind| shown_event_type(kind, secrets)) {
        Some(kind) => format!("stream event {position} ({kind}, {size} bytes) was rejected"),
        None => format!("stream event {position} ({size} bytes) was rejected"),
    }
}

fn shown_event_type(kind: &str, secrets: &[String]) -> Option<String> {
    let token = !kind.is_empty()
        && kind.len() <= MAX_EVENT_TYPE_BYTES
        && kind.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'_'
        });
    if !token {
        return None;
    }
    let shown = sanitized(kind.to_owned(), secrets);
    (shown == kind).then_some(shown)
}

fn into_completion(
    completion: ResponsesCompletion,
    secrets: &[String],
    model: &str,
) -> Result<Completion, ProviderError> {
    let finish_reason = match completion.finish {
        ResponsesFinish::Stop => FinishReason::Stop,
        ResponsesFinish::ToolCalls => FinishReason::ToolCalls,
        ResponsesFinish::Length => {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "OutputTruncated",
            ));
        }
        ResponsesFinish::ContentFilter => {
            return Err(ProviderError::new(
                ProviderErrorKind::ProviderError,
                "ContentFiltered",
            ));
        }
        ResponsesFinish::ProviderError => {
            let (kind, detail) = match completion.failure {
                Some(failure) => (
                    match failure.cause {
                        FailureCause::Retryable => ProviderErrorKind::ServerError,
                        FailureCause::RateLimited => ProviderErrorKind::RateLimited,
                        FailureCause::NonRetryable => ProviderErrorKind::ProviderError,
                    },
                    failure.detail(|text| mask_configured_secrets(text, secrets)),
                ),
                None => (
                    ProviderErrorKind::ProviderError,
                    "provider_error: Provider response failed".to_owned(),
                ),
            };
            let detail = sanitized(detail, secrets);
            let mut error = ProviderError::new(kind, "ProviderError")
                .with_detail(format!("provider error: {detail}"));
            error.diagnostic = Some(detail);
            return Err(error);
        }
    };
    Ok(Completion {
        content: completion.content,
        tool_calls: completion.tool_calls,
        finish_reason,
        usage: completion.usage,
        provider_replay: completion.provider_state.map(|parts_json| ProviderReplay {
            source: replay_source(model),
            parts_json,
        }),
    })
}

fn codex_failure(error: ResponsesError) -> ProviderError {
    let code = match error {
        ResponsesError::Cancelled => return ProviderError::cancelled(),
        ResponsesError::EventTooLarge => "OpenAICodexSseEventTooLarge",
        ResponsesError::InvalidEvent => "InvalidOpenAICodexSseEvent",
        ResponsesError::StreamIncomplete => "OpenAICodexStreamIncomplete",
        ResponsesError::ToolCallLimitExceeded => "OpenAICodexToolCallLimitExceeded",
        ResponsesError::ToolArgumentsTooLarge => "OpenAICodexToolArgumentsTooLarge",
        ResponsesError::ResourceLimitExceeded => "OpenAICodexResourceLimitExceeded",
        ResponsesError::ProviderStateTooLarge => "OpenAICodexProviderStateTooLarge",
        ResponsesError::InvalidProviderState => "InvalidOpenAICodexProviderState",
        ResponsesError::InvalidModel => "InvalidOpenAICodexModel",
        other => return ProviderError::new(ProviderErrorKind::Protocol, other.to_string()),
    };
    ProviderError::new(ProviderErrorKind::Protocol, code)
}

#[cfg(test)]
mod tests;
