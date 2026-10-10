use ofx_trace::trace_log;
use reqwest::{RequestBuilder, StatusCode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

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
