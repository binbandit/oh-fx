use std::sync::Arc;

use ofx_contract::{
    BoxFuture, Completion, FinishReason, ModelProvider, ModelRequest, ProviderError,
    ProviderErrorKind, ReviewTransport, ReviewTransportOutcome, StreamEvent, Usage,
};
use tokio_util::sync::CancellationToken;

const CODEX_REVIEWER_MODEL: &str = "gpt-5.6-luna";
const MAX_REVIEW_OUTPUT_TOKENS: u32 = 2048;
const OUTPUT_TRUNCATED: &str = "OutputTruncated";
const CONTENT_FILTERED: &str = "ContentFiltered";
const REQUIRED_TOOL_MISSING: &str = "RequiredToolMissing";

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
            Ok(completion) => ReviewTransportOutcome::Completion(completion),
            Err(error) => failure(&error),
        }
    })
}

fn responses_failure(error: &ProviderError) -> ReviewTransportOutcome {
    match error.kind {
        ProviderErrorKind::Cancelled => ReviewTransportOutcome::Cancelled,
        ProviderErrorKind::Timeout => ReviewTransportOutcome::TimedOut,
        ProviderErrorKind::Protocol if error.code == OUTPUT_TRUNCATED => {
            ReviewTransportOutcome::Completion(undecided())
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
            ReviewTransportOutcome::Completion(undecided())
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
        provider_replay: None,
    }
}

#[cfg(test)]
mod tests;
