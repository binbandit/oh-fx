use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, sleep_until};

pub(crate) const ELICITATION_TIMEOUT: Duration = Duration::from_mins(30);

pub(crate) fn monotonic_millis() -> u64 {
    static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
    let origin = *ORIGIN.get_or_init(std::time::Instant::now);
    u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug)]
pub(crate) struct DeadlineGate {
    deadline: watch::Sender<Instant>,
}

impl DeadlineGate {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline: watch::Sender::new(deadline),
        }
    }

    pub(crate) fn pause_for_input(&self) -> Instant {
        let deadline = Instant::now() + ELICITATION_TIMEOUT;
        self.deadline.send_replace(deadline);
        deadline
    }

    pub(crate) fn resume(&self, timeout: Duration) {
        self.deadline.send_replace(Instant::now() + timeout);
    }

    pub(crate) async fn expired(&self) {
        let mut deadline = self.deadline.subscribe();
        loop {
            let current = *deadline.borrow_and_update();
            tokio::select! {
                () = sleep_until(current) => return,
                changed = deadline.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_never_runs_backwards() {
        let first = monotonic_millis();
        let second = monotonic_millis();
        assert!(second >= first);
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_gate_pauses_input_and_resumes_with_one_fresh_operation_budget() {
        let start = Instant::now();
        let gate = DeadlineGate::new(start + Duration::from_secs(1));
        let paused = gate.pause_for_input();
        assert_eq!(paused, start + ELICITATION_TIMEOUT);
        let expired = gate.expired();
        tokio::pin!(expired);
        assert!(
            tokio::time::timeout(Duration::from_mins(20), &mut expired)
                .await
                .is_err()
        );
        gate.resume(Duration::from_millis(1500));
        expired.await;
        assert_eq!(
            Instant::now(),
            start + Duration::from_mins(20) + Duration::from_millis(1500)
        );
    }
}
