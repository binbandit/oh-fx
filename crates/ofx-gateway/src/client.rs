use reqwest::{RequestBuilder, StatusCode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundedFailure {
    Cancelled,
    Failed,
}

pub(crate) async fn bounded_get(
    request: RequestBuilder,
    max_bytes: usize,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<(StatusCode, Vec<u8>), BoundedFailure> {
    let operation = async {
        let mut response = request.send().await.map_err(|_| BoundedFailure::Failed)?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| BoundedFailure::Failed)? {
            if body.len() + chunk.len() > max_bytes {
                return Err(BoundedFailure::Failed);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(BoundedFailure::Cancelled),
        outcome = tokio::time::timeout_at(deadline, operation) => {
            outcome.unwrap_or(Err(BoundedFailure::Failed))
        }
    }
}
