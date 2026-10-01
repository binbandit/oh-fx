use std::time::Duration;

const WAIT_LIMIT: Duration = Duration::from_secs(1);
const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct NaturalDrain {
    waited: Duration,
    bytes: usize,
}

impl NaturalDrain {
    pub(super) fn record(&mut self, read: usize, waited: Duration) {
        self.bytes = self.bytes.saturating_add(read);
        self.waited = self.waited.saturating_add(waited);
    }

    pub(super) fn remaining_wait(self) -> Option<Duration> {
        if self.bytes >= MAX_BYTES {
            return None;
        }
        WAIT_LIMIT
            .checked_sub(self.waited)
            .filter(|remaining| !remaining.is_zero())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_time_spent_waiting_for_output_counts_toward_the_wait_limit() {
        let mut drain = NaturalDrain::default();
        for _ in 0..100 {
            drain.record(4096, Duration::ZERO);
        }
        assert_eq!(drain.remaining_wait(), Some(WAIT_LIMIT));
        drain.record(16, Duration::from_millis(999));
        assert_eq!(drain.remaining_wait(), Some(Duration::from_millis(1)));
        drain.record(0, Duration::from_millis(1));
        assert_eq!(drain.remaining_wait(), None);
    }

    #[test]
    fn the_byte_budget_ends_a_drain_that_never_waits() {
        let mut drain = NaturalDrain::default();
        drain.record(MAX_BYTES - 1, Duration::ZERO);
        assert!(drain.remaining_wait().is_some());
        drain.record(1, Duration::ZERO);
        assert_eq!(drain.remaining_wait(), None);
    }
}
