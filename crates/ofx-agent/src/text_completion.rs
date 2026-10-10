use ofx_contract::{
    Completion, FinishReason, ModelProvider, ModelRequest, ProviderError, ProviderErrorKind,
    StreamEvent, Usage,
};
use ofx_text::mask_secrets;
use ofx_trace::{preview, terminal_preview};
use tokio_util::sync::CancellationToken;

use crate::model_response_recovery::{
    DEFAULT_MAX_PROVIDER_ATTEMPTS, Evidence, Output, Progress, RetryPacing, ToolEvidence, decide,
    recovery_cause,
};

const OUTPUT_TRUNCATED: &str = "OutputTruncated";
const SAFE_DETAIL_BYTES: usize = 512;
const DETAIL_PREVIEW_BYTES: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cancelled;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    Transport,
    Provider,
    ToolCall,
    Incomplete,
    Truncated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) reason: Reason,
    pub(crate) detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub(crate) reply: Result<String, Failure>,
    pub(crate) usage: Usage,
}

pub(crate) async fn complete(
    provider: &dyn ModelProvider,
    request: &ModelRequest<'_>,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> Result<Outcome, Cancelled> {
    let mut attempt = 1;
    let mut pacing = RetryPacing::Idle;
    loop {
        if cancel.is_cancelled() {
            return Err(Cancelled);
        }
        let mut capture = Capture::default();
        let mut sink = |event: StreamEvent| {
            if let StreamEvent::TextDelta { text } = event {
                capture.append(&text, max_bytes);
            }
        };
        let streamed = provider.stream(request, &mut sink, cancel).await;
        if cancel.is_cancelled() {
            return Err(Cancelled);
        }
        let error = match streamed {
            Ok(completion) => {
                if !capture.saw_content
                    && let Some(content) = &completion.content
                {
                    capture.append(content, max_bytes);
                }
                return Ok(Outcome {
                    reply: settled(&completion, capture),
                    usage: completion.usage,
                });
            }
            Err(error) => error,
        };
        if error.kind == ProviderErrorKind::Cancelled {
            return Err(Cancelled);
        }
        if error.code == OUTPUT_TRUNCATED {
            return Ok(failed(
                Reason::Incomplete,
                format!("finish_reason=length bytes={}", capture.text.len()),
            ));
        }
        let cause = recovery_cause(error.kind).filter(|_| !capture.saw_content);
        let Some(cause) = cause.filter(|_| attempt < DEFAULT_MAX_PROVIDER_ATTEMPTS) else {
            return Ok(rejected(&error));
        };
        let decision = decide(Evidence {
            cause,
            retry_after_seconds: error.retry_after.map(|delay| delay.as_secs()),
            pacing,
            progress: Progress::Unknown,
            output: Output::None,
            tool: ToolEvidence::None,
            recovery_elapsed: None,
        });
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Cancelled),
            () = tokio::time::sleep(decision.delay) => {}
        }
        attempt += 1;
        pacing = decision.next_pacing;
    }
}

fn settled(completion: &Completion, capture: Capture) -> Result<String, Failure> {
    if !completion.tool_calls.is_empty() {
        return Err(Failure {
            reason: Reason::ToolCall,
            detail: String::new(),
        });
    }
    if completion.finish_reason != FinishReason::Stop {
        return Err(Failure {
            reason: Reason::Incomplete,
            detail: format!(
                "finish_reason={} bytes={}",
                finish_reason_name(completion.finish_reason),
                capture.text.len()
            ),
        });
    }
    if capture.observed_bytes > capture.text.len() {
        return Err(Failure {
            reason: Reason::Truncated,
            detail: format!("bytes={}", capture.observed_bytes),
        });
    }
    Ok(capture.text)
}

fn rejected(error: &ProviderError) -> Outcome {
    match answered_kind(error) {
        Some(kind) => {
            let masked = mask_secrets(error.detail.as_deref().unwrap_or_default());
            let safe = terminal_preview(&masked, SAFE_DETAIL_BYTES);
            failed(
                Reason::Provider,
                format!(
                    "kind={kind} detail={}",
                    preview(&safe, DETAIL_PREVIEW_BYTES)
                ),
            )
        }
        None => failed(Reason::Transport, format!("err={}", error.code)),
    }
}

fn failed(reason: Reason, detail: String) -> Outcome {
    Outcome {
        reply: Err(Failure { reason, detail }),
        usage: Usage::default(),
    }
}

fn answered_kind(error: &ProviderError) -> Option<&'static str> {
    error.status?;
    Some(match error.kind {
        ProviderErrorKind::InvalidRequest => "invalid_request",
        ProviderErrorKind::Unauthorized => "unauthorized",
        ProviderErrorKind::Forbidden => "forbidden",
        ProviderErrorKind::RequestTooLarge => "request_too_large",
        ProviderErrorKind::RateLimited => "rate_limited",
        ProviderErrorKind::ServerError => "server_error",
        ProviderErrorKind::BadGateway => "bad_gateway",
        ProviderErrorKind::Unavailable => "unavailable",
        ProviderErrorKind::GatewayTimeout => "gateway_timeout",
        _ => "provider_error",
    })
}

const fn finish_reason_name(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::ToolCalls => "tool_calls",
    }
}

#[derive(Default)]
struct Capture {
    text: String,
    observed_bytes: usize,
    saw_content: bool,
}

impl Capture {
    fn append(&mut self, chunk: &str, max_bytes: usize) {
        self.saw_content |= !chunk.is_empty();
        self.observed_bytes = self.observed_bytes.saturating_add(chunk.len());
        if self.text.len() + chunk.len() <= max_bytes {
            self.text.push_str(chunk);
        }
    }
}

#[cfg(test)]
mod tests;
