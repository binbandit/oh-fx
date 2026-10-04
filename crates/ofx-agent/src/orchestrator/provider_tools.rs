use std::mem;

use ofx_contract::{
    ChatMessage, Completion, DEFAULT_MAX_TOOL_RESULT_BYTES, FinishReason, ProviderReplay,
    ToolArgumentIntegrity, ToolCall, ToolExecutionProvenance, ToolOutput, ToolResultStatus,
    is_provider_search_alias, is_tool_output_error, prepare_model_output,
    provider_search_description,
};

use super::{
    Agent, EventSink, Stop, Turn, TurnFailure, escalate_repeated_failure, tool_finished,
    tool_started,
};
use crate::execution_memory::partial_view;

pub(super) fn provider_executed(call: &ToolCall) -> bool {
    call.provenance == ToolExecutionProvenance::ProviderExecuted
}

pub(super) fn joins_parallel_groups(call: &ToolCall) -> bool {
    call.provider_result.is_none()
}

pub(super) fn malformed_provider_calls(calls: &[ToolCall]) -> Option<TurnFailure> {
    let provider_calls = || calls.iter().filter(|call| provider_executed(call));
    if provider_calls().any(|call| {
        ToolArgumentIntegrity::classify_function_input(&call.arguments)
            == ToolArgumentIntegrity::MalformedJson
    }) {
        return Some(TurnFailure::MalformedProviderArguments);
    }
    provider_calls()
        .any(|call| call.provider_result.is_none())
        .then_some(TurnFailure::MalformedProviderResult)
}

pub(super) fn ends_with_provider_results(completion: &Completion) -> bool {
    completion.finish_reason == FinishReason::Stop
        && completion
            .content
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        && !completion.tool_calls.is_empty()
        && completion.tool_calls.iter().all(provider_executed)
}

impl Agent {
    pub(super) fn publish_provider_result(
        &mut self,
        turn: &mut Turn,
        call: &ToolCall,
        events: EventSink<'_>,
    ) {
        let shown = is_provider_search_alias(&call.name);
        if shown {
            events(tool_started(
                turn.id,
                call,
                provider_search_description(&call.arguments),
            ));
        }
        let result = call.provider_result.clone().unwrap_or_default();
        let status = if is_tool_output_error(&result) {
            ToolResultStatus::Failure
        } else {
            ToolResultStatus::Success
        };
        turn.raw_outputs
            .push(partial_view(call.id.clone(), result.len()));
        let model_output = prepare_model_output(&call.name, result, DEFAULT_MAX_TOOL_RESULT_BYTES);
        if shown {
            let output = ToolOutput {
                status,
                ..ToolOutput::success(model_output.clone())
            };
            events(tool_finished(turn.id, call, Some(&output)));
        }
        let content = if ToolArgumentIntegrity::classify_function_input(&call.arguments)
            == ToolArgumentIntegrity::Valid
        {
            escalate_repeated_failure(turn, call, status, model_output)
        } else {
            model_output
        };
        self.history.push(ChatMessage::Tool {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content,
            status,
        });
    }

    pub(super) fn finish_with_provider_results(
        &mut self,
        turn: &mut Turn,
        mut completion: Completion,
        more_steps: bool,
        events: EventSink<'_>,
    ) -> Result<Option<String>, Stop> {
        let calls = mem::take(&mut completion.tool_calls);
        let (step_replay, final_replay) = match completion.provider_replay.take() {
            Some(replay) => (
                self.projected_replay(&replay, false, true)?,
                self.projected_replay(&replay, true, false)?,
            ),
            None => (None, None),
        };
        self.history.push(ChatMessage::Assistant {
            content: None,
            tool_calls: calls.clone(),
            provider_replay: step_replay,
        });
        for call in &calls {
            self.publish_provider_result(turn, call, events);
        }
        completion.provider_replay = final_replay;
        self.finish(turn, completion, more_steps, events)
    }

    fn projected_replay(
        &self,
        replay: &ProviderReplay,
        text: bool,
        reasoning: bool,
    ) -> Result<Option<ProviderReplay>, Stop> {
        self.provider
            .project_replay(replay, text, reasoning)
            .map_err(|error| Stop::failed(TurnFailure::Provider(error)))
    }
}
