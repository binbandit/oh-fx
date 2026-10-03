use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineRead {
    Line(Vec<u8>),
    Overflow,
    Incomplete,
}

pub struct LineReader<R> {
    inner: R,
    discarding: bool,
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            discarding: false,
        }
    }

    pub async fn read_line(&mut self, limit: impl Fn() -> usize) -> io::Result<Option<LineRead>> {
        if self.discarding && !self.discard_rest_of_line().await? {
            return Ok(None);
        }
        let mut line = Vec::new();
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                return Ok((!line.is_empty()).then_some(LineRead::Incomplete));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let chunk = &available[..newline.unwrap_or(available.len())];
            if line.len() + chunk.len() > limit() {
                let consumed = chunk.len();
                self.inner.consume(consumed);
                self.discarding = newline.is_none();
                if newline.is_some() {
                    self.inner.consume(1);
                }
                return Ok(Some(LineRead::Overflow));
            }
            line.extend_from_slice(chunk);
            let consumed = chunk.len() + usize::from(newline.is_some());
            self.inner.consume(consumed);
            if newline.is_none() {
                continue;
            }
            if line.is_empty() {
                continue;
            }
            return Ok(Some(LineRead::Line(line)));
        }
    }

    async fn discard_rest_of_line(&mut self) -> io::Result<bool> {
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                self.discarding = false;
                return Ok(false);
            }
            if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                self.inner.consume(newline + 1);
                self.discarding = false;
                return Ok(true);
            }
            let consumed = available.len();
            self.inner.consume(consumed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, BufReader, ReadBuf};

    use super::*;

    struct ChunkedTestSource {
        bytes: &'static [u8],
        offset: usize,
        max_chunk: usize,
    }

    impl AsyncRead for ChunkedTestSource {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let remaining = self.bytes.len() - self.offset;
            let count = remaining.min(buffer.remaining()).min(self.max_chunk);
            let start = self.offset;
            buffer.put_slice(&self.bytes[start..start + count]);
            self.offset += count;
            Poll::Ready(Ok(()))
        }
    }

    fn reader(bytes: &'static [u8], max_chunk: usize) -> LineReader<BufReader<ChunkedTestSource>> {
        LineReader::new(BufReader::with_capacity(
            max_chunk,
            ChunkedTestSource {
                bytes,
                offset: 0,
                max_chunk,
            },
        ))
    }

    #[tokio::test]
    async fn reader_accepts_an_exact_fragmented_frame() {
        let mut reader = reader(b"12345678\n", 2);
        assert_eq!(
            reader.read_line(|| 8).await.unwrap(),
            Some(LineRead::Line(b"12345678".to_vec()))
        );
    }

    #[tokio::test]
    async fn reader_drains_one_oversized_fragmented_frame_and_resumes_at_the_next_frame() {
        let mut reader = reader(b"123456789\n{\"id\":2}\n", 3);
        assert_eq!(
            reader.read_line(|| 8).await.unwrap(),
            Some(LineRead::Overflow)
        );
        assert_eq!(
            reader.read_line(|| 8).await.unwrap(),
            Some(LineRead::Line(b"{\"id\":2}".to_vec()))
        );
    }

    #[tokio::test]
    async fn reader_reports_one_overflow_when_an_oversized_frame_ends_at_eof() {
        let mut reader = reader(b"123456789", 2);
        assert_eq!(
            reader.read_line(|| 8).await.unwrap(),
            Some(LineRead::Overflow)
        );
        assert_eq!(reader.read_line(|| 8).await.unwrap(), None);
    }

    #[tokio::test]
    async fn skips_empty_lines_and_reports_a_partial_final_line() {
        let mut reader = reader(b"\n\none\npartial", 4);
        assert_eq!(
            reader.read_line(|| 8).await.unwrap(),
            Some(LineRead::Line(b"one".to_vec()))
        );
        assert_eq!(
            reader.read_line(|| 8).await.unwrap(),
            Some(LineRead::Incomplete)
        );
        assert_eq!(reader.read_line(|| 8).await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_limit_is_read_again_for_every_chunk_of_a_frame() {
        let mut reader = reader(b"123456789\n", 2);
        let reads = std::cell::Cell::new(0);
        let limit = || {
            reads.set(reads.get() + 1);
            if reads.get() == 1 { 4 } else { 16 }
        };
        assert_eq!(
            reader.read_line(limit).await.unwrap(),
            Some(LineRead::Line(b"123456789".to_vec()))
        );
        assert!(reads.get() > 1);
    }
}
