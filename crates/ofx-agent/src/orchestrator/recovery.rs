use std::borrow::Cow;
use std::mem;

use ofx_contract::{
    ChatMessage, ModelRecoveryCause, ProviderError, RecoveryStrategy, ToolChoice, TurnId, UiEvent,
};

use super::{EventSink, Turn};
use crate::assistant_stream::LanguageStage;
use crate::model_response_recovery::{Output, Strategy, ToolEvidence, failed_in_stream};

const RESPONSE_RESTARTED: &str = "\n\n[Response interrupted. Restarting.]\n\n";

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
    messages: Option<Cow<'a, [ChatMessage]>>,
}

#[derive(Debug, Default)]
pub(super) struct RestoredReply {
    pub(super) source: String,
    pub(super) presented: bool,
}

impl<'a> Restart<'a> {
    pub(super) fn begin(turn: &mut Turn, events: EventSink<'_>) -> Self {
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
            messages: None,
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
        cause == ModelRecoveryCause::ProviderUnavailable
            && self.source(stage).is_empty()
            && observed == ToolEvidence::None
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

    pub(super) fn messages<'b>(&'b self, sent: &'b [ChatMessage]) -> &'b [ChatMessage] {
        self.messages.as_deref().unwrap_or(sent)
    }

    pub(super) fn resend(&mut self, messages: Cow<'a, [ChatMessage]>) {
        self.messages = Some(messages);
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
