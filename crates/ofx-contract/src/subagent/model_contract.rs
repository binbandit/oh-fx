use serde_json::Value;
use sha2::{Digest, Sha256};

use super::domain::{
    MAX_MESSAGE_BYTES, MAX_MODEL_BYTES, MAX_PROMPT_BYTES, valid_agent_name, valid_instructions,
    valid_text,
};
use crate::types::ReasoningEffort;

const MAX_ERROR_CODE_BYTES: usize = 64;
const FINGERPRINT_DOMAIN: &[u8] = b"fx.subagent.request.v1\0";

pub const STEERING_PENDING_RESULT: &str = "The subagent is still running. Handle the user's steering now. Its result will arrive automatically; do not delegate again to poll for it.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubagentAction {
    Run,
    Message,
}

impl SubagentAction {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Message => "message",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentRequestInput<'a> {
    Run {
        task: &'a str,
        model: Option<&'a str>,
        effort: Option<&'a str>,
    },
    Message {
        agent: &'a str,
        instructions: Option<&'a str>,
        message: &'a str,
        model: Option<&'a str>,
        effort: Option<&'a str>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentRequestError {
    InvalidTask,
    InvalidAgent,
    InvalidInstructions,
    InvalidMessage,
    InvalidModel,
    InvalidEffort,
}

impl SubagentRequestError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidTask => "invalid_task",
            Self::InvalidAgent => "invalid_agent",
            Self::InvalidInstructions => "invalid_instructions",
            Self::InvalidMessage => "invalid_message",
            Self::InvalidModel => "invalid_model",
            Self::InvalidEffort => "invalid_effort",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentRequest {
    Run {
        task: String,
        model: Option<String>,
        effort: Option<ReasoningEffort>,
    },
    Message {
        agent: String,
        instructions: Option<String>,
        message: String,
        model: Option<String>,
        effort: Option<ReasoningEffort>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentOverride<'a> {
    pub model: Option<&'a str>,
    pub effort: Option<&'a ReasoningEffort>,
}

impl SubagentOverride<'_> {
    pub fn is_present(&self) -> bool {
        self.model.is_some() || self.effort.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChildKind {
    OneOff,
    Persistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChildPhase {
    Idle,
    Running,
    AwaitingApproval,
    Interrupted,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildSnapshot {
    pub kind: ChildKind,
    pub phase: ChildPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentRejectCode {
    ChildUnavailable,
    ChildBusy,
    ChildNotPersistent,
}

impl SubagentRejectCode {
    pub const fn code(self) -> &'static str {
        match self {
            Self::ChildUnavailable => "child_unavailable",
            Self::ChildBusy => "child_busy",
            Self::ChildNotPersistent => "child_not_persistent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentPlan {
    CreateOneOff,
    CreatePersistent,
    ContinuePersistent,
    SteerPersistent,
    Reject(SubagentRejectCode),
}

impl SubagentRequest {
    pub fn validate(input: SubagentRequestInput<'_>) -> Result<Self, SubagentRequestError> {
        match input {
            SubagentRequestInput::Run {
                task,
                model,
                effort,
            } => {
                if !valid_text(task, MAX_PROMPT_BYTES) {
                    return Err(SubagentRequestError::InvalidTask);
                }
                let effort = validate_overrides(model, effort)?;
                Ok(Self::Run {
                    task: task.to_owned(),
                    model: model.map(str::to_owned),
                    effort,
                })
            }
            SubagentRequestInput::Message {
                agent,
                instructions,
                message,
                model,
                effort,
            } => {
                if !valid_agent_name(agent) {
                    return Err(SubagentRequestError::InvalidAgent);
                }
                if instructions.is_some_and(|text| text.is_empty() || !valid_instructions(text)) {
                    return Err(SubagentRequestError::InvalidInstructions);
                }
                if !valid_text(message, MAX_MESSAGE_BYTES) {
                    return Err(SubagentRequestError::InvalidMessage);
                }
                let effort = validate_overrides(model, effort)?;
                Ok(Self::Message {
                    agent: agent.to_owned(),
                    instructions: instructions.map(str::to_owned),
                    message: message.to_owned(),
                    model: model.map(str::to_owned),
                    effort,
                })
            }
        }
    }

    pub fn action(&self) -> SubagentAction {
        match self {
            Self::Run { .. } => SubagentAction::Run,
            Self::Message { .. } => SubagentAction::Message,
        }
    }

    pub fn agent_name(&self) -> Option<&str> {
        match self {
            Self::Run { .. } => None,
            Self::Message { agent, .. } => Some(agent),
        }
    }

    pub fn instructions(&self) -> Option<&str> {
        match self {
            Self::Run { .. } => None,
            Self::Message { instructions, .. } => instructions.as_deref(),
        }
    }

    pub fn content(&self) -> &str {
        match self {
            Self::Run { task, .. } => task,
            Self::Message { message, .. } => message,
        }
    }

    pub fn overrides(&self) -> SubagentOverride<'_> {
        let (model, effort) = match self {
            Self::Run { model, effort, .. } | Self::Message { model, effort, .. } => {
                (model, effort)
            }
        };
        SubagentOverride {
            model: model.as_deref(),
            effort: effort.as_ref(),
        }
    }

    pub fn plan(&self, child: Option<ChildSnapshot>) -> SubagentPlan {
        let Self::Message { instructions, .. } = self else {
            return SubagentPlan::CreateOneOff;
        };
        let Some(child) = child else {
            return SubagentPlan::CreatePersistent;
        };
        match (child.kind, child.phase) {
            (ChildKind::OneOff, _) => SubagentPlan::Reject(SubagentRejectCode::ChildNotPersistent),
            (ChildKind::Persistent, ChildPhase::Idle | ChildPhase::Interrupted) => {
                SubagentPlan::ContinuePersistent
            }
            (ChildKind::Persistent, ChildPhase::Running | ChildPhase::AwaitingApproval) => {
                if instructions.is_some() {
                    SubagentPlan::Reject(SubagentRejectCode::ChildBusy)
                } else {
                    SubagentPlan::SteerPersistent
                }
            }
            (ChildKind::Persistent, ChildPhase::Finished) => {
                SubagentPlan::Reject(SubagentRejectCode::ChildUnavailable)
            }
        }
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(FINGERPRINT_DOMAIN);
        hash.update(self.action().label());
        hash.update(b"\0");
        match self {
            Self::Run { task, .. } => hash.update(task),
            Self::Message {
                agent,
                instructions,
                message,
                ..
            } => {
                hash.update(agent);
                hash.update(b"\0");
                match instructions {
                    Some(instructions) => {
                        hash.update(b"\x01");
                        hash.update(instructions);
                    }
                    None => hash.update(b"\0"),
                }
                hash.update(b"\0");
                hash.update(message);
            }
        }
        let overrides = self.overrides();
        if overrides.is_present() {
            hash.update(b"\0\x01");
            update_optional(&mut hash, overrides.model);
            hash.update(b"\0");
            update_optional(&mut hash, overrides.effort.map(ReasoningEffort::label));
        }
        hash.finalize().into()
    }
}

fn validate_overrides(
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Option<ReasoningEffort>, SubagentRequestError> {
    if model.is_some_and(|model| !valid_text(model, MAX_MODEL_BYTES)) {
        return Err(SubagentRequestError::InvalidModel);
    }
    effort
        .map(|raw| ReasoningEffort::parse(raw).ok_or(SubagentRequestError::InvalidEffort))
        .transpose()
}

fn update_optional(hash: &mut Sha256, value: Option<&str>) {
    match value {
        Some(text) => {
            hash.update(b"\x01");
            hash.update(text);
        }
        None => hash.update(b"\0"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SteeringDelivery {
    Queued,
    Applied,
    NotApplied,
}

impl SteeringDelivery {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Applied => "applied",
            Self::NotApplied => "not_applied",
        }
    }

    pub fn parse(label: &str) -> Option<Self> {
        [Self::Queued, Self::Applied, Self::NotApplied]
            .into_iter()
            .find(|delivery| delivery.label() == label)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubagentResult<'a> {
    pub ok: bool,
    pub pending: bool,
    pub result: Option<&'a str>,
    pub error_code: Option<&'a str>,
    pub delivery: Option<SteeringDelivery>,
}

impl<'a> SubagentResult<'a> {
    pub fn failure(error_code: &'a str) -> Self {
        Self {
            error_code: Some(error_code),
            ..Self::default()
        }
    }

    pub fn encode(&self) -> String {
        let mut encoded = format!("{{\"ok\":{},\"result\":", self.ok);
        push_optional_string(&mut encoded, self.result);
        encoded.push_str(",\"error_code\":");
        push_optional_string(&mut encoded, self.error_code.map(bounded_error_code));
        if self.pending {
            encoded.push_str(",\"pending\":true");
        }
        if let Some(delivery) = self.delivery {
            encoded.push_str(",\"delivery\":\"");
            encoded.push_str(delivery.label());
            encoded.push('"');
        }
        encoded.push('}');
        encoded
    }
}

pub fn feedback_result(delivery: SteeringDelivery) -> SubagentResult<'static> {
    SubagentResult {
        ok: delivery != SteeringDelivery::NotApplied,
        pending: false,
        result: Some(match delivery {
            SteeringDelivery::Queued => {
                "Feedback queued for the running child. Its result will arrive automatically."
            }
            SteeringDelivery::Applied => {
                "Feedback consumed at the child's safe boundary. This is not a task-completion result."
            }
            SteeringDelivery::NotApplied => "Feedback was not applied before the child stopped.",
        }),
        error_code: (delivery == SteeringDelivery::NotApplied).then_some("feedback_not_applied"),
        delivery: Some(delivery),
    }
}

fn bounded_error_code(code: &str) -> &str {
    let mut end = code.len().min(MAX_ERROR_CODE_BYTES);
    while !code.is_char_boundary(end) {
        end -= 1;
    }
    &code[..end]
}

fn push_optional_string(encoded: &mut String, value: Option<&str>) {
    match value {
        Some(text) => encoded.push_str(&Value::from(text).to_string()),
        None => encoded.push_str("null"),
    }
}

#[cfg(test)]
mod tests;
