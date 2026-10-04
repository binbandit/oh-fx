use ofx_contract::{FinishReason, ModelProvider, ModelRequest, ProviderErrorKind, StreamEvent};
use tokio_util::sync::CancellationToken;

use crate::model_response_recovery::{
    DEFAULT_MAX_PROVIDER_ATTEMPTS, Evidence, Output, Progress, RetryPacing, decide, recovery_cause,
};

const OUTPUT_TRUNCATED: &str = "OutputTruncated";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    Cancelled,
    Incomplete,
    Unusable,
}

pub(crate) async fn complete(
    provider: &dyn ModelProvider,
    request: &ModelRequest<'_>,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> Result<String, Failure> {
    let mut attempt = 1;
    let mut pacing = RetryPacing::Idle;
    loop {
        if cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        let mut capture = Capture::default();
        let mut sink = |event: StreamEvent| {
            if let StreamEvent::TextDelta { text } = event {
                capture.append(&text, max_bytes);
            }
        };
        let streamed = provider.stream(request, &mut sink, cancel).await;
        if cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        let error = match streamed {
            Ok(completion) => {
                if !capture.saw_content
                    && let Some(content) = &completion.content
                {
                    capture.append(content, max_bytes);
                }
                if !completion.tool_calls.is_empty()
                    || completion.finish_reason != FinishReason::Stop
                    || capture.observed_bytes > capture.text.len()
                {
                    return Err(Failure::Unusable);
                }
                return Ok(capture.text);
            }
            Err(error) => error,
        };
        if error.kind == ProviderErrorKind::Cancelled {
            return Err(Failure::Cancelled);
        }
        if error.code == OUTPUT_TRUNCATED {
            return Err(Failure::Incomplete);
        }
        let cause = recovery_cause(error.kind).filter(|_| !capture.saw_content);
        let Some(cause) = cause.filter(|_| attempt < DEFAULT_MAX_PROVIDER_ATTEMPTS) else {
            return Err(Failure::Unusable);
        };
        let decision = decide(Evidence {
            cause,
            retry_after_seconds: error.retry_after.map(|delay| delay.as_secs()),
            pacing,
            progress: Progress::Unknown,
            output: Output::None,
            recovery_elapsed: None,
        });
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Failure::Cancelled),
            () = tokio::time::sleep(decision.delay) => {}
        }
        attempt += 1;
        pacing = decision.next_pacing;
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
