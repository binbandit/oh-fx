use std::time::Duration;

use ofx_contract::{ProviderError, ProviderErrorKind};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::chat_completions::{ChunkSource, sanitized};

const STREAM_STALL_TIMEOUT: Duration = Duration::from_mins(10);

pub(crate) struct StallWatch {
    stalls_at: Instant,
}

impl StallWatch {
    pub(crate) fn start() -> Self {
        Self {
            stalls_at: Instant::now() + STREAM_STALL_TIMEOUT,
        }
    }

    pub(crate) fn progressed(&mut self) {
        self.stalls_at = Instant::now() + STREAM_STALL_TIMEOUT;
    }

    pub(crate) async fn next_chunk<S: ChunkSource>(
        &self,
        source: &mut S,
        cancel: &CancellationToken,
        secrets: &[String],
    ) -> Result<Option<impl AsRef<[u8]> + Send>, ProviderError> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(ProviderError::cancelled()),
            chunk = source.next_chunk() => chunk.map_err(|detail| {
                ProviderError::new(ProviderErrorKind::TransportInterrupted, "ReadFailed")
                    .with_detail(sanitized(detail, secrets))
            }),
            () = tokio::time::sleep_until(self.stalls_at) => {
                Err(ProviderError::new(ProviderErrorKind::StreamStalled, "StreamStalled"))
            }
        }
    }
}
