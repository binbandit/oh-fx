use std::borrow::Cow;
use std::mem;

use ofx_contract::{ChatMessage, RecoveryStrategy, ToolChoice, TurnId, UiEvent};

use crate::assistant_stream::LanguageStage;
use crate::model_response_recovery::{Output, Strategy};

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
    match (current, decided) {
        (
            Some(
                RecoveryStrategy::RegenerateTool
                | RecoveryStrategy::ContinueAfterTool
                | RecoveryStrategy::ReconcileTool,
            ),
            _,
        ) => current,
        (_, Strategy::ContinueResponse) => Some(RecoveryStrategy::ContinueResponse),
        (Some(RecoveryStrategy::ContinueResponse), _) => Some(RecoveryStrategy::RetryRequest),
        _ => current,
    }
}

pub(super) fn recovery_tool_choice(recovery: Option<RecoveryStrategy>) -> ToolChoice {
    if recovery == Some(RecoveryStrategy::ReconcileTool) {
        ToolChoice::None
    } else {
        ToolChoice::Auto
    }
}

#[derive(Debug, Default)]
pub(super) struct Restart<'a> {
    interrupted: String,
    latest: String,
    messages: Option<Cow<'a, [ChatMessage]>>,
}

impl<'a> Restart<'a> {
    pub(super) fn observe(&mut self, partial: String) -> bool {
        self.latest = partial;
        !self.latest.is_empty()
    }

    pub(super) fn output(&self, stage: &LanguageStage) -> Output {
        if self.source(stage).is_empty() {
            Output::None
        } else {
            Output::Partial
        }
    }

    fn source(&self, stage: &LanguageStage) -> &str {
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
