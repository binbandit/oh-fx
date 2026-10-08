use std::time::Duration;

use tokio::time::Instant;

use ofx_contract::{
    ModelRecoveryAction, ModelRecoveryCause, ModelRecoveryRequiredAction, ProviderError,
    ProviderErrorKind,
};

pub(crate) const DEFAULT_MAX_PROVIDER_ATTEMPTS: usize = 10;
const MAX_RETRY_AFTER_SECONDS: u64 = 30;
const BILLABLE_RETRY_WINDOW: Duration = Duration::from_mins(15);
const THROTTLED_RETRY_DELAY: Duration = Duration::from_mins(1);
const STALLED_STREAK: usize = 2;

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
pub(crate) enum Progress {
    Unknown,
    Advancing,
    Stalled,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ProgressTracker {
    last: Option<usize>,
    streak: usize,
}

impl ProgressTracker {
    pub(crate) fn observe(&mut self, streamed_bytes: usize) -> Progress {
        let had_previous = self.last.is_some();
        if self.last == Some(streamed_bytes) {
            self.streak += 1;
        } else {
            self.streak = 0;
        }
        self.last = Some(streamed_bytes);
        if self.streak >= STALLED_STREAK {
            Progress::Stalled
        } else if had_previous {
            Progress::Advancing
        } else {
            Progress::Unknown
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    None,
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolEvidence {
    None,
    ProvenUnexecuted,
    Confirmed,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strategy {
    RetryRequest,
    ContinueResponse,
    RegenerateTool,
    ContinueAfterTool,
    ReconcileTool,
    WaitForConnectivity,
    ProbeLiveness,
    Stop,
}

impl Strategy {
    pub(crate) fn action(self) -> Option<ModelRecoveryAction> {
        match self {
            Self::RetryRequest => Some(ModelRecoveryAction::RetryingRequest),
            Self::ContinueResponse => Some(ModelRecoveryAction::ContinuingResponse),
            Self::RegenerateTool => Some(ModelRecoveryAction::RegeneratingTool),
            Self::ContinueAfterTool => Some(ModelRecoveryAction::ContinuingAfterTool),
            Self::ReconcileTool => Some(ModelRecoveryAction::ReconcilingTool),
            Self::WaitForConnectivity => Some(ModelRecoveryAction::WaitingForConnectivity),
            Self::ProbeLiveness => Some(ModelRecoveryAction::CheckingLiveness),
            Self::Stop => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Evidence {
    pub(crate) cause: ModelRecoveryCause,
    pub(crate) retry_after_seconds: Option<u64>,
    pub(crate) pacing: RetryPacing,
    pub(crate) progress: Progress,
    pub(crate) output: Output,
    pub(crate) tool: ToolEvidence,
    pub(crate) recovery_elapsed: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) strategy: Strategy,
    pub(crate) delay: Duration,
    pub(crate) next_pacing: RetryPacing,
    pub(crate) required_action: ModelRecoveryRequiredAction,
}

impl Decision {
    fn paced(strategy: Strategy, delay: Duration, next_pacing: RetryPacing) -> Self {
        Self {
            strategy,
            delay,
            next_pacing,
            required_action: ModelRecoveryRequiredAction::None,
        }
    }
}

pub(crate) fn decide(evidence: Evidence) -> Decision {
    let cause = evidence.cause;
    if cause == ModelRecoveryCause::ConnectivityLost {
        let next_pacing = evidence.pacing.after_failure(cause, None);
        return Decision::paced(
            Strategy::WaitForConnectivity,
            connectivity_probe_delay(next_pacing.attempt()),
            next_pacing,
        );
    }
    if evidence.progress == Progress::Stalled {
        return Decision {
            strategy: Strategy::Stop,
            delay: Duration::ZERO,
            next_pacing: RetryPacing::Idle,
            required_action: ModelRecoveryRequiredAction::SurfaceStall,
        };
    }
    let throttled = evidence
        .recovery_elapsed
        .is_some_and(|elapsed| elapsed > BILLABLE_RETRY_WINDOW);
    if cause == ModelRecoveryCause::ProviderStreamTimeout {
        let next_pacing = evidence.pacing.after_failure(cause, None);
        let delay = if throttled {
            THROTTLED_RETRY_DELAY
        } else {
            retry_delay(next_pacing.attempt())
        };
        return Decision::paced(Strategy::ProbeLiveness, delay, next_pacing);
    }
    let next_pacing = evidence
        .pacing
        .after_failure(cause, evidence.retry_after_seconds);
    let delay = match evidence.retry_after_seconds {
        Some(seconds) if seconds > 0 => {
            let hinted = Duration::from_secs(seconds.min(MAX_RETRY_AFTER_SECONDS));
            if throttled {
                hinted.max(THROTTLED_RETRY_DELAY)
            } else {
                hinted
            }
        }
        _ if throttled => THROTTLED_RETRY_DELAY,
        _ => retry_delay(next_pacing.attempt()),
    };
    let strategy = match (evidence.tool, evidence.output) {
        (ToolEvidence::ProvenUnexecuted, _) => Strategy::RegenerateTool,
        (ToolEvidence::Confirmed, _) => Strategy::ContinueAfterTool,
        (ToolEvidence::Uncertain, _) => Strategy::ReconcileTool,
        (ToolEvidence::None, Output::Partial) => Strategy::ContinueResponse,
        (ToolEvidence::None, Output::None) => Strategy::RetryRequest,
    };
    Decision::paced(strategy, delay, next_pacing)
}

#[derive(Debug, Default)]
pub(crate) struct Recovery {
    pacing: RetryPacing,
    progress: ProgressTracker,
    started: Option<Instant>,
}

impl Recovery {
    pub(crate) fn decide(
        &mut self,
        cause: ModelRecoveryCause,
        error: &ProviderError,
        streamed_bytes: usize,
        (output, tool): (Output, ToolEvidence),
    ) -> Decision {
        let started = *self.started.get_or_insert_with(Instant::now);
        let decision = decide(Evidence {
            cause,
            retry_after_seconds: error.retry_after.map(|delay| delay.as_secs()),
            pacing: self.pacing,
            progress: if tracks_progress(cause, error) {
                self.progress.observe(streamed_bytes)
            } else {
                Progress::Unknown
            },
            output,
            tool,
            recovery_elapsed: Some(started.elapsed()),
        });
        self.pacing = decision.next_pacing;
        decision
    }
}

pub(crate) fn failed_in_stream(cause: ModelRecoveryCause, error: &ProviderError) -> bool {
    error.status.is_none()
        && matches!(
            cause,
            ModelRecoveryCause::ProviderUnavailable | ModelRecoveryCause::RateLimited
        )
}

fn tracks_progress(cause: ModelRecoveryCause, error: &ProviderError) -> bool {
    match cause {
        ModelRecoveryCause::NetworkInterrupted
        | ModelRecoveryCause::ConnectivityLost
        | ModelRecoveryCause::ProviderStreamTimeout => true,
        ModelRecoveryCause::ProviderUnavailable => error.status.is_none(),
        ModelRecoveryCause::RateLimited => false,
    }
}

pub(crate) fn recovery_cause(kind: ProviderErrorKind) -> Option<ModelRecoveryCause> {
    match kind {
        ProviderErrorKind::RateLimited => Some(ModelRecoveryCause::RateLimited),
        ProviderErrorKind::ServerError
        | ProviderErrorKind::BadGateway
        | ProviderErrorKind::Unavailable
        | ProviderErrorKind::GatewayTimeout => Some(ModelRecoveryCause::ProviderUnavailable),
        ProviderErrorKind::ConnectivityLost => Some(ModelRecoveryCause::ConnectivityLost),
        ProviderErrorKind::TransportInterrupted | ProviderErrorKind::Timeout => {
            Some(ModelRecoveryCause::NetworkInterrupted)
        }
        ProviderErrorKind::StreamStalled => Some(ModelRecoveryCause::ProviderStreamTimeout),
        _ => None,
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
mod tests;
