use std::borrow::Cow;
use std::mem;

use ofx_contract::{
    ChatMessage, ModelRecoveryCause, ProviderError, RecoveryStrategy, ToolChoice, TurnId, UiEvent,
};

use super::{Agent, EventSink, Stop, Turn, TurnFailure};
use crate::assistant_stream::LanguageStage;
use crate::model_response_recovery::{
    DEFAULT_MAX_PROVIDER_ATTEMPTS, Output, Strategy, ToolEvidence, failed_in_stream,
};

const RESPONSE_RESTARTED: &str = "\n\n[Response interrupted. Restarting.]\n\n";
const PREFILL_CONTINUATION: &str = "Continue from the preceding tool result.";
const PREFILL_UNSUPPORTED: &str = "does not support assistant message prefill";
const USER_TAIL_REQUIRED: &str = "must end with a user message";
const BAD_REQUEST: u16 = 400;

const CONTINUE_RESPONSE: &str = "The previous response was interrupted. Restart that response from the beginning using the completed tool results above. Do not repeat completed tool actions.";
const REGENERATE_TOOL: &str = "The previous response ended during an incomplete tool call. fx did not execute that call. Recreate it only if it is still needed.";
const CONTINUE_AFTER_TOOL: &str =
    "Continue from the confirmed tool result above without repeating the tool.";
const RECONCILE_TOOL: &str = "Reconcile the available tool evidence above before continuing. Do not repeat the tool unless the evidence proves it is safe.";

pub(super) fn recovery_note(strategy: RecoveryStrategy) -> Option<&'static str> {
    match strategy {
        RecoveryStrategy::RetryRequest => None,
        RecoveryStrategy::ContinueResponse => Some(CONTINUE_RESPONSE),
        RecoveryStrategy::RegenerateTool => Some(REGENERATE_TOOL),
        RecoveryStrategy::ContinueAfterTool => Some(CONTINUE_AFTER_TOOL),
        RecoveryStrategy::ReconcileTool => Some(RECONCILE_TOOL),
    }
}

pub(super) fn retried_strategy(
    current: Option<RecoveryStrategy>,
    decided: Strategy,
) -> Option<RecoveryStrategy> {
    let strategy = match decided {
        Strategy::ContinueResponse => RecoveryStrategy::ContinueResponse,
        Strategy::RegenerateTool => RecoveryStrategy::RegenerateTool,
        Strategy::ContinueAfterTool => RecoveryStrategy::ContinueAfterTool,
        Strategy::ReconcileTool => RecoveryStrategy::ReconcileTool,
        Strategy::RetryRequest
        | Strategy::WaitForConnectivity
        | Strategy::ProbeLiveness
        | Strategy::Stop => RecoveryStrategy::RetryRequest,
    };
    (current.is_some() || strategy != RecoveryStrategy::RetryRequest).then_some(strategy)
}

pub(super) fn recovery_tool_choice(recovery: Option<RecoveryStrategy>) -> ToolChoice {
    if recovery == Some(RecoveryStrategy::ReconcileTool) {
        ToolChoice::None
    } else {
        ToolChoice::Auto
    }
}

#[derive(Debug)]
pub(super) struct Restart<'a> {
    interrupted: String,
    latest: String,
    messages: Cow<'a, [ChatMessage]>,
}

#[derive(Debug, Default)]
pub(super) struct RestoredReply {
    pub(super) source: String,
    pub(super) presented: bool,
}

impl<'a> Restart<'a> {
    pub(super) fn begin(turn: &mut Turn, sent: &'a [ChatMessage], events: EventSink<'_>) -> Self {
        let RestoredReply {
            mut source,
            presented,
        } = mem::take(&mut turn.restored);
        if !source.is_empty() {
            if !presented && let Some(text) = turn.language.stage.admit(source.clone()) {
                events(UiEvent::AssistantText {
                    turn_id: turn.id,
                    text,
                });
            }
            if !presented && turn.language.stage.holds_candidate() {
                source.clear();
            } else {
                events(restarted(turn.id));
            }
            turn.language.stage.restart();
        }
        Self {
            interrupted: source,
            latest: String::new(),
            messages: Cow::Borrowed(sent),
        }
    }

    pub(super) fn observe(
        &mut self,
        partial: String,
        observed: ToolEvidence,
        evidence: &mut ToolEvidence,
    ) -> bool {
        *evidence = evidence.observed(observed);
        self.latest = partial;
        !self.latest.is_empty()
    }

    pub(super) fn evidence(
        &self,
        held: &mut ToolEvidence,
        (observed, cause, error): (ToolEvidence, ModelRecoveryCause, &ProviderError),
        stage: &LanguageStage,
    ) -> (Output, ToolEvidence) {
        if observed == ToolEvidence::ProvenUnexecuted && failed_in_stream(cause, error) {
            *held = ToolEvidence::Uncertain;
        }
        (self.output(stage), *held)
    }

    pub(super) fn replay_safe(
        &self,
        cause: ModelRecoveryCause,
        observed: ToolEvidence,
        stage: &LanguageStage,
    ) -> bool {
        cause == ModelRecoveryCause::ProviderUnavailable && self.unsent(observed, stage)
    }

    fn unsent(&self, observed: ToolEvidence, stage: &LanguageStage) -> bool {
        self.source(stage).is_empty() && observed == ToolEvidence::None
    }

    fn rejected_prefill(
        &self,
        error: &ProviderError,
        observed: ToolEvidence,
        stage: &LanguageStage,
    ) -> bool {
        rejects_prefill(error)
            && ends_with_tool_result(&self.messages)
            && self.unsent(observed, stage)
    }

    fn output(&self, stage: &LanguageStage) -> Output {
        if self.source(stage).is_empty() {
            Output::None
        } else {
            Output::Partial
        }
    }

    pub(super) fn source(&self, stage: &LanguageStage) -> &str {
        match stage.interruption_source(&self.latest) {
            "" => &self.interrupted,
            checked => checked,
        }
    }

    pub(super) fn partial(&self) -> &str {
        if self.latest.is_empty() {
            &self.interrupted
        } else {
            &self.latest
        }
    }

    pub(super) fn restarted(&mut self, stage: &LanguageStage) -> bool {
        if self.latest.is_empty() || stage.holds_candidate() {
            self.latest.clear();
            return false;
        }
        self.interrupted = mem::take(&mut self.latest);
        true
    }

    pub(super) fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub(super) fn resend(&mut self, messages: Cow<'a, [ChatMessage]>) {
        self.messages = messages;
    }

    pub(super) fn failed(self, error: ProviderError) -> Stop {
        Stop::Failed {
            failure: TurnFailure::Provider(error),
            partial: self.into_partial(),
        }
    }

    pub(super) fn into_partial(self) -> String {
        if self.latest.is_empty() {
            self.interrupted
        } else {
            self.latest
        }
    }
}

pub(super) fn restarted(turn_id: TurnId) -> UiEvent {
    UiEvent::AssistantRestarted {
        turn_id,
        text: RESPONSE_RESTARTED.to_owned(),
    }
}

fn ends_with_tool_result(sent: &[ChatMessage]) -> bool {
    matches!(sent.last(), Some(ChatMessage::Tool { .. }))
}

fn rejects_prefill(error: &ProviderError) -> bool {
    error.status == Some(BAD_REQUEST)
        && error.detail.as_deref().is_some_and(|detail| {
            detail.contains(PREFILL_UNSUPPORTED) && detail.contains(USER_TAIL_REQUIRED)
        })
}

impl Agent {
    pub(super) fn asks_again<'a>(
        &'a self,
        turn: &mut Turn,
        (error, observed, attempt): (&ProviderError, ToolEvidence, usize),
        restart: &mut Restart<'a>,
    ) -> bool {
        let rejected = turn.continuation.is_none()
            && attempt < DEFAULT_MAX_PROVIDER_ATTEMPTS
            && restart.rejected_prefill(error, observed, &turn.language.stage);
        if rejected {
            turn.continuation = Some(PREFILL_CONTINUATION);
            restart.resend(self.request_messages(turn));
        }
        rejected
    }
}

#[cfg(test)]
mod tests {
    use ofx_contract::{ProviderErrorKind, ToolCallId, ToolResultStatus};

    use super::*;

    #[test]
    fn assistant_prefill_rejection_recovers_after_any_tool_result_tail() {
        let rejected = |status, detail: &str| {
            let mut error = ProviderError::new(ProviderErrorKind::InvalidRequest, "BadRequest");
            error.status = Some(status);
            error.detail = Some(detail.to_owned());
            error
        };
        let detail = "AI_APICallError: This model does not support assistant message prefill. The conversation must end with a user message.";
        let after_tool = [
            ChatMessage::user("go"),
            ChatMessage::Tool {
                call_id: ToolCallId::new("call"),
                tool_name: "subagent".to_owned(),
                content: "failed".to_owned(),
                status: ToolResultStatus::Failure,
            },
        ];
        assert!(rejects_prefill(&rejected(400, detail)));
        assert!(ends_with_tool_result(&after_tool));
        assert!(!rejects_prefill(&rejected(400, "other failure")));
        assert!(!rejects_prefill(&rejected(429, detail)));
        assert!(!ends_with_tool_result(&[ChatMessage::user("go")]));
    }
}
