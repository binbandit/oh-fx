use crate::text_utils::is_posix_space;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamingEstimator {
    settled_tokens: u64,
    span_bytes: u64,
}

impl StreamingEstimator {
    pub fn consume(&mut self, text: &str) {
        for &byte in text.as_bytes() {
            if is_posix_space(byte) {
                self.finish_span();
            } else {
                self.span_bytes = self.span_bytes.saturating_add(1);
            }
        }
    }

    pub fn estimate(&self) -> u64 {
        self.settled_tokens
            .saturating_add(estimate_span(self.span_bytes))
    }

    fn finish_span(&mut self) {
        self.settled_tokens = self
            .settled_tokens
            .saturating_add(estimate_span(self.span_bytes));
        self.span_bytes = 0;
    }
}

fn estimate_span(span_bytes: u64) -> u64 {
    span_bytes.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_estimator_is_invariant_across_chunk_boundaries() {
        let text = "split 你好 inside words";
        let mut one_shot = StreamingEstimator::default();
        one_shot.consume(text);
        let expected = one_shot.estimate();

        for split in (0..=text.len()).filter(|&split| text.is_char_boundary(split)) {
            let mut fragmented = StreamingEstimator::default();
            fragmented.consume(&text[..split]);
            fragmented.consume(&text[split..]);
            assert_eq!(fragmented.estimate(), expected);
        }
    }

    #[test]
    fn streaming_estimator_rounds_each_whitespace_separated_span_up_to_four_bytes() {
        let mut estimator = StreamingEstimator::default();
        estimator.consume("a abcd abcde\t\x0b\n你好");
        assert_eq!(estimator.estimate(), 1 + 1 + 2 + 2);
    }
}
