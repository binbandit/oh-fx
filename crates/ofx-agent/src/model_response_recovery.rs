use std::time::Duration;

use ofx_contract::{ModelRecoveryAction, ModelRecoveryCause};

pub(crate) const DEFAULT_MAX_PROVIDER_ATTEMPTS: usize = 10;
const MAX_RETRY_AFTER_SECONDS: u64 = 30;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RetryPacing {
    #[default]
    Idle,
    Implicit {
        cause: ModelRecoveryCause,
        attempt: usize,
    },
}

impl RetryPacing {
    fn after_failure(self, cause: ModelRecoveryCause, retry_after_seconds: Option<u64>) -> Self {
        if retry_after_seconds.is_some_and(|seconds| seconds > 0) {
            return Self::Idle;
        }
        match self {
            Self::Implicit {
                cause: previous,
                attempt,
            } if previous == cause => Self::Implicit {
                cause,
                attempt: attempt.saturating_add(1),
            },
            _ => Self::Implicit { cause, attempt: 1 },
        }
    }

    fn attempt(self) -> usize {
        match self {
            Self::Idle => 1,
            Self::Implicit { attempt, .. } => attempt,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) action: ModelRecoveryAction,
    pub(crate) delay: Duration,
    pub(crate) next_pacing: RetryPacing,
}

pub(crate) fn decide(
    cause: ModelRecoveryCause,
    retry_after_seconds: Option<u64>,
    pacing: RetryPacing,
) -> Decision {
    if cause == ModelRecoveryCause::ConnectivityLost {
        let next_pacing = pacing.after_failure(cause, None);
        return Decision {
            action: ModelRecoveryAction::WaitingForConnectivity,
            delay: connectivity_probe_delay(next_pacing.attempt()),
            next_pacing,
        };
    }
    let next_pacing = pacing.after_failure(cause, retry_after_seconds);
    let delay = match retry_after_seconds {
        Some(seconds) if seconds > 0 => Duration::from_secs(seconds.min(MAX_RETRY_AFTER_SECONDS)),
        _ => retry_delay(next_pacing.attempt()),
    };
    Decision {
        action: ModelRecoveryAction::RetryingRequest,
        delay,
        next_pacing,
    }
}

fn connectivity_probe_delay(attempt: usize) -> Duration {
    match attempt {
        0 | 1 => Duration::from_secs(1),
        2 => Duration::from_secs(2),
        _ => Duration::from_secs(5),
    }
}

fn retry_delay(attempt: usize) -> Duration {
    match attempt {
        0 => Duration::ZERO,
        1 => Duration::from_millis(250),
        _ => {
            let mut seconds = 1;
            let mut current = 2;
            while current < attempt && seconds < MAX_RETRY_AFTER_SECONDS {
                seconds = (seconds * 2).min(MAX_RETRY_AFTER_SECONDS);
                current += 1;
            }
            Duration::from_secs(seconds)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE_LIMITED: ModelRecoveryCause = ModelRecoveryCause::RateLimited;
    const UNAVAILABLE: ModelRecoveryCause = ModelRecoveryCause::ProviderUnavailable;
    const INTERRUPTED: ModelRecoveryCause = ModelRecoveryCause::NetworkInterrupted;
    const CONNECTIVITY: ModelRecoveryCause = ModelRecoveryCause::ConnectivityLost;

    #[test]
    fn retry_schedule_uses_the_approved_cap() {
        let expected = [
            Duration::from_millis(250),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ];
        let mut total = Duration::ZERO;
        for (attempt, delay) in (1..).zip(expected) {
            assert_eq!(retry_delay(attempt), delay);
            total += delay;
        }
        assert_eq!(total, Duration::from_millis(121_250));
    }

    #[test]
    fn model_response_recovery_policy_is_deterministic() {
        let first = decide(INTERRUPTED, None, RetryPacing::Idle);
        assert_eq!(first, decide(INTERRUPTED, None, RetryPacing::Idle));
        assert_eq!(first.action, ModelRecoveryAction::RetryingRequest);
        assert_eq!(first.delay, Duration::from_millis(250));
    }

    #[test]
    fn connectivity_loss_waits_with_a_gentle_cadence() {
        let mut pacing = RetryPacing::Idle;
        for expected in [1, 2, 5, 5] {
            let waiting = decide(CONNECTIVITY, None, pacing);
            assert_eq!(waiting.action, ModelRecoveryAction::WaitingForConnectivity);
            assert_eq!(waiting.delay, Duration::from_secs(expected));
            pacing = waiting.next_pacing;
        }
    }

    #[test]
    fn retry_after_is_honoured_up_to_the_cap() {
        for (hint, expected) in [(30, 30), (31, 30), (u64::MAX, 30), (4, 4)] {
            let decision = decide(RATE_LIMITED, Some(hint), RetryPacing::Idle);
            assert_eq!(decision.delay, Duration::from_secs(expected));
            assert_eq!(decision.next_pacing, RetryPacing::Idle);
        }
    }

    #[test]
    fn implicit_retry_pacing_resets_on_a_new_cause_and_ignores_zero_hints() {
        let first = decide(INTERRUPTED, None, RetryPacing::Idle);
        let second = decide(INTERRUPTED, None, first.next_pacing);
        assert_eq!(second.delay, Duration::from_secs(1));
        let switched = decide(UNAVAILABLE, None, second.next_pacing);
        assert_eq!(switched.delay, Duration::from_millis(250));
        let zero_hinted = decide(UNAVAILABLE, Some(0), switched.next_pacing);
        assert_eq!(zero_hinted.delay, Duration::from_secs(1));
        let zero_again = decide(UNAVAILABLE, Some(0), zero_hinted.next_pacing);
        assert_eq!(zero_again.delay, Duration::from_secs(2));
        let timed = decide(UNAVAILABLE, Some(4), zero_again.next_pacing);
        assert_eq!(timed.delay, Duration::from_secs(4));
        assert_eq!(timed.next_pacing, RetryPacing::Idle);
    }
}
