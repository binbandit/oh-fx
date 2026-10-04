use ofx_contract::{
    Completion, ModelFailureDiagnostic, ModelRecoveryAction, ModelRecoveryCause,
    ModelRecoveryRequiredAction, ProviderError, RecoveryProgress, RecoveryStrategy,
    RouteRecoveryKind, RouteRecoveryStatus, UiEvent,
};

use super::recovery::Restart;
use super::{Agent, EventSink, Stop, Turn, TurnFailure, failure_diagnostic, recovered_status};
use crate::model_response_recovery::{
    DEFAULT_MAX_PROVIDER_ATTEMPTS, ToolEvidence, failed_in_stream,
};

const UNEXPECTED_TOOL_CALL: &str = "UnexpectedToolCallDuringReconciliation";

pub(super) struct Pause {
    pub(super) cause: ModelRecoveryCause,
    pub(super) attempt: usize,
    pub(super) required_action: ModelRecoveryRequiredAction,
    pub(super) diagnostic: ModelFailureDiagnostic,
}

pub(super) fn paused_required_action(evidence: ToolEvidence) -> ModelRecoveryRequiredAction {
    if evidence == ToolEvidence::Uncertain {
        ModelRecoveryRequiredAction::InspectUncertainTool
    } else {
        ModelRecoveryRequiredAction::ContinueLater
    }
}

impl Agent {
    pub(super) fn pause(
        &self,
        turn: &Turn,
        pause: Pause,
        restart: &Restart<'_>,
        events: EventSink<'_>,
    ) -> Stop {
        let partial = restart.partial().to_owned();
        let source = restart.source(&turn.language.stage);
        let recorded = self.record_recovery(
            turn,
            pause.cause,
            RecoveryProgress::Paused,
            pause.attempt,
            source,
        );
        if let Err(failure) = recorded {
            return Stop::Failed {
                failure: TurnFailure::Persistence(failure),
                partial,
            };
        }
        events(UiEvent::Recovery {
            turn_id: turn.id,
            status: RouteRecoveryStatus {
                kind: RouteRecoveryKind::TerminalProviderError,
                failed_attempt: pause.attempt,
                succeeded_attempt: 0,
                attempt_limit: DEFAULT_MAX_PROVIDER_ATTEMPTS,
                cause: Some(pause.cause),
                action: Some(ModelRecoveryAction::Paused),
                required_action: pause.required_action,
                delay_seconds: 0,
                diagnostic: Some(pause.diagnostic),
                retry_wait: None,
            },
        });
        Stop::Failed {
            failure: TurnFailure::RecoveryPaused,
            partial,
        }
    }

    pub(super) fn completed(
        &self,
        turn: &mut Turn,
        completion: Completion,
        (recovering, attempt, observed): (bool, usize, ToolEvidence),
        restart: &Restart<'_>,
        events: EventSink<'_>,
    ) -> Result<Completion, Stop> {
        let called = observed != ToolEvidence::None || !completion.tool_calls.is_empty();
        if called && turn.recovery == Some(RecoveryStrategy::ReconcileTool) {
            return Err(self.unexpected_tool_call(turn, attempt, restart, events));
        }
        if recovering {
            events(UiEvent::Recovery {
                turn_id: turn.id,
                status: recovered_status(attempt),
            });
        }
        Ok(completion)
    }

    pub(super) fn reconcile_broken(
        &self,
        turn: &mut Turn,
        (observed, cause, error): (ToolEvidence, ModelRecoveryCause, &ProviderError),
        (attempt, consumed): (usize, usize),
        restart: &Restart<'_>,
        events: EventSink<'_>,
    ) -> Result<(), Stop> {
        if turn.recovery != Some(RecoveryStrategy::ReconcileTool) || observed == ToolEvidence::None
        {
            return Ok(());
        }
        if failed_in_stream(cause, error) {
            return Err(self.unexpected_tool_call(turn, attempt, restart, events));
        }
        let pause = Pause {
            cause,
            attempt: consumed,
            required_action: ModelRecoveryRequiredAction::InspectUncertainTool,
            diagnostic: failure_diagnostic(error),
        };
        Err(self.pause(turn, pause, restart, events))
    }

    fn unexpected_tool_call(
        &self,
        turn: &mut Turn,
        attempt: usize,
        restart: &Restart<'_>,
        events: EventSink<'_>,
    ) -> Stop {
        turn.tool_evidence = ToolEvidence::Uncertain;
        let pause = Pause {
            cause: turn
                .recovery_cause
                .unwrap_or(ModelRecoveryCause::NetworkInterrupted),
            attempt,
            required_action: ModelRecoveryRequiredAction::InspectUncertainTool,
            diagnostic: ModelFailureDiagnostic::new(UNEXPECTED_TOOL_CALL),
        };
        self.pause(turn, pause, restart, events)
    }
}
