use ofx_contract::{ModelRecoveryAction, ModelRecoveryCause, ProviderError, RecoveryProgress};
use ofx_text::mask_secrets;
use ofx_trace::{TraceContext, preview, terminal_preview, trace_event, trace_log};

use crate::gateway_step::{CONTENT_FILTER, ERROR, MISSING_FINISH, Settlement, failure_settlement};
use crate::model_response_recovery::{DEFAULT_MAX_PROVIDER_ATTEMPTS, Strategy, ToolEvidence};

const AGENT: &str = "agent";
const GATEWAY: &str = "gateway";
const SAFE_DETAIL_BYTES: usize = 512;
const DETAIL_PREVIEW_BYTES: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Recovering {
    Retry(Strategy),
    Stop,
    Stall,
    Pause,
    Cancelled,
}

pub(super) struct FailedAttempt<'a> {
    pub(super) error: &'a ProviderError,
    pub(super) attempt: usize,
    pub(super) consumed: usize,
    pub(super) spoke: bool,
    pub(super) tool: ToolEvidence,
    pub(super) cancel_requested: bool,
    pub(super) replay_safe: bool,
    pub(super) partial_bytes: usize,
}

pub(super) struct Route<'a> {
    pub(super) selected_model: &'a str,
    pub(super) model: &'a str,
    pub(super) fast_mode: bool,
}

impl Recovering {
    const fn name(self) -> &'static str {
        match self {
            Self::Retry(strategy) => strategy_name(strategy),
            Self::Stop | Self::Stall | Self::Cancelled => "stop",
            Self::Pause => "pause",
        }
    }

    const fn retries(self) -> bool {
        matches!(self, Self::Retry(_))
    }
}

pub(super) fn failed_attempt(
    context: TraceContext,
    failed: &FailedAttempt<'_>,
    route: &Route<'_>,
    recovering: Recovering,
) {
    match failure_settlement(failed.error) {
        Settlement::Failed(code) => stream_error(context, failed, code, recovering),
        Settlement::Finished(_) if recovering == Recovering::Cancelled => {}
        Settlement::Finished(reason) if reason == ERROR || reason == CONTENT_FILTER => {
            route_failure(context, failed, route, reason, recovering);
            if recovering == Recovering::Stop {
                trace_event!(
                    AGENT,
                    "provider_completion_failed",
                    context,
                    "finish_reason={reason} content_bytes={} tool_call_count=0",
                    failed.partial_bytes
                );
            }
        }
        Settlement::Finished(MISSING_FINISH) if recovering == Recovering::Stop => {
            trace_event!(
                AGENT,
                "provider_finish_missing",
                context,
                "content_bytes={} tool_call_count=0",
                failed.partial_bytes
            );
        }
        Settlement::Completed(_) | Settlement::Finished(_) | Settlement::Answered(_) => {}
    }
}

fn stream_error(
    context: TraceContext,
    failed: &FailedAttempt<'_>,
    code: &str,
    recovering: Recovering,
) {
    trace_event!(
        GATEWAY,
        "stream_error",
        context,
        "err={code} cancel_requested={} provider_attempts={}/{DEFAULT_MAX_PROVIDER_ATTEMPTS} saw_content={} saw_tool_start={} saw_provider_tool_start={} recovery={} replay_safe={} retry={}",
        failed.cancel_requested,
        failed.consumed,
        failed.spoke,
        failed.tool != ToolEvidence::None,
        failed.tool == ToolEvidence::Uncertain,
        recovering.name(),
        failed.replay_safe,
        recovering.retries(),
    );
}

fn route_failure(
    context: TraceContext,
    failed: &FailedAttempt<'_>,
    route: &Route<'_>,
    reason: &str,
    recovering: Recovering,
) {
    if !ofx_trace::enabled(AGENT) {
        return;
    }
    let error = failed.error;
    let masked = mask_secrets(
        error
            .diagnostic
            .as_deref()
            .or(error.detail.as_deref())
            .unwrap_or_default(),
    );
    let safe = terminal_preview(&masked, SAFE_DETAIL_BYTES);
    trace_event!(
        AGENT,
        "route_failure",
        context,
        "selected_model={} route={} fast_mode={} semantic_attempt={}/{DEFAULT_MAX_PROVIDER_ATTEMPTS} http_status=200 finish_reason={reason} saw_content={} saw_tool_start={} retry={} detail={}",
        route.selected_model,
        route.model,
        route.fast_mode,
        failed.attempt,
        failed.spoke,
        failed.tool != ToolEvidence::None,
        recovering.retries(),
        preview(&safe, DETAIL_PREVIEW_BYTES),
    );
}

pub(super) fn recovery_fast_fallback(context: TraceContext, failed_route: &str, selected: &str) {
    trace_event!(
        AGENT,
        "recovery_fast_fallback",
        context,
        "failed_route={failed_route} selected_model={selected}"
    );
}

pub(super) fn assistant_prefill_recovery(context: TraceContext, tool_name: &str, attempt: usize) {
    trace_event!(
        GATEWAY,
        "assistant_prefill_recovery",
        context,
        "tool_name={tool_name} provider_attempt={attempt}/{DEFAULT_MAX_PROVIDER_ATTEMPTS}"
    );
}

pub(super) fn recovery_checkpoint_set(
    context: TraceContext,
    consumed: usize,
    cause: ModelRecoveryCause,
    progress: RecoveryProgress,
) {
    trace_event!(
        AGENT,
        "recovery_checkpoint_set",
        context,
        "provider_attempts={consumed}/{DEFAULT_MAX_PROVIDER_ATTEMPTS} outstanding=false cause={} action={}",
        cause_name(cause),
        progress_name(progress),
    );
}

pub(super) fn restarting_response(preview_bytes: usize, superseded_bytes: usize) {
    trace_log!(
        AGENT,
        "restarting response preview_bytes={preview_bytes} superseded_preview_bytes={superseded_bytes}"
    );
}

pub(super) fn recovery_clear_failed(occasion: &str, code: &str) {
    trace_log!(
        AGENT,
        "recovery checkpoint clear on {occasion} failed err={code}"
    );
}

const fn strategy_name(strategy: Strategy) -> &'static str {
    match strategy {
        Strategy::RetryRequest => "retry_request",
        Strategy::ContinueResponse => "continue_response",
        Strategy::RegenerateTool => "regenerate_tool",
        Strategy::ContinueAfterTool => "continue_after_confirmed_tool",
        Strategy::ReconcileTool => "reconcile_tool",
        Strategy::WaitForConnectivity => "wait_for_connectivity",
        Strategy::ProbeLiveness => "probe_liveness",
        Strategy::Stop => "stop",
    }
}

const fn cause_name(cause: ModelRecoveryCause) -> &'static str {
    match cause {
        ModelRecoveryCause::NetworkInterrupted => "transport_interrupted",
        other => other.as_str(),
    }
}

const fn progress_name(progress: RecoveryProgress) -> &'static str {
    match progress {
        RecoveryProgress::Waiting(action) => match action {
            ModelRecoveryAction::RetryingRequest => "retry_request",
            ModelRecoveryAction::ContinuingResponse => "continue_response",
            ModelRecoveryAction::RegeneratingTool => "regenerate_tool",
            ModelRecoveryAction::ContinuingAfterTool => "continue_after_confirmed_tool",
            ModelRecoveryAction::ReconcilingTool => "reconcile_tool",
            ModelRecoveryAction::WaitingForConnectivity => "wait_for_connectivity",
            ModelRecoveryAction::CheckingLiveness => "probe_liveness",
            ModelRecoveryAction::Paused => "pause",
        },
        RecoveryProgress::Paused => "pause",
    }
}

#[cfg(test)]
mod tests;
