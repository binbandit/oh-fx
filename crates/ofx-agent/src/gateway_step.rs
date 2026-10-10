use std::sync::Arc;

use ofx_contract::{
    BoxFuture, Completion, FinishReason, ModelProvider, ModelRequest, ProviderError,
    ProviderErrorKind, ProviderReplay, StreamSink,
};
use ofx_trace::{NETWORK_CALLS, NetworkCall, NetworkRing, TraceContext};
use tokio_util::sync::CancellationToken;

const OUTPUT_TRUNCATED: &str = "OutputTruncated";
const CONTENT_FILTERED: &str = "ContentFiltered";
const INCOMPLETE_STREAM: &str = "IncompleteStream";
const PROVIDER_FINISH_ERROR: &str = "ProviderError";
const COMPLETED_STATUS: u16 = 200;
const PROVIDER_ERROR_STATUS: u16 = 520;

pub(crate) const LENGTH: &str = "length";
pub(crate) const CONTENT_FILTER: &str = "content-filter";
pub(crate) const ERROR: &str = "error";
pub(crate) const MISSING_FINISH: &str = "";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settlement<'a> {
    Completed(&'a Completion),
    Finished(&'static str),
    Answered(u16),
    Failed(&'a str),
}

pub(crate) fn settlement(outcome: &Result<Completion, ProviderError>) -> Settlement<'_> {
    match outcome {
        Ok(completion) => Settlement::Completed(completion),
        Err(error) => failure_settlement(error),
    }
}

pub(crate) fn failure_settlement(error: &ProviderError) -> Settlement<'_> {
    match error.code.as_str() {
        OUTPUT_TRUNCATED => Settlement::Finished(LENGTH),
        CONTENT_FILTERED => Settlement::Finished(CONTENT_FILTER),
        INCOMPLETE_STREAM => Settlement::Finished(MISSING_FINISH),
        PROVIDER_FINISH_ERROR if error.status.is_none() => Settlement::Finished(ERROR),
        _ if error.status.is_some() => Settlement::Answered(failure_status(error.kind)),
        code => Settlement::Failed(code),
    }
}

pub(crate) const fn finish_label(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::ToolCalls => "tool-calls",
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Meter {
    ring: &'static NetworkRing,
    context: TraceContext,
}

impl Meter {
    pub(crate) const fn new(ring: &'static NetworkRing, context: TraceContext) -> Self {
        Self { ring, context }
    }

    pub(crate) fn record(
        self,
        model: &str,
        started_at_ms: i64,
        outcome: &Result<Completion, ProviderError>,
    ) {
        let elapsed = ofx_trace::timestamp_ms().saturating_sub(started_at_ms);
        let mut call = NetworkCall {
            started_at_ms,
            duration_ms: u32::try_from(elapsed.max(0)).unwrap_or(u32::MAX),
            turn_id: self.context.turn_id,
            step_id: self.context.step_id,
            subagent_id: self.context.subagent_id,
            model: model.to_owned(),
            ..NetworkCall::default()
        };
        match settlement(outcome) {
            Settlement::Completed(completion) => {
                call.status = COMPLETED_STATUS;
                call.response_bytes = clamped(completion_bytes(completion));
                call.input_tokens = tokens(completion.usage.input_tokens);
                call.output_tokens = tokens(completion.usage.output_tokens);
                finish_label(completion.finish_reason).clone_into(&mut call.stop_reason);
            }
            Settlement::Finished(reason) => {
                call.status = COMPLETED_STATUS;
                reason.clone_into(&mut call.stop_reason);
            }
            Settlement::Answered(status) => {
                call.status = status;
                call.response_bytes = clamped(
                    outcome
                        .as_ref()
                        .err()
                        .and_then(|error| error.detail.as_deref())
                        .map_or(0, str::len),
                );
            }
            Settlement::Failed(code) => code.clone_into(&mut call.error),
        }
        self.ring.record(call);
    }
}

pub struct MeteredProvider {
    inner: Arc<dyn ModelProvider>,
    ring: &'static NetworkRing,
}

impl MeteredProvider {
    pub fn new(inner: Arc<dyn ModelProvider>) -> Self {
        Self {
            inner,
            ring: &NETWORK_CALLS,
        }
    }

    fn meter(&self) -> Meter {
        Meter::new(self.ring, TraceContext::default())
    }
}

impl ModelProvider for MeteredProvider {
    fn stream<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        Box::pin(async move {
            let started_at_ms = ofx_trace::timestamp_ms();
            let outcome = self.inner.stream(request, sink, cancel).await;
            self.meter().record(request.model, started_at_ms, &outcome);
            outcome
        })
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Option<String> {
        self.inner.request_body(request)
    }

    fn stream_body<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        body: String,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        Box::pin(async move {
            let started_at_ms = ofx_trace::timestamp_ms();
            let outcome = self.inner.stream_body(request, body, sink, cancel).await;
            self.meter().record(request.model, started_at_ms, &outcome);
            outcome
        })
    }

    fn project_replay(
        &self,
        replay: &ProviderReplay,
        text: bool,
        reasoning: bool,
    ) -> Result<Option<ProviderReplay>, ProviderError> {
        self.inner.project_replay(replay, text, reasoning)
    }
}

fn completion_bytes(completion: &Completion) -> usize {
    let content = completion.content.as_deref().map_or(0, str::len);
    let replay = completion
        .provider_replay
        .as_ref()
        .map_or(0, |replay| replay.parts_json.len());
    let calls: usize = completion
        .tool_calls
        .iter()
        .map(|call| {
            call.id.as_str().len()
                + call.name.len()
                + call.arguments.len()
                + call.provider_result.as_deref().map_or(0, str::len)
        })
        .sum();
    content + replay + calls
}

const fn failure_status(kind: ProviderErrorKind) -> u16 {
    match kind {
        ProviderErrorKind::InvalidRequest => 400,
        ProviderErrorKind::Unauthorized => 401,
        ProviderErrorKind::Forbidden => 403,
        ProviderErrorKind::RequestTooLarge => 413,
        ProviderErrorKind::RateLimited => 429,
        ProviderErrorKind::ServerError => 500,
        ProviderErrorKind::BadGateway => 502,
        ProviderErrorKind::Unavailable => 503,
        ProviderErrorKind::GatewayTimeout => 504,
        _ => PROVIDER_ERROR_STATUS,
    }
}

fn tokens(count: Option<u64>) -> u32 {
    count.map_or(0, |count| u32::try_from(count).unwrap_or(u32::MAX))
}

fn clamped(bytes: usize) -> u32 {
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
