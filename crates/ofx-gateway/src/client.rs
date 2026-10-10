mod resolved_model;
mod sse_stream;

use std::fmt;

use ofx_contract::{ProviderError, ProviderErrorKind, StreamEvent, StreamSink};
use ofx_trace::{TraceContext, trace_event, trace_log};
use reqwest::header::CONTENT_TYPE;
use reqwest::{RequestBuilder, StatusCode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::chat_completions::{SendFailure, http_failure, send, transport_failure};
use sse_stream::Stream;
pub(crate) use sse_stream::{FailureCause, Finish, GatewayCompletion, finish_label};

const STREAM: &str = "stream";
const TIMEOUT: &str = "Timeout";
const STREAM_TOO_LONG: &str = "StreamTooLong";
const CONNECTION_FAILED: &str = "ConnectionFailed";
const REQUEST_FAILED: &str = "RequestFailed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundedFailure {
    Cancelled,
    Failed(&'static str),
}

pub(crate) async fn bounded_get(
    request: RequestBuilder,
    max_bytes: usize,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<(StatusCode, Vec<u8>), BoundedFailure> {
    if cancel.is_cancelled() {
        trace_log!(
            STREAM,
            "bounded termination cause=cancellation phase=admission"
        );
        return Err(BoundedFailure::Cancelled);
    }
    if Instant::now() >= deadline {
        trace_log!(STREAM, "bounded termination cause=deadline phase=admission");
        return Err(BoundedFailure::Failed(TIMEOUT));
    }
    let operation = async {
        let mut response = request.send().await.map_err(|error| transport(&error))?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| transport(&error))? {
            if body.len() + chunk.len() > max_bytes {
                return Err(BoundedFailure::Failed(STREAM_TOO_LONG));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            trace_log!(STREAM, "bounded termination cause=cancellation phase=control");
            Err(BoundedFailure::Cancelled)
        }
        () = tokio::time::sleep_until(deadline) => {
            if cancel.is_cancelled() {
                trace_log!(STREAM, "bounded termination cause=cancellation phase=deadline_cleanup");
                return Err(BoundedFailure::Cancelled);
            }
            trace_log!(STREAM, "bounded termination cause=deadline phase=control");
            Err(BoundedFailure::Failed(TIMEOUT))
        }
        outcome = operation => {
            if cancel.is_cancelled() {
                trace_log!(STREAM, "bounded termination cause=cancellation phase=request_result");
                return Err(BoundedFailure::Cancelled);
            }
            outcome
        }
    }
}

fn transport(error: &reqwest::Error) -> BoundedFailure {
    if error.is_timeout() {
        BoundedFailure::Failed(TIMEOUT)
    } else if error.is_connect() {
        BoundedFailure::Failed(CONNECTION_FAILED)
    } else {
        BoundedFailure::Failed(REQUEST_FAILED)
    }
}

const REFERER: &str = "https://github.com/binbandit/oh-fx";
const TITLE: &str = "oh-fx";
const TEAM_HEADER: &str = "x-vercel-ai-gateway-team";
const ATTEMPT: &str = "attempt=1";

pub(crate) struct GatewayAttempt<'a> {
    pub(crate) client: &'a reqwest::Client,
    pub(crate) url: &'a str,
    pub(crate) api_key: Option<&'a str>,
    pub(crate) team: Option<&'a str>,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) model: &'a str,
    pub(crate) secrets: &'a [String],
}

impl GatewayAttempt<'_> {
    pub(crate) async fn stream(
        &self,
        payload: String,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> Result<GatewayCompletion, ProviderError> {
        let bytes = payload.len();
        let builder = self.request(payload);
        step(
            "before_http_open_connect",
            format_args!("{ATTEMPT} attempt_limit=1 retries_used=0"),
        );
        step(
            "before_request_open",
            format_args!("{ATTEMPT} attempt_limit=1 retries_used=0 payload_bytes={bytes}"),
        );
        sink.emit(StreamEvent::Admitted);
        let mut response = match send(builder, cancel).await {
            Ok(response) => response,
            Err(SendFailure::Cancelled) => return Err(ProviderError::cancelled()),
            Err(SendFailure::Transport(error)) => {
                let failure = transport_failure(&error, self.secrets);
                let name = if error.is_connect() {
                    "http_open_connect_error"
                } else {
                    "receive_head_error"
                };
                step(name, format_args!("{ATTEMPT} err={}", failure.code));
                return Err(failure);
            }
        };
        step("after_http_open_connect", format_args!("{ATTEMPT}"));
        step("after_request_open", format_args!("{ATTEMPT}"));
        for name in ["before_request_send", "after_request_send", "after_send"] {
            step(name, format_args!("{ATTEMPT} payload_bytes={bytes}"));
        }
        step("before_receive_head", format_args!("{ATTEMPT}"));
        let status = response.status();
        step(
            "after_receive_head",
            format_args!("{ATTEMPT} status={}", status.as_u16()),
        );
        let resolved = resolved_model::from_headers(response.headers(), self.model, self.secrets);
        if status != StatusCode::OK {
            trace_log!("stream", "http status={} {ATTEMPT}", status.as_u16());
            return Err(http_failure(response, cancel, self.secrets, None).await);
        }
        step("before_sse_consume", format_args!("{ATTEMPT}"));
        let stream = Stream {
            requested_model: self.model,
            secrets: self.secrets,
        };
        let completion = match stream.consume(&mut response, sink, cancel).await {
            Ok(completion) => completion,
            Err(error) => {
                if error.kind != ProviderErrorKind::Cancelled {
                    step(
                        "sse_consume_error",
                        format_args!("{ATTEMPT} err={}", error.code),
                    );
                }
                return Err(error);
            }
        };
        if !resolved {
            resolved_model::missing(self.model);
        }
        completed(&completion);
        Ok(completion)
    }

    fn request(&self, payload: String) -> RequestBuilder {
        let mut builder = self
            .client
            .post(self.url)
            .header(CONTENT_TYPE, "application/json")
            .header("HTTP-Referer", REFERER)
            .header("X-Title", TITLE)
            .header("x-vercel-gateway-extended-time", "true")
            .header("ai-gateway-protocol-version", "0.0.1")
            .header("ai-language-model-specification-version", "4")
            .header("ai-language-model-id", self.model)
            .header("ai-language-model-streaming", "true");
        if let Some(key) = self.api_key {
            builder = builder.bearer_auth(key);
        }
        if let Some(team) = self.team.filter(|team| !team.is_empty()) {
            builder = builder.header(TEAM_HEADER, team);
        }
        if let Some(session) = self.session_id.filter(|session| !session.is_empty()) {
            builder = builder
                .header("x-session-id", session)
                .header("x-session-affinity", session);
        }
        builder.body(payload)
    }
}

fn completed(completion: &GatewayCompletion) {
    let finish = finish_label(completion.finish);
    let content = completion.content.len();
    step(
        "after_sse_consume",
        format_args!("{ATTEMPT} finish_reason={finish} content_bytes={content} tool_call_count=0"),
    );
    trace_log!(
        "stream",
        "completed {ATTEMPT} finish_reason={finish} content_bytes={content} tool_calls=0"
    );
    step(
        "stream_complete",
        format_args!(
            "{ATTEMPT} finish_reason={finish} content_bytes={content} tool_call_count=0 tool_calls=0"
        ),
    );
}

fn step(name: &str, message: fmt::Arguments<'_>) {
    trace_event!("gateway", name, TraceContext::default(), "{message}");
}
