use ofx_contract::{ChatMessage, RecordedOutput, RecoveredTurn, ToolCallId};

use super::{RecoveryCheckpoint, RequestedModes, RouteCredential, SavedSteering, SavedToolStep};
use crate::session_codec::SavedProvider;

impl RecoveryCheckpoint {
    pub(crate) fn prompt(&self) -> &str {
        &self.user
    }

    pub(crate) fn authorizes(&self, credential: RouteCredential) -> bool {
        !self.route.may_have_sent
            || self
                .route
                .credential
                .is_some_and(|saved| saved.identifies(credential))
    }

    pub(crate) fn into_continuation(
        self,
        provider: &SavedProvider,
        model: &str,
        fast_mode: bool,
        ultrafast_mode: bool,
    ) -> RecoveredTurn {
        let unchanged = self.route.provider == *provider
            && self.route.model == model
            && self.route.requested
                == RequestedModes {
                    fast_mode,
                    ultrafast_mode,
                };
        let (messages, outputs) = messages(self.execution.tool_steps, self.execution.steering);
        RecoveredTurn {
            messages,
            outputs,
            files: self.execution.files.into_iter().map(Into::into).collect(),
            prompt: self.user,
            source: self.assistant_source,
            source_presented: false,
            cause: self.cause,
            tool_state: self.tool_state,
            strategy: self.strategy,
            fast_mode: if unchanged {
                self.route.fast_mode
            } else {
                fast_mode
            },
        }
    }
}

fn messages(
    steps: Vec<SavedToolStep>,
    steering: Vec<SavedSteering>,
) -> (Vec<ChatMessage>, Vec<RecordedOutput>) {
    let mut messages = Vec::new();
    let mut outputs = Vec::new();
    let mut steering = steering.into_iter().peekable();
    for (index, step) in steps.into_iter().enumerate() {
        while let Some(entry) = steering.next_if(|entry| entry.after_tool_step_count == index) {
            push_steering(&mut messages, entry);
        }
        let SavedToolStep {
            assistant,
            provider_replay,
            tool_calls,
            tool_results,
            ..
        } = step;
        let answered: Vec<_> = tool_calls
            .into_iter()
            .filter(|call| {
                tool_results
                    .iter()
                    .any(|result| result.tool_call_id == call.id.as_str())
            })
            .collect();
        if answered.is_empty() && provider_replay.is_none() && assistant.is_none() {
            continue;
        }
        messages.push(ChatMessage::Assistant {
            content: assistant,
            tool_calls: answered,
            provider_replay,
        });
        let mut feedback = Vec::new();
        for result in tool_results {
            let call_id = ToolCallId::new(result.tool_call_id);
            outputs.push(RecordedOutput {
                call_id: call_id.clone(),
                bytes: result.output_bytes,
                whole_file: false,
                process: result.process,
            });
            feedback.extend(
                result
                    .permission_feedback
                    .into_iter()
                    .map(|text| ChatMessage::permission_feedback(call_id.clone(), text)),
            );
            messages.push(ChatMessage::Tool {
                call_id,
                tool_name: result.tool_name,
                content: result.output,
                status: result.status,
                images: Vec::new(),
            });
        }
        messages.extend(feedback);
    }
    for entry in steering {
        push_steering(&mut messages, entry);
    }
    (messages, outputs)
}

fn push_steering(messages: &mut Vec<ChatMessage>, entry: SavedSteering) {
    if let Some(prefix) = entry.assistant_prefix.filter(|prefix| !prefix.is_empty()) {
        messages.push(ChatMessage::Assistant {
            content: Some(prefix),
            tool_calls: Vec::new(),
            provider_replay: None,
        });
    }
    messages.push(ChatMessage::restored_steering(entry.text));
}
