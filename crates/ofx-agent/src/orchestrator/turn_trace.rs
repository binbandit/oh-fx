use ofx_contract::{
    Completion, FinishReason, ProviderError, ToolCall, ToolCallId, ToolOutput, ToolResultStatus,
};
use ofx_trace::{TraceContext, keyless_json_preview, trace_event, trace_log};

use super::{Stop, TurnFailure};

const AGENT: &str = "agent";
const TOOL: &str = "tool";
const INTERRUPT: &str = "interrupt";
const GATEWAY: &str = "gateway";
const NONE: &str = "none";
const OUTPUT_TRUNCATED: &str = "OutputTruncated";
const CONTENT_FILTERED: &str = "ContentFiltered";
const INCOMPLETE_STREAM: &str = "IncompleteStream";
const PROVIDER_FINISH_ERROR: &str = "ProviderError";
const PANICKED: &str = "Panicked";

#[derive(Debug, Default)]
pub(super) struct ToolTrail {
    pub(super) completed: Vec<String>,
    pub(super) last_call: Option<(ToolCallId, String)>,
    pub(super) active: Option<(ToolCallId, String)>,
    pub(super) gateway_messages: usize,
    pub(super) cancelled_in_tools: bool,
    pub(super) finish: Option<&'static str>,
}

struct Interrupted<'a> {
    prompt: &'a str,
    partial: &'a str,
    trail: &'a ToolTrail,
}

impl ToolTrail {
    fn completed_names(&self) -> String {
        if self.completed.is_empty() {
            NONE.to_owned()
        } else {
            self.completed.join(",")
        }
    }
}

pub(super) fn turn_context(context: TraceContext) -> TraceContext {
    TraceContext {
        step_id: 0,
        ..context
    }
}

pub(super) fn prompt_start(context: TraceContext, prompt: &str, model: &str) {
    trace_event!(
        AGENT,
        "prompt_start",
        turn_context(context),
        "prompt_bytes={} model={model}",
        prompt.len()
    );
}

pub(super) fn prompt_finish(context: TraceContext, outcome: &str) {
    trace_event!(
        AGENT,
        "prompt_finish",
        turn_context(context),
        "outcome_kind={outcome}"
    );
}

pub(super) fn step_begin(context: TraceContext, step_index: u64, step_limit: u64, messages: usize) {
    trace_log!(
        AGENT,
        "step start step={step_index} limit={step_limit} messages={messages}"
    );
    trace_event!(
        AGENT,
        "step_begin",
        context,
        "step_index={step_index} step_limit={step_limit} gateway_messages={messages}"
    );
}

pub(super) fn before_provider_preflight(context: TraceContext, model: &str, messages: usize) {
    trace_event!(
        AGENT,
        "before_provider_preflight",
        context,
        "model={model} messages={messages}"
    );
}

pub(super) fn provider_admitted(context: TraceContext, model: &str) {
    trace_event!(AGENT, "provider_admitted", context, "model={model}");
}

pub(super) struct ProviderOptionsTrace<'a> {
    pub(super) model: &'a str,
    pub(super) fast_mode: bool,
    pub(super) effort: Option<&'a str>,
    pub(super) reasoning_selected: bool,
    pub(super) fast_selected: bool,
}

pub(super) fn provider_options(context: TraceContext, options: &ProviderOptionsTrace<'_>) {
    let reasoning = if options.reasoning_selected {
        "selected"
    } else if options.effort.is_none() {
        "default"
    } else {
        "unsupported_or_missing"
    };
    let fast = if options.fast_selected {
        "selected"
    } else if !options.fast_mode {
        "default"
    } else {
        "unsupported_or_missing"
    };
    trace_event!(
        GATEWAY,
        "provider_options",
        context,
        "model={} fast_mode={} effort={} reasoning={reasoning} fast={fast}",
        options.model,
        options.fast_mode,
        options.effort.unwrap_or("auto"),
    );
}

pub(super) fn step_completion(context: TraceContext, step_index: u64, completion: &Completion) {
    let content_bytes = completion.content.as_deref().map_or(0, str::len);
    let calls = completion.tool_calls.len();
    let finish_reason = finish_reason_label(completion.finish_reason);
    trace_log!(
        AGENT,
        "step completion step={step_index} content_bytes={content_bytes} tool_calls={calls} finish_reason={finish_reason}"
    );
    trace_event!(
        AGENT,
        "assistant_completion",
        context,
        "content_bytes={content_bytes} tool_call_count={calls} finish_reason={finish_reason}"
    );
    if ofx_trace::enabled(TOOL) {
        for call in &completion.tool_calls {
            ofx_trace::event(
                TOOL,
                "returned_tool_call",
                context,
                Some(format_args!(
                    "call_id={} tool_name={} args_bytes={} args_preview={}",
                    call.id.as_str(),
                    call.name,
                    call.arguments.len(),
                    keyless_json_preview(&call.arguments),
                )),
            );
        }
    }
}

pub(super) fn invalid_tool_finish(context: TraceContext, completion: &Completion) {
    let finish_reason = finish_reason_label(completion.finish_reason);
    if completion.tool_calls.is_empty() {
        trace_event!(
            AGENT,
            "provider_tool_finish_missing_calls",
            context,
            "finish_reason={finish_reason}"
        );
    } else {
        trace_event!(
            AGENT,
            "provider_tool_calls_rejected",
            context,
            "finish_reason={finish_reason} tool_call_count={}",
            completion.tool_calls.len()
        );
    }
}

pub(super) fn malformed_provider_call(context: TraceContext, failure: &TurnFailure) {
    match failure {
        TurnFailure::MalformedProviderArguments => trace_event!(
            AGENT,
            "provider_tool_arguments_rejected",
            context,
            "failure=malformed_json provenance=provider_executed"
        ),
        _ => trace_event!(
            AGENT,
            "authoritative_tool_admission_rejected",
            context,
            "failure=missing_result provenance=provider_executed"
        ),
    }
}

pub(super) fn continuation_injected(silent_steps: u32, content: Option<&str>, replay: bool) {
    trace_log!(
        AGENT,
        "injecting continuation after {silent_steps} silent tool steps omitted_assistant_bytes={} preserved_provider_state={replay}",
        content.map_or(0, str::len)
    );
}

pub(super) fn step_limit_reached(
    context: TraceContext,
    step_index: u64,
    step_limit: u64,
    trail: &ToolTrail,
) {
    if !ofx_trace::enabled(AGENT) {
        return;
    }
    let names = trail.completed_names();
    let (last_id, last_name) = trail
        .last_call
        .as_ref()
        .map_or((NONE, NONE), |(id, name)| (id.as_str(), name.as_str()));
    let messages = trail.gateway_messages;
    let completed = trail.completed.len();
    trace_log!(
        AGENT,
        "step limit reached turn_id={} step_id={} step_index={step_index} step_limit={step_limit} gateway_messages={messages} completed_tool_count={completed} completed_tool_names={names} last_tool_call_name={last_name} last_tool_call_id={last_id} outcome_kind=step_limit",
        context.turn_id,
        context.step_id
    );
    trace_event!(
        AGENT,
        "step_limit_reached",
        context,
        "step_index={step_index} step_limit={step_limit} gateway_messages={messages} completed_tool_count={completed} completed_tool_names={names} last_tool_call_name={last_name} last_tool_call_id={last_id} outcome_kind=step_limit"
    );
}

pub(super) fn repeated_tool_failure(context: TraceContext, failure: &TurnFailure, calls: usize) {
    let name = match failure {
        TurnFailure::RepeatedMalformedArguments => "repeated_malformed_tool_arguments",
        _ => "repeated_shell_execution_failure",
    };
    trace_event!(AGENT, name, context, "tool_call_count={calls}");
}

pub(super) fn uncertain_provider_tool_rejected(context: TraceContext, calls: usize) {
    trace_event!(
        AGENT,
        "uncertain_provider_tool_rejected",
        context,
        "tool_call_count={calls} tool_choice=none"
    );
}

fn cancel_observed(context: TraceContext, active_tool_known: bool) {
    trace_event!(
        INTERRUPT,
        "cancel_observed",
        context,
        "active_tool_known={active_tool_known}"
    );
}

fn interrupted_persisted(context: TraceContext, interrupted: &Interrupted<'_>) {
    if !ofx_trace::enabled(AGENT) && !ofx_trace::enabled(INTERRUPT) {
        return;
    }
    let trail = interrupted.trail;
    let names = trail.completed_names();
    let prompt_bytes = interrupted.prompt.len();
    let partial_bytes = interrupted.partial.len();
    let active = trail.active.is_some();
    let completed = trail.completed.len();
    trace_log!(
        AGENT,
        "interrupted marker persisted prompt_bytes={prompt_bytes} partial_assistant_bytes={partial_bytes} active_tool={active} completed_tool_count={completed} completed_tool_names={names}"
    );
    trace_event!(
        INTERRUPT,
        "interrupted_history_persisted",
        context,
        "prompt_bytes={prompt_bytes} partial_assistant_bytes={partial_bytes} active_tool_known={active} completed_tool_count={completed} completed_tool_names={names}"
    );
    trace_event!(
        INTERRUPT,
        "interrupt_persisted",
        context,
        "prompt_bytes={prompt_bytes} partial_assistant_bytes={partial_bytes} active_tool={active} completed_tool_count={completed} completed_tool_names={names} active_tool_reason={}",
        interrupt_reason(interrupted)
    );
    if let Some((id, name)) = &trail.active {
        trace_log!(
            AGENT,
            "aborted tool output persisted call_id={} name={name}",
            id.as_str()
        );
        trace_event!(
            INTERRUPT,
            "aborted_tool_persisted",
            context,
            "call_id={} name={name}",
            id.as_str()
        );
    }
    trace_event!(
        INTERRUPT,
        "finish_event_emitted",
        context,
        "outcome_kind=interrupted"
    );
}

pub(super) fn interrupted(context: TraceContext, prompt: &str, partial: &str, trail: &ToolTrail) {
    if trail.finish.is_some() {
        return;
    }
    cancel_observed(context, trail.cancelled_in_tools);
    interrupted_persisted(
        context,
        &Interrupted {
            prompt,
            partial,
            trail,
        },
    );
}

pub(super) fn stream_failure_persisted(context: TraceContext, prompt: &str, partial: &str) {
    trace_event!(
        GATEWAY,
        "stream_failure_history_persisted",
        context,
        "prompt_bytes={} partial_assistant_bytes={}",
        prompt.len(),
        partial.len()
    );
    trace_event!(
        GATEWAY,
        "stream_failure_finish_event_emitted",
        context,
        "outcome_kind=failed"
    );
}

pub(super) fn outcome_kind(
    result: &Result<String, Stop>,
    trail: &ToolTrail,
    cancelled: bool,
) -> &'static str {
    if let Some(kind) = trail.finish {
        return kind;
    }
    match result {
        Ok(_) => "assistant",
        Err(Stop::Interrupted { .. }) => "interrupted",
        Err(Stop::Paused { .. }) => "recovery_paused",
        Err(Stop::Failed { failure, .. }) => failure_kind(failure, cancelled),
    }
}

fn failure_kind(failure: &TurnFailure, cancelled: bool) -> &'static str {
    match failure {
        TurnFailure::StepLimitReached => "step_limit",
        TurnFailure::RepeatedMalformedArguments => "repeated_malformed_tool_arguments",
        TurnFailure::RepeatedShellExecutionFailure => "repeated_shell_execution_failure",
        TurnFailure::ResponseLanguageMismatch => "response_language_mismatch",
        TurnFailure::InvalidCompletion => "invalid_tool_finish",
        TurnFailure::MalformedProviderResult => "malformed_provider_result",
        TurnFailure::MalformedProviderArguments => "malformed_provider_tool_arguments",
        TurnFailure::Provider(error) => provider_kind(error),
        _ if cancelled => "cancelled",
        _ => "http_error",
    }
}

fn provider_kind(error: &ProviderError) -> &'static str {
    match error.code.as_str() {
        OUTPUT_TRUNCATED => "provider_length",
        CONTENT_FILTERED => "content_filter",
        PROVIDER_FINISH_ERROR if error.status.is_none() => "provider_error",
        INCOMPLETE_STREAM => "stream_interrupted",
        _ => "http_error",
    }
}

fn interrupt_reason(interrupted: &Interrupted<'_>) -> &'static str {
    if interrupted.trail.active.is_some() {
        "active_tool_call_present"
    } else if !interrupted.trail.completed.is_empty() {
        "completed_tools_present"
    } else if !interrupted.partial.is_empty() {
        "partial_assistant_only"
    } else {
        "no_assistant_output"
    }
}

pub(super) fn tool_call(context: TraceContext, call: &ToolCall) {
    trace_event!(
        TOOL,
        "tool_call",
        context,
        "call_id={} name={}",
        call.id.as_str(),
        call.name
    );
}

pub(super) fn tool_execution_start(context: TraceContext, call: &ToolCall) {
    for name in ["before_tool_execution", "execution_start"] {
        trace_event!(
            TOOL,
            name,
            context,
            "call_id={} name={}",
            call.id.as_str(),
            call.name
        );
    }
}

pub(super) fn result_kind(output: &ToolOutput) -> &'static str {
    if output.status == ToolResultStatus::Failure {
        "error"
    } else if output.file_change.is_some() {
        "diff"
    } else {
        "model_output"
    }
}

pub(super) fn tool_execution_result(
    context: TraceContext,
    call: &ToolCall,
    kind: &str,
    panicked: bool,
    model_output_bytes: usize,
) {
    for name in ["after_tool_execution", "execution_result"] {
        if panicked {
            trace_event!(
                TOOL,
                name,
                context,
                "call_id={} name={} result_kind={kind} err={PANICKED} model_output_bytes={model_output_bytes}",
                call.id.as_str(),
                call.name
            );
        } else {
            trace_event!(
                TOOL,
                name,
                context,
                "call_id={} name={} result_kind={kind} model_output_bytes={model_output_bytes}",
                call.id.as_str(),
                call.name
            );
        }
    }
}

pub(super) fn provider_result(context: TraceContext, call: &ToolCall, model_output_bytes: usize) {
    trace_event!(
        TOOL,
        "execution_result",
        context,
        "call_id={} name={} result_kind=provider_executed model_output_bytes={model_output_bytes}",
        call.id.as_str(),
        call.name
    );
}

pub(super) fn parallel_group(context: TraceContext, name: &str, kind: &str, count: usize) {
    trace_event!(TOOL, name, context, "kind={kind} count={count}");
}

const fn finish_reason_label(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::ToolCalls => "tool-calls",
    }
}

#[cfg(test)]
mod tests;
