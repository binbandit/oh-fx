use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
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
    deadline: watch::Sender<Option<Instant>>,
    open_inputs: Mutex<usize>,
}

pub(crate) struct InputPause<'a> {
    gate: &'a DeadlineGate,
    operation_timeout: Duration,
}

impl DeadlineGate {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline: watch::Sender::new(Some(deadline)),
            open_inputs: Mutex::new(0),
        }
    }

    pub(crate) fn pause_for_input(&self, operation_timeout: Duration) -> InputPause<'_> {
        *self.open_inputs() += 1;
        self.deadline.send_replace(None);
        InputPause {
            gate: self,
            operation_timeout,
        }
    }

    fn open_inputs(&self) -> MutexGuard<'_, usize> {
        self.open_inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) async fn expired(&self) {
        let mut deadline = self.deadline.subscribe();
        loop {
            let current = *deadline.borrow_and_update();
            tokio::select! {
                () = expiry(current) => return,
                changed = deadline.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

async fn expiry(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

impl Drop for InputPause<'_> {
    fn drop(&mut self) {
        let mut open_inputs = self.gate.open_inputs();
        *open_inputs -= 1;
        if *open_inputs == 0 {
            self.gate
                .deadline
                .send_replace(Some(Instant::now() + self.operation_timeout));
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
        let paused = gate.pause_for_input(Duration::from_millis(1500));
        assert!(stays_paused(&gate).await);
        drop(paused);
        gate.expired().await;
        assert_eq!(
            Instant::now(),
            start + Duration::from_hours(1) + Duration::from_millis(1500)
        );
    }

    async fn stays_paused(gate: &DeadlineGate) -> bool {
        tokio::time::timeout(Duration::from_hours(1), gate.expired())
            .await
            .is_err()
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_stays_paused_until_the_last_overlapping_input_ends() {
        let timeout = Duration::from_secs(5);
        for first_ends_first in [true, false] {
            let gate = DeadlineGate::new(Instant::now() + timeout);
            let first = gate.pause_for_input(timeout);
            let second = gate.pause_for_input(timeout);
            if first_ends_first {
                drop(first);
                assert!(stays_paused(&gate).await);
                drop(second);
            } else {
                drop(second);
                assert!(stays_paused(&gate).await);
                drop(first);
            }
            let resumed = Instant::now();
            gate.expired().await;
            assert_eq!(Instant::now(), resumed + timeout);
        }
    }
}
