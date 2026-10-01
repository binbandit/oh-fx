use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use ofx_contract::{
    BoxFuture, ChatMessage, Completion, FinishReason, ModelProvider, ModelRequest, ProviderError,
    ProviderErrorKind, StreamEvent, StreamSink, ToolCall, valid_credential_account_id,
};
use ofx_http::{ClientError, ConnectionOptions, SseDecoder, build_connection_client};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Response, StatusCode};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::chat_completions::{
    ChunkSource, SendFailure, excerpt, http_failure, sanitized, send, transport_failure,
};
use crate::responses_protocol::{
    Delta, FailureCause, Reducer, ReplayLimits, ResponsesCompletion, ResponsesError,
    ResponsesFinish, StreamLimits, push_json_string, write_input, write_tools,
};

const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const ORIGINATOR: &str = "fx";
const EVENT_STREAM: &str = "text/event-stream";
const DEFAULT_INSTRUCTIONS: &str = "You are a helpful assistant.";
const MAX_MODEL_BYTES: usize = 1024;
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

type Fingerprint = [u8; 32];

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
    ) -> BoxFuture<'a, Option<CodexAccess>>;
}

struct Replay {
    model: String,
    parts: String,
}

pub struct CodexProvider {
    client: reqwest::Client,
    responses_url: String,
    credentials: Arc<dyn CodexCredentials>,
    access: Mutex<CodexAccess>,
    replay: Mutex<HashMap<Fingerprint, Replay>>,
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
            replay: Mutex::new(HashMap::new()),
        })
    }

    async fn complete(
        &self,
        request: &ModelRequest<'_>,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> Result<Completion, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let (body, history) = self.prepare(request)?;
        self.refresh_if_due().await;
        let mut response = self.post(&body, cancel).await?;
        if response.status() == StatusCode::UNAUTHORIZED
            && self.replace_access(CodexRefresh::Force).await
        {
            response = self.post(&body, cancel).await?;
        }
        if response.status() != StatusCode::OK {
            return Err(http_failure(response, cancel, &self.secrets(), None).await);
        }
        let completion = self.consume(&mut response, sink, cancel).await?;
        let provider_state = completion.provider_state.clone();
        let completion = into_completion(completion, &self.secrets())?;
        if let Some(parts) = provider_state {
            self.remember(&history, &completion, request.model, parts);
        }
        Ok(completion)
    }

    fn prepare(
        &self,
        request: &ModelRequest<'_>,
    ) -> Result<(String, Vec<Fingerprint>), ProviderError> {
        let memory = lock(&self.replay);
        let fingerprints: Vec<Option<Fingerprint>> = request
            .messages
            .iter()
            .map(|message| match message {
                ChatMessage::Assistant {
                    content,
                    tool_calls,
                } => Some(fingerprint(content.as_deref(), tool_calls)),
                _ => None,
            })
            .collect();
        let replays: Vec<Option<&str>> = fingerprints
            .iter()
            .map(|key| {
                key.and_then(|key| memory.get(&key))
                    .filter(|replay| replay.model == request.model)
                    .map(|replay| replay.parts.as_str())
            })
            .collect();
        let body = build_request(request, &replays).map_err(codex_failure)?;
        Ok((body, fingerprints.into_iter().flatten().collect()))
    }

    fn remember(
        &self,
        history: &[Fingerprint],
        completion: &Completion,
        model: &str,
        parts: String,
    ) {
        let kept: HashSet<&Fingerprint> = history.iter().collect();
        let mut memory = lock(&self.replay);
        memory.retain(|key, _| kept.contains(key));
        memory.insert(
            fingerprint(completion.content.as_deref(), &completion.tool_calls),
            Replay {
                model: model.to_owned(),
                parts,
            },
        );
    }

    fn secrets(&self) -> Vec<String> {
        vec![lock(&self.access).token.to_string()]
    }

    async fn refresh_if_due(&self) {
        let due = lock(&self.access).refresh_after_ms <= now_ms();
        if due {
            self.replace_access(CodexRefresh::IfNeeded).await;
        }
    }

    async fn replace_access(&self, mode: CodexRefresh) -> bool {
        let account_id = lock(&self.access).account_id.clone();
        let Some(fresh) = self.credentials.refresh(mode, &account_id).await else {
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
        let builder = self
            .client
            .post(&self.responses_url)
            .header(CONTENT_TYPE, "application/json")
            .bearer_auth(token.as_str())
            .header("chatgpt-account-id", account_id)
            .header("originator", ORIGINATOR)
            .header("OpenAI-Beta", "responses=experimental")
            .header(ACCEPT, EVENT_STREAM)
            .body(body.to_owned());
        match send(builder, cancel).await {
            Ok(response) => Ok(response),
            Err(SendFailure::Cancelled) => Err(ProviderError::cancelled()),
            Err(SendFailure::Timeout) => {
                Err(ProviderError::new(ProviderErrorKind::Timeout, "Timeout"))
            }
            Err(SendFailure::Transport(error)) => {
                Err(transport_failure(&error, std::slice::from_ref(&*token)))
            }
        }
    }

    async fn consume<S: ChunkSource + Send>(
        &self,
        source: &mut S,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> Result<ResponsesCompletion, ProviderError> {
        consume_stream(source, sink, cancel, STREAM_LIMITS, &self.secrets()).await
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

fn fingerprint(content: Option<&str>, tool_calls: &[ToolCall]) -> Fingerprint {
    let mut digest = Sha256::new();
    let mut field = |bytes: &[u8]| {
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    };
    field(content.unwrap_or_default().as_bytes());
    field(&[u8::from(content.is_some())]);
    for call in tool_calls {
        field(call.id.as_str().as_bytes());
        field(call.name.as_bytes());
        field(call.arguments.as_bytes());
    }
    digest.finalize().into()
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
    out.push_str(
        ",\"parallel_tool_calls\":true,\"include\":[\"reasoning.encrypted_content\"],\"text\":{\"verbosity\":\"low\"}}",
    );
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
            let (result, deltas) = reducer.apply(data, cancel.is_cancelled());
            for delta in deltas {
                if cancel.is_cancelled() {
                    return Err(ProviderError::cancelled());
                }
                sink.emit(match delta {
                    Delta::Text(text) => StreamEvent::TextDelta { text },
                    Delta::Reasoning(text) => StreamEvent::ReasoningDelta { text },
                });
            }
            match result {
                Ok(true) => break 'stream,
                Ok(false) => {}
                Err(error) => {
                    let failure = codex_failure(error);
                    if failure.kind == ProviderErrorKind::Cancelled {
                        return Err(failure);
                    }
                    let detail = format!("stream event: {}", excerpt(data));
                    return Err(failure.with_detail(sanitized(detail, secrets)));
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

fn into_completion(
    completion: ResponsesCompletion,
    secrets: &[String],
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
                    failure.detail,
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
