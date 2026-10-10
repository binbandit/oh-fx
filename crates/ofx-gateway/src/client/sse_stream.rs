use ofx_contract::{
    DuplicateKeys, Json, Object, ProviderError, ProviderErrorKind, StreamEvent, StreamSink, Usage,
    parse_strict_json,
};
use ofx_http::SseDecoder;
use ofx_text::mask_secrets;
use ofx_trace::{TraceContext, keyless_json_preview, trace_event, trace_log};
use tokio_util::sync::CancellationToken;

mod tools;

use super::replay::ReplayBuilder;
use super::resolved_model;
use crate::chat_completions::ChunkSource;
use crate::secret_mask::mask_configured_secrets;
use crate::stall_watch::StallWatch;
pub(crate) use tools::ToolStream;

const MAX_SSE_EVENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_FAILURE_DETAIL_BYTES: usize = 600;
const TRIMMED: [char; 4] = [' ', '\r', '\n', '\t'];
const GATEWAY_STREAM_TIMEOUT: &str = "gateway_stream_timeout";
const MESSAGE_KEYS: [&str; 4] = ["message", "detail", "details", "reason"];
const CANONICAL_EVENT_TYPES: [&str; 20] = [
    "response-metadata",
    "text-start",
    "text-delta",
    "text-end",
    "reasoning-start",
    "reasoning-delta",
    "reasoning-end",
    "tool-input-start",
    "tool-input-delta",
    "tool-input-end",
    "tool-call",
    "tool-result",
    "source",
    "file",
    "raw",
    "error",
    "start",
    "start-step",
    "finish-step",
    "finish",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Finish {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    ProviderError,
    Other,
}

impl Finish {
    fn parse(unified: &str) -> Option<Self> {
        Some(match unified {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "content-filter" => Self::ContentFilter,
            "tool-calls" => Self::ToolCalls,
            "error" => Self::ProviderError,
            "other" => Self::Other,
            _ => return None,
        })
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ContentFilter => "content-filter",
            Self::ToolCalls => "tool-calls",
            Self::ProviderError => "error",
            Self::Other => "other",
        }
    }
}

pub(crate) fn finish_label(finish: Option<Finish>) -> &'static str {
    finish.map_or("(none)", Finish::label)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureCause {
    GatewayStreamTimeout,
}

#[derive(Debug, Default)]
pub(crate) struct GatewayCompletion {
    pub(crate) content: String,
    pub(crate) finish: Option<Finish>,
    pub(crate) usage: Usage,
    pub(crate) failure_cause: Option<FailureCause>,
    pub(crate) failure_detail: Option<String>,
    pub(crate) events: usize,
    pub(crate) tools: ToolStream,
    pub(crate) replay: Option<String>,
}

pub(crate) struct Stream<'a> {
    pub(crate) requested_model: &'a str,
    pub(crate) secrets: &'a [String],
}

impl Stream<'_> {
    pub(crate) async fn consume<S: ChunkSource + Send>(
        &self,
        source: &mut S,
        sink: &mut dyn StreamSink,
        cancel: &CancellationToken,
    ) -> Result<GatewayCompletion, ProviderError> {
        let mut completion = GatewayCompletion::default();
        let mut replay = ReplayBuilder::default();
        let mut decoder = SseDecoder::new(MAX_SSE_EVENT_BYTES);
        let mut watch = StallWatch::start();
        loop {
            if cancel.is_cancelled() {
                return Err(cancelled(&completion));
            }
            match decoder.next_event() {
                Ok(Some(data)) => {
                    watch.progressed();
                    if data == b"[DONE]" {
                        terminated("done_without_finish", None);
                        break;
                    }
                    if self.accept(&mut completion, &mut replay, data, sink)? {
                        break;
                    }
                    continue;
                }
                Ok(None) => {}
                Err(_) => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Protocol,
                        "GatewaySseEventTooLarge",
                    ));
                }
            }
            match watch.next_chunk(source, cancel, self.secrets).await {
                Ok(Some(bytes)) => decoder.push(bytes.as_ref()),
                Ok(None) => {
                    terminated("eof_without_finish", completion.finish);
                    break;
                }
                Err(error) if error.kind == ProviderErrorKind::Cancelled => {
                    return Err(cancelled(&completion));
                }
                Err(error) => {
                    terminated("read_failure", completion.finish);
                    return Err(error);
                }
            }
        }
        if matches!(completion.finish, Some(Finish::Stop | Finish::ToolCalls)) {
            completion.replay =
                replay.finish(&completion.content, &completion.tools.replay_calls())?;
        }
        summarize(&completion);
        Ok(completion)
    }

    fn accept(
        &self,
        completion: &mut GatewayCompletion,
        replay: &mut ReplayBuilder,
        data: &[u8],
        sink: &mut dyn StreamSink,
    ) -> Result<bool, ProviderError> {
        completion.events += 1;
        let Ok(root) = parse_strict_json(data, DuplicateKeys::AfterValue) else {
            trace_log!(
                "sse",
                "event type=invalid bytes={} preview=<invalid-json>",
                data.len()
            );
            return Err(
                ProviderError::new(ProviderErrorKind::Protocol, "InvalidGatewaySseEvent")
                    .with_detail(format!(
                        "stream event {} ({} bytes) was rejected",
                        completion.events,
                        data.len()
                    )),
            );
        };
        traced_event(&root, data);
        let Some(fields) = root.as_object() else {
            return Ok(false);
        };
        let Some(kind) = fields.get("type").and_then(Json::as_str) else {
            return Ok(false);
        };
        replay.observe(kind, fields, completion.content.len())?;
        match kind {
            "response-metadata" => {
                if let Some(model) = fields
                    .get("modelId")
                    .and_then(Json::as_str)
                    .filter(|model| !model.is_empty())
                {
                    resolved_model::traced(
                        self.requested_model,
                        "sse.response-metadata.modelId",
                        &mask_configured_secrets(model.to_owned(), self.secrets),
                    );
                }
            }
            "error" => {
                completion.failure_cause = completion.failure_cause.or(failure_cause(fields));
                self.capture_detail(completion, fields);
            }
            "text-delta" => {
                if let Some(delta) = non_empty_delta(fields) {
                    completion.content.push_str(delta);
                    sink.emit(StreamEvent::TextDelta {
                        text: delta.to_owned(),
                    });
                }
            }
            "reasoning-delta" => {
                if let Some(delta) = non_empty_delta(fields) {
                    sink.emit(StreamEvent::ReasoningDelta {
                        text: delta.to_owned(),
                    });
                }
            }
            "tool-input-start" => completion.tools.start(fields, sink),
            "tool-input-delta" => completion.tools.input(fields, true, sink),
            "tool-input-end" => completion.tools.input(fields, false, sink),
            "tool-call" => completion.tools.call(fields, self.secrets),
            "tool-result" => completion.tools.result(fields),
            "finish" => {
                completion.failure_cause = completion.failure_cause.or(failure_cause(fields));
                let finish = finish_reason(fields)?;
                if finish == Finish::ProviderError {
                    self.capture_detail(completion, fields);
                }
                completion.finish = Some(finish);
                completion.usage = usage(fields);
                terminated("valid_finish", completion.finish);
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn capture_detail(&self, completion: &mut GatewayCompletion, root: &Object<'_>) {
        if completion.failure_detail.is_none() {
            completion.failure_detail = failure_detail(root).and_then(|text| self.clipped(&text));
        }
    }

    fn clipped(&self, text: &str) -> Option<String> {
        let configured = mask_configured_secrets(text.to_owned(), self.secrets);
        let masked = mask_secrets(&configured);
        let trimmed = masked.trim_matches(TRIMMED);
        let kept = &trimmed[..trimmed.floor_char_boundary(MAX_FAILURE_DETAIL_BYTES)];
        (!kept.is_empty()).then(|| kept.to_owned())
    }
}

fn cancelled(completion: &GatewayCompletion) -> ProviderError {
    terminated("cancellation", completion.finish);
    summarize(completion);
    ProviderError::cancelled()
}

fn summarize(completion: &GatewayCompletion) {
    trace_log!(
        "stream",
        "sse summary events={} finish_reason={}",
        completion.events,
        finish_label(completion.finish)
    );
}

fn terminated(cause: &str, finish: Option<Finish>) {
    let finish = finish_label(finish);
    trace_log!("stream", "termination cause={cause} finish_reason={finish}");
    trace_event!(
        "gateway",
        "sse_termination",
        TraceContext::default(),
        "cause={cause} finish_reason={finish}"
    );
}

fn traced_event(root: &Json<'_>, data: &[u8]) {
    if !ofx_trace::enabled("sse") {
        return;
    }
    let kind = root
        .get("type")
        .and_then(Json::as_str)
        .map_or("invalid", |kind| {
            CANONICAL_EVENT_TYPES
                .iter()
                .find(|known| **known == kind)
                .copied()
                .unwrap_or("unknown")
        });
    let preview = std::str::from_utf8(data).map_or_else(
        |_| format!("<invalid-json bytes={}>", data.len()),
        keyless_json_preview,
    );
    trace_log!(
        "sse",
        "event type={kind} bytes={} preview={preview}",
        data.len()
    );
}

fn non_empty_delta<'a>(fields: &'a Object<'_>) -> Option<&'a str> {
    fields
        .get("delta")
        .and_then(Json::as_str)
        .filter(|delta| !delta.is_empty())
}

fn finish_reason(fields: &Object<'_>) -> Result<Finish, ProviderError> {
    let invalid = || ProviderError::new(ProviderErrorKind::Protocol, "InvalidProviderFinishReason");
    let unified = fields
        .get("finishReason")
        .and_then(|reason| reason.get("unified"))
        .and_then(Json::as_str)
        .ok_or_else(invalid)?;
    Finish::parse(unified).ok_or_else(|| {
        terminated("invalid_finish", None);
        invalid()
    })
}

fn usage(fields: &Object<'_>) -> Usage {
    let total = |key: &str| {
        fields
            .get("usage")?
            .get(key)?
            .get("total")?
            .as_i64()
            .and_then(|total| u64::try_from(total).ok())
    };
    Usage {
        input_tokens: total("inputTokens"),
        output_tokens: total("outputTokens"),
    }
}

fn is_timeout_code(value: Option<&Json<'_>>) -> bool {
    value.and_then(Json::as_str) == Some(GATEWAY_STREAM_TIMEOUT)
}

fn has_timeout(fields: &Object<'_>) -> bool {
    is_timeout_code(fields.get("code")) || is_timeout_code(fields.get("type"))
}

fn failure_cause(fields: &Object<'_>) -> Option<FailureCause> {
    let nested = ["error", "providerError"]
        .iter()
        .filter_map(|key| fields.get(key).and_then(Json::as_object))
        .any(has_timeout);
    let raw = fields
        .get("finishReason")
        .and_then(Json::as_object)
        .is_some_and(|reason| is_timeout_code(reason.get("raw")));
    (has_timeout(fields) || nested || raw).then_some(FailureCause::GatewayStreamTimeout)
}

fn text_value<'a>(value: Option<&'a Json<'_>>) -> Option<&'a str> {
    value.and_then(Json::as_str).filter(|text| !text.is_empty())
}

fn object_detail(fields: &Object<'_>, include_type: bool) -> Option<String> {
    let code = match fields.get("code") {
        Some(code) => text_value(Some(code)),
        None if include_type => text_value(fields.get("type")),
        None => None,
    };
    if let (Some(code), Some(message)) = (code, text_value(fields.get("message"))) {
        return Some(format!("{code}: {message}"));
    }
    if let Some(message) = MESSAGE_KEYS
        .iter()
        .find_map(|key| text_value(fields.get(key)))
    {
        return Some(format!("provider_error: {message}"));
    }
    code.map(str::to_owned)
}

fn failure_detail(root: &Object<'_>) -> Option<String> {
    if let Some(detail) = object_detail(root, false) {
        return Some(detail);
    }
    if let Some(error) = root.get("error") {
        if let Some(text) = text_value(Some(error)) {
            return Some(format!("provider_error: {text}"));
        }
        if let Some(fields) = error.as_object() {
            return object_detail(fields, true).or_else(|| serde_json::to_string(error).ok());
        }
    }
    let value = root.get("providerError")?;
    if let Some(text) = text_value(Some(value)) {
        return Some(format!("provider_error: {text}"));
    }
    value
        .as_object()
        .and_then(|fields| object_detail(fields, true))
        .or_else(|| serde_json::to_string(value).ok())
}

#[cfg(test)]
mod tests;
