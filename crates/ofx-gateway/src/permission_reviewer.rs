use std::sync::Arc;

use ofx_contract::{
    BoxFuture, Completion, FinishReason, ModelProvider, ModelRequest, ProviderError,
    ProviderErrorKind, ReviewTransport, ReviewTransportOutcome, StreamEvent, Usage,
};
use ofx_trace::trace_log;
use tokio_util::sync::CancellationToken;

use crate::gateway::GatewayProvider;

const PERMISSION: &str = "permission";
const SINGLE_TRANSPORT_ATTEMPT: usize = 1;
const CODEX_REVIEWER_MODEL: &str = "gpt-5.6-luna";
const GATEWAY_REVIEWER_MODEL: &str = "openai/gpt-5.6-luna";
const MAX_REVIEW_OUTPUT_TOKENS: u32 = 2048;
const OUTPUT_TRUNCATED: &str = "OutputTruncated";
const CONTENT_FILTERED: &str = "ContentFiltered";
const REQUIRED_TOOL_MISSING: &str = "RequiredToolMissing";
const PROVIDER_ERROR: &str = "ProviderError";
const REQUEST_FAILED: &str = "RequestFailed";
const GATEWAY_COMPLETIONS: [&str; 6] = [
    OUTPUT_TRUNCATED,
    "StreamInterrupted",
    "InvalidProviderCompletion",
    "MalformedProviderResultIdentity",
    "MalformedAuthoritativeToolIdentity",
    "MalformedProviderToolArguments",
];

type OutputTokens = dyn Fn(&str) -> Option<u32> + Send + Sync;

pub struct CodexReviewTransport {
    provider: Arc<dyn ModelProvider>,
}

impl CodexReviewTransport {
    pub fn new(provider: Arc<dyn ModelProvider>) -> Self {
        Self { provider }
    }
}

impl ReviewTransport for CodexReviewTransport {
    fn model<'a>(&'a self, _source_model: &'a str) -> &'a str {
        CODEX_REVIEWER_MODEL
    }

    fn max_output_tokens(&self, _model: &str) -> u32 {
        MAX_REVIEW_OUTPUT_TOKENS
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        self.provider.request_body(request)
    }

    fn send<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        body: String,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ReviewTransportOutcome> {
        send(&*self.provider, request, body, cancel, responses_failure)
    }
}

pub struct ChatCompletionsReviewTransport {
    provider: Arc<dyn ModelProvider>,
    reviewer_model: Option<String>,
    output_tokens: Box<OutputTokens>,
}

impl ChatCompletionsReviewTransport {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        reviewer_model: Option<String>,
        output_tokens: impl Fn(&str) -> Option<u32> + Send + Sync + 'static,
    ) -> Self {
        Self {
            provider,
            reviewer_model,
            output_tokens: Box::new(output_tokens),
        }
    }
}

impl ReviewTransport for ChatCompletionsReviewTransport {
    fn model<'a>(&'a self, source_model: &'a str) -> &'a str {
        self.reviewer_model.as_deref().unwrap_or(source_model)
    }

    fn max_output_tokens(&self, model: &str) -> u32 {
        (self.output_tokens)(model).map_or(MAX_REVIEW_OUTPUT_TOKENS, |limit| {
            limit.min(MAX_REVIEW_OUTPUT_TOKENS)
        })
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        self.provider.request_body(request)
    }

    fn send<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        body: String,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ReviewTransportOutcome> {
        send(
            &*self.provider,
            request,
            body,
            cancel,
            chat_completions_failure,
        )
    }
}

pub struct GatewayReviewTransport {
    provider: Arc<GatewayProvider>,
    reviewer_model: Option<String>,
}

impl GatewayReviewTransport {
    pub fn new(provider: Arc<GatewayProvider>, reviewer_model: Option<String>) -> Self {
        Self {
            provider,
            reviewer_model,
        }
    }
}

impl ReviewTransport for GatewayReviewTransport {
    fn model<'a>(&'a self, _source_model: &'a str) -> &'a str {
        self.reviewer_model
            .as_deref()
            .unwrap_or(GATEWAY_REVIEWER_MODEL)
    }

    fn max_output_tokens(&self, _model: &str) -> u32 {
        MAX_REVIEW_OUTPUT_TOKENS
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        self.provider.review_body(request)
    }

    fn send<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        body: String,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ReviewTransportOutcome> {
        Box::pin(async move {
            trace_log!(
                PERMISSION,
                "event=auto_review_transport_start model={} retry_count={SINGLE_TRANSPORT_ATTEMPT}",
                request.model
            );
            match self.provider.review(request, body, cancel).await {
                Ok(completion) => {
                    trace_completion(finish_name(completion.finish_reason), &completion);
                    ReviewTransportOutcome::Completion(Box::new(completion))
                }
                Err(error) => {
                    let outcome = gateway_failure(&error);
                    trace_gateway_failure(&outcome, &error);
                    outcome
                }
            }
        })
    }
}

fn gateway_failure(error: &ProviderError) -> ReviewTransportOutcome {
    if let Some(status) = error.status {
        return if matches!(status, 408 | 425 | 429) || status >= 500 {
            ReviewTransportOutcome::TransientFailure
        } else {
            ReviewTransportOutcome::PermanentFailure
        };
    }
    match (error.kind, error.code.as_str()) {
        (ProviderErrorKind::Cancelled, _) => ReviewTransportOutcome::Cancelled,
        (ProviderErrorKind::Timeout, _) => ReviewTransportOutcome::TimedOut,
        (_, CONTENT_FILTERED) => ReviewTransportOutcome::PermanentFailure,
        (_, PROVIDER_ERROR | REQUEST_FAILED) => ReviewTransportOutcome::TransientFailure,
        (_, code) if GATEWAY_COMPLETIONS.contains(&code) => {
            ReviewTransportOutcome::Completion(Box::new(undecided()))
        }
        _ => ReviewTransportOutcome::PermanentFailure,
    }
}

fn send<'a>(
    provider: &'a dyn ModelProvider,
    request: &'a ModelRequest<'a>,
    body: String,
    cancel: &'a CancellationToken,
    failure: fn(&ProviderError) -> ReviewTransportOutcome,
) -> BoxFuture<'a, ReviewTransportOutcome> {
    Box::pin(async move {
        let mut discarded = |_: StreamEvent| {};
        match provider
            .stream_body(request, body, &mut discarded, cancel)
            .await
        {
            Ok(completion) => {
                trace_completion(finish_name(completion.finish_reason), &completion);
                ReviewTransportOutcome::Completion(Box::new(completion))
            }
            Err(error) => {
                let outcome = failure(&error);
                trace_failure(&outcome, &error);
                outcome
            }
        }
    })
}

fn trace_completion(finish_reason: &str, completion: &Completion) {
    trace_log!(
        PERMISSION,
        "event=auto_review_transport result=completion finish_reason={finish_reason} tool_calls={} content_bytes={}",
        completion.tool_calls.len(),
        completion.content.as_deref().map_or(0, str::len)
    );
}

fn trace_gateway_failure(outcome: &ReviewTransportOutcome, error: &ProviderError) {
    match error.status {
        Some(status) => trace_log!(
            PERMISSION,
            "event=auto_review_transport result={} http_status={status}",
            outcome_name(outcome)
        ),
        None => trace_failure(outcome, error),
    }
}

const fn outcome_name(outcome: &ReviewTransportOutcome) -> &'static str {
    match outcome {
        ReviewTransportOutcome::Completion(_) => "completion",
        ReviewTransportOutcome::TransientFailure => "transient_failure",
        ReviewTransportOutcome::PermanentFailure => "permanent_failure",
        ReviewTransportOutcome::TimedOut => "timed_out",
        ReviewTransportOutcome::Cancelled => "cancelled",
    }
}

fn trace_failure(outcome: &ReviewTransportOutcome, error: &ProviderError) {
    if let ReviewTransportOutcome::Completion(completion) = outcome {
        let finish = if error.code == OUTPUT_TRUNCATED {
            "length"
        } else {
            "absent"
        };
        return trace_completion(finish, completion);
    }
    let result = outcome_name(outcome);
    if error.code == CONTENT_FILTERED {
        trace_log!(
            PERMISSION,
            "event=auto_review_transport result={result} reason=content_filter"
        );
    } else if error.code == PROVIDER_ERROR && error.status.is_none() {
        trace_log!(
            PERMISSION,
            "event=auto_review_transport result={result} reason=provider_error"
        );
    } else if let Some(kind) = error.status.and(failure_kind(error.kind)) {
        trace_log!(
            PERMISSION,
            "event=auto_review_transport result={result} reason=provider_failure failure_kind={kind}"
        );
    } else {
        trace_log!(
            PERMISSION,
            "event=auto_review_transport result={result} reason=transport_error error={}",
            error.code
        );
    }
}

const fn finish_name(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::ToolCalls => "tool_calls",
    }
}

const fn failure_kind(kind: ProviderErrorKind) -> Option<&'static str> {
    Some(match kind {
        ProviderErrorKind::InvalidRequest => "invalid_request",
        ProviderErrorKind::Unauthorized => "unauthorized",
        ProviderErrorKind::Forbidden => "forbidden",
        ProviderErrorKind::RequestTooLarge => "request_too_large",
        ProviderErrorKind::RateLimited => "rate_limited",
        ProviderErrorKind::ServerError => "server_error",
        ProviderErrorKind::BadGateway => "bad_gateway",
        ProviderErrorKind::Unavailable => "unavailable",
        ProviderErrorKind::GatewayTimeout => "gateway_timeout",
        ProviderErrorKind::ProviderError => "provider_error",
        ProviderErrorKind::ConnectionFailed
        | ProviderErrorKind::ConnectivityLost
        | ProviderErrorKind::TransportInterrupted
        | ProviderErrorKind::Timeout
        | ProviderErrorKind::StreamStalled
        | ProviderErrorKind::Protocol
        | ProviderErrorKind::Cancelled => return None,
    })
}

fn responses_failure(error: &ProviderError) -> ReviewTransportOutcome {
    match error.kind {
        ProviderErrorKind::Cancelled => ReviewTransportOutcome::Cancelled,
        ProviderErrorKind::Timeout => ReviewTransportOutcome::TimedOut,
        ProviderErrorKind::Protocol if error.code == OUTPUT_TRUNCATED => {
            ReviewTransportOutcome::Completion(Box::new(undecided()))
        }
        ProviderErrorKind::ProviderError if error.code == CONTENT_FILTERED => {
            ReviewTransportOutcome::PermanentFailure
        }
        ProviderErrorKind::RateLimited
        | ProviderErrorKind::ServerError
        | ProviderErrorKind::BadGateway
        | ProviderErrorKind::Unavailable
        | ProviderErrorKind::GatewayTimeout
        | ProviderErrorKind::ProviderError
        | ProviderErrorKind::ConnectionFailed
        | ProviderErrorKind::ConnectivityLost
        | ProviderErrorKind::TransportInterrupted
        | ProviderErrorKind::StreamStalled
        | ProviderErrorKind::Protocol => ReviewTransportOutcome::TransientFailure,
        ProviderErrorKind::InvalidRequest
        | ProviderErrorKind::Unauthorized
        | ProviderErrorKind::Forbidden
        | ProviderErrorKind::RequestTooLarge => ReviewTransportOutcome::PermanentFailure,
    }
}

fn chat_completions_failure(error: &ProviderError) -> ReviewTransportOutcome {
    match error.kind {
        ProviderErrorKind::Cancelled => ReviewTransportOutcome::Cancelled,
        ProviderErrorKind::Timeout => ReviewTransportOutcome::TimedOut,
        ProviderErrorKind::Protocol if error.code == REQUIRED_TOOL_MISSING => {
            ReviewTransportOutcome::Completion(Box::new(undecided()))
        }
        _ => ReviewTransportOutcome::PermanentFailure,
    }
}

fn undecided() -> Completion {
    Completion {
        content: None,
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
        billing: None,
        provider_replay: None,
    }
}

#[cfg(test)]
mod tests;
