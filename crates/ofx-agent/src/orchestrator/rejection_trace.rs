use ofx_contract::{CONTEXT_DEFERRED_TOOL_OUTPUT, ToolArgumentIntegrity, ToolCall};
use ofx_trace::{TraceContext, trace_event};

use crate::lifecycle::HookBlock;

const TOOL: &str = "tool";
const PANICKED: &str = "Panicked";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refused {
    Malformed(ToolArgumentIntegrity),
    Hook(HookBlock),
    Validation,
    Availability,
    Panicked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Denial {
    User,
    ReviewCaution,
    ReviewEvidenceIncomplete,
    ReviewUnavailable,
}

impl Denial {
    const fn name(self) -> &'static str {
        match self {
            Self::User => "user_denied",
            Self::ReviewCaution => "review_caution",
            Self::ReviewEvidenceIncomplete => "review_evidence_incomplete",
            Self::ReviewUnavailable => "review_unavailable",
        }
    }
}

const fn integrity_name(integrity: ToolArgumentIntegrity) -> &'static str {
    match integrity {
        ToolArgumentIntegrity::Valid => "valid",
        ToolArgumentIntegrity::MalformedJson => "malformed_json",
        ToolArgumentIntegrity::NonObjectJson => "non_object_json",
    }
}

pub(super) fn rejected(context: TraceContext, call: &ToolCall, refused: Refused, bytes: usize) {
    let (id, name) = (call.id.as_str(), &call.name);
    match refused {
        Refused::Malformed(integrity) => {
            let integrity = integrity_name(integrity);
            trace_event!(
                TOOL,
                "argument_integrity_rejected",
                context,
                "call_id={id} name={name} failure={integrity} provenance=fx_local"
            );
            trace_event!(
                TOOL,
                "execution_result",
                context,
                "call_id={id} name={name} result_kind=malformed_arguments model_output_bytes={bytes} argument_integrity={integrity}"
            );
        }
        Refused::Hook(block) => {
            let kind = block.name();
            trace_event!(
                TOOL,
                "pre_tool_use_blocked",
                context,
                "call_id={id} name={name} kind={kind}"
            );
            trace_event!(
                TOOL,
                "execution_result",
                context,
                "call_id={id} name={name} result_kind={kind} model_output_bytes={bytes} argument_integrity=valid"
            );
        }
        Refused::Validation => result(context, call, "validation_failure", bytes),
        Refused::Availability => result(context, call, "availability_failure", bytes),
        Refused::Panicked => trace_event!(
            TOOL,
            "execution_result",
            context,
            "call_id={id} name={name} result_kind=validation_failure err={PANICKED} model_output_bytes={bytes}"
        ),
    }
}

pub(super) fn denied(context: TraceContext, call: &ToolCall, denial: Denial, bytes: usize) {
    trace_event!(
        TOOL,
        "execution_result",
        context,
        "call_id={} name={} result_kind=permission_denied reason={} model_output_bytes={bytes}",
        call.id.as_str(),
        call.name,
        denial.name()
    );
}

pub(super) fn not_executed(context: TraceContext, call: &ToolCall, output: &str) {
    result(context, call, deferral_kind(output), output.len());
}

fn deferral_kind(output: &str) -> &'static str {
    if output == CONTEXT_DEFERRED_TOOL_OUTPUT {
        "context_deferred"
    } else {
        "applicable_targets_changed"
    }
}

fn result(context: TraceContext, call: &ToolCall, kind: &str, bytes: usize) {
    trace_event!(
        TOOL,
        "execution_result",
        context,
        "call_id={} name={} result_kind={kind} model_output_bytes={bytes}",
        call.id.as_str(),
        call.name
    );
}

#[cfg(test)]
mod tests;
