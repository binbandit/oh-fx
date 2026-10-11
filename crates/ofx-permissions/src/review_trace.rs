use std::time::Instant;

use ofx_contract::{CommandRequest, GatedAction, ReviewFailure, ReviewVerdict, Reviewed, ToolCall};
use ofx_trace::trace_log;

const PERMISSION: &str = "permission";

pub(crate) fn unavailable_before_review(call: &ToolCall, failure: ReviewFailure) {
    trace_log!(
        PERMISSION,
        "event=auto_review_result tool_name={} decision=unavailable fallback_reason={} recovery=agent_replan execution_started=false call_id={}",
        call.name,
        failure.as_str(),
        call.id.as_str()
    );
}

pub(crate) fn budget_exhausted(call: &ToolCall) {
    trace_log!(
        PERMISSION,
        "event=auto_review_budget_exhausted tool_name={} call_id={} execution_started=false",
        call.name,
        call.id.as_str()
    );
    unavailable_before_review(call, ReviewFailure::TurnReviewBudgetExhausted);
}

pub(crate) fn started(call: &ToolCall, action: GatedAction<'_>) -> Instant {
    trace_log!(
        PERMISSION,
        "event=auto_review_start tool_name={} action_kind={} call_id={}",
        call.name,
        action_kind(action),
        call.id.as_str()
    );
    Instant::now()
}

pub(crate) fn finished(call: &ToolCall, started: Instant, reviewed: Option<&Reviewed>) {
    let (name, id) = (&call.name, call.id.as_str());
    let elapsed = started.elapsed().as_millis();
    match reviewed.map(|reviewed| &reviewed.verdict) {
        None => trace_log!(
            PERMISSION,
            "event=auto_review_result tool_name={name} decision=cancelled_or_error fallback_reason=Cancelled elapsed_ms={elapsed} execution_started=false call_id={id}"
        ),
        Some(ReviewVerdict::EvidenceIncomplete) => trace_log!(
            PERMISSION,
            "event=auto_review_result tool_name={name} decision=held fallback_reason=review_evidence_incomplete recovery=agent_replan elapsed_ms={elapsed} execution_started=false call_id={id}"
        ),
        Some(ReviewVerdict::Unavailable(failure)) => trace_log!(
            PERMISSION,
            "event=auto_review_result tool_name={name} decision=unavailable fallback_reason={} recovery=agent_replan elapsed_ms={elapsed} execution_started=false call_id={id}",
            failure.as_str()
        ),
        Some(verdict) => trace_log!(
            PERMISSION,
            "event=auto_review_result tool_name={name} decision={} fallback_reason=none elapsed_ms={elapsed} execution_started=false call_id={id}",
            if *verdict == ReviewVerdict::Clear {
                "clear"
            } else {
                "caution"
            }
        ),
    }
}

const fn action_kind(action: GatedAction<'_>) -> &'static str {
    match action {
        GatedAction::Command(CommandRequest::Run { .. }) => "command",
        GatedAction::Command(CommandRequest::SendInput { .. }) => "shell_input",
        GatedAction::FileMutation(_) => "file_mutation",
        GatedAction::Command(CommandRequest::Observe | CommandRequest::Stop)
        | GatedAction::Call(_)
        | GatedAction::McpTool(_) => "tool",
    }
}

pub(crate) struct Composing<'a> {
    pub(crate) started: Instant,
    pub(crate) target_call_id: &'a str,
}

impl<'a> Composing<'a> {
    pub(crate) fn start(
        view: &str,
        (source_model, reviewer_model): (&str, &str),
        (pending_calls, trusted_root_bytes): (usize, usize),
        target_call_id: &'a str,
    ) -> Self {
        trace_log!(
            PERMISSION,
            "event=auto_review_compose_start origin=root view={view} source_model={source_model} reviewer_model={reviewer_model} pending_calls={pending_calls} trusted_root_bytes={trusted_root_bytes} target_call_id={target_call_id}"
        );
        Self {
            started: Instant::now(),
            target_call_id,
        }
    }

    pub(crate) fn failed(&self, result: &str) {
        trace_log!(
            PERMISSION,
            "event=auto_review_compose_result result={result} elapsed_ms={} target_call_id={}",
            self.started.elapsed().as_millis(),
            self.target_call_id
        );
    }

    pub(crate) fn ready(&self, payload_bytes: usize) {
        trace_log!(
            PERMISSION,
            "event=auto_review_compose_result result=ready payload_bytes={payload_bytes} elapsed_ms={} target_call_id={}",
            self.started.elapsed().as_millis(),
            self.target_call_id
        );
    }

    pub(crate) fn send(&self, attempt: u8) {
        trace_log!(
            PERMISSION,
            "event=auto_review_send attempt={attempt} max_attempts=3 target_call_id={}",
            self.target_call_id
        );
    }

    pub(crate) fn transport_retry(&self, failure: ReviewFailure) {
        trace_log!(
            PERMISSION,
            "event=auto_review_transport_retry reason={} target_call_id={}",
            failure.as_str(),
            self.target_call_id
        );
    }

    pub(crate) fn format_retry(
        &self,
        failure: ReviewFailure,
        tool_calls: usize,
        content_bytes: usize,
    ) {
        trace_log!(
            PERMISSION,
            "event=auto_review_format_retry reason={} tool_calls={tool_calls} content_bytes={content_bytes} target_call_id={}",
            failure.as_str(),
            self.target_call_id
        );
    }
}
