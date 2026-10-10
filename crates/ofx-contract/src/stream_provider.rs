use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::ids::ToolCallId;
use crate::tool_dispatch::ToolSpec;
use crate::types::{
    ChatMessage, FinishReason, ProviderBilling, ProviderReplay, ToolCall, ToolChoice, Usage,
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelRequest<'a> {
    pub model: &'a str,
    pub instructions: &'a [&'a str],
    pub messages: &'a [ChatMessage],
    pub tools: &'a [ToolSpec],
    pub tool_choice: ToolChoice,
    pub max_output_tokens: Option<u32>,
    pub provider_options: ProviderOptions<'a>,
    pub session_id: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderOptions<'a> {
    pub reasoning_effort: Option<&'a str>,
    pub fast: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Admitted,
    ToolCallStarted {
        call_id: ToolCallId,
        tool_name: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolInputDelta {
        text: String,
    },
}

pub trait StreamSink: Send {
    fn emit(&mut self, event: StreamEvent);
}

impl<F: FnMut(StreamEvent) + Send> StreamSink for F {
    fn emit(&mut self, event: StreamEvent) {
        self(event);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Usage,
    pub billing: Option<Box<ProviderBilling>>,
    pub provider_replay: Option<ProviderReplay>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderErrorKind {
    InvalidRequest,
    Unauthorized,
    Forbidden,
    RequestTooLarge,
    RateLimited,
    ServerError,
    BadGateway,
    Unavailable,
    GatewayTimeout,
    ProviderError,
    ConnectionFailed,
    ConnectivityLost,
    TransportInterrupted,
    Timeout,
    StreamStalled,
    Protocol,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub code: String,
    pub status: Option<u16>,
    pub detail: Option<String>,
    pub diagnostic: Option<String>,
    pub retry_after: Option<Duration>,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, code: impl Into<String>) -> Self {
        Self {
            kind,
            code: code.into(),
            status: None,
            detail: None,
            diagnostic: None,
            retry_after: None,
        }
    }

    pub fn cancelled() -> Self {
        Self::new(ProviderErrorKind::Cancelled, "Cancelled")
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

pub trait ModelProvider: Send + Sync {
    fn stream<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>>;

    fn request_body(&self, _request: &ModelRequest<'_>) -> Option<String> {
        None
    }

    fn stream_body<'a>(
        &'a self,
        request: &'a ModelRequest<'a>,
        _body: String,
        sink: &'a mut dyn StreamSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
        self.stream(request, sink, cancel)
    }

    fn project_replay(
        &self,
        _replay: &ProviderReplay,
        _text: bool,
        _reasoning: bool,
    ) -> Result<Option<ProviderReplay>, ProviderError> {
        Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "ProviderReplayProjectionUnavailable",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ReplaySource;

    struct Silent;

    impl ModelProvider for Silent {
        fn stream<'a>(
            &'a self,
            _request: &'a ModelRequest<'a>,
            _sink: &'a mut dyn StreamSink,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Completion, ProviderError>> {
            Box::pin(async { Err(ProviderError::cancelled()) })
        }
    }

    #[test]
    fn providers_without_replay_projection_refuse_to_project() {
        let replay = ProviderReplay {
            source: ReplaySource {
                provider: "codex".to_owned(),
                model: "model".to_owned(),
                binding: None,
            },
            parts_json: "[]".to_owned(),
        };
        assert_eq!(
            Silent.project_replay(&replay, false, true),
            Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "ProviderReplayProjectionUnavailable"
            ))
        );
    }

    #[test]
    fn providers_that_cannot_show_their_request_body_measure_nothing() {
        let request = ModelRequest {
            model: "model",
            instructions: &[],
            messages: &[],
            tools: &[],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            provider_options: ProviderOptions::default(),
            session_id: None,
        };
        assert_eq!(Silent.request_body(&request), None);
    }

    #[test]
    fn closures_are_stream_sinks() {
        let mut seen = Vec::new();
        let mut sink = |event: StreamEvent| seen.push(event);
        sink.emit(StreamEvent::TextDelta {
            text: "hi".to_owned(),
        });
        assert_eq!(
            seen,
            vec![StreamEvent::TextDelta {
                text: "hi".to_owned()
            }]
        );
    }

    #[test]
    fn provider_errors_carry_their_detail() {
        let error = ProviderError::new(ProviderErrorKind::Protocol, "InvalidChunk")
            .with_detail("stream data: {not json");
        assert_eq!(error.code, "InvalidChunk");
        assert_eq!(error.detail.as_deref(), Some("stream data: {not json"));
        assert_eq!(
            ProviderError::cancelled().kind,
            ProviderErrorKind::Cancelled
        );
    }
}
