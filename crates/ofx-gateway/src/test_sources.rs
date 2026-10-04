use std::collections::VecDeque;
use std::time::Duration;

use crate::chat_completions::ChunkSource;

pub(crate) struct Paced(VecDeque<(Duration, Vec<u8>)>);

impl Paced {
    pub(crate) fn new<const N: usize>(chunks: [(Duration, &str); N]) -> Self {
        Self(
            chunks
                .into_iter()
                .map(|(delay, chunk)| (delay, chunk.as_bytes().to_vec()))
                .collect(),
        )
    }
}

impl ChunkSource for Paced {
    async fn next_chunk(&mut self) -> Result<Option<impl AsRef<[u8]> + Send>, String> {
        let Some((delay, chunk)) = self.0.pop_front() else {
            return std::future::pending().await;
        };
        tokio::time::sleep(delay).await;
        Ok(Some(chunk))
    }
}
