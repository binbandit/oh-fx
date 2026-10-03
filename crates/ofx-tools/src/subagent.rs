use std::sync::Arc;

use ofx_contract::{
    BoxFuture, CallDescription, Concurrency, PreparedCall, SubagentActionState, SubagentProvider,
    SubagentRequest, SubagentRequestError, SubagentRequestInput, SubagentResult, Tool,
    ToolActivity, ToolArgsError, ToolContext, ToolEffect, ToolOutput, ToolSpec,
    format_subagent_plain_action, parse_tool_args_object,
};
use serde_json::{Map, Value};

const TOOL_NAME: &str = "subagent";
const DESCRIPTION: &str = "Delegate work and receive one terminal child result. Use run for one temporary child and one task. Use message with a stable name to create or continue a persistent conversation in this parent session. A plain message to a working child queues feedback for its next safe boundary without cancelling its current tool. A delivery receipt is not the child's final result; that result arrives separately. Optional instructions replace only that child's system overlay between turns; fx preserves its trusted base prompt. Optional model and effort apply only when a child is created and are rejected for an existing child. fx owns timing, worker identities, cancellation, permissions, persistence, and cleanup.";
const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"request":{"oneOf":[{"type":"object","properties":{"action":{"type":"string","enum":["run"]},"task":{"type":"string","minLength":1,"maxLength":65536,"description":"One complete task for a temporary child. The child accepts no follow-up."},"model":{"type":"string","minLength":1,"maxLength":256,"description":"Optional model for this child, as a catalog model ID such as openai/gpt-5.6-terra. Unambiguous partial names resolve to catalog IDs; unknown or ambiguous names are rejected with candidate IDs. Inherits the parent's model when omitted."},"effort":{"type":"string","minLength":1,"maxLength":64,"description":"Optional reasoning effort for this child. Inherits the parent's effort when omitted."}},"additionalProperties":false,"required":["action","task"]},{"type":"object","properties":{"action":{"type":"string","enum":["message"]},"agent":{"type":"string","minLength":1,"maxLength":64,"description":"Stable lowercase name for one persistent conversation in this parent session. A new valid name creates it; later calls continue it."},"instructions":{"type":"string","minLength":1,"maxLength":65536,"description":"Optional persistent instructions for this child. Replaces its child-specific system overlay before this message when idle; rejected while the child is working. Omit to preserve the overlay or send live feedback. Cannot replace fx's trusted base prompt or widen authority."},"message":{"type":"string","minLength":1,"maxLength":65536,"description":"Message for that named agent: creates it on first use, continues an idle conversation, or queues feedback for a working child. Do not resend merely to poll for completion."},"model":{"type":"string","minLength":1,"maxLength":256,"description":"Optional model applied when this message creates the child, as a catalog model ID such as openai/gpt-5.6-terra. Unambiguous partial names resolve to catalog IDs; unknown or ambiguous names are rejected with candidate IDs. Inherits the parent's model when omitted. Rejected when the named child already exists."},"effort":{"type":"string","minLength":1,"maxLength":64,"description":"Optional reasoning effort applied when this message creates the child. Inherits the parent's effort when omitted. Rejected when the named child already exists."}},"additionalProperties":false,"required":["action","agent","message"]}]}},"additionalProperties":false,"required":["request"]}"#;
const UNTARGETED_TITLE: &str = "Managing subagent";
const RUN_FIELDS: [&str; 4] = ["action", "task", "model", "effort"];
const MESSAGE_FIELDS: [&str; 6] = [
    "action",
    "agent",
    "instructions",
    "message",
    "model",
    "effort",
];

pub struct SubagentTool {
    spec: ToolSpec,
    provider: Arc<dyn SubagentProvider>,
}

impl SubagentTool {
    pub fn new(provider: Arc<dyn SubagentProvider>) -> Self {
        Self {
            spec: ToolSpec {
                name: TOOL_NAME.to_owned(),
                description: DESCRIPTION.to_owned(),
                input_schema: INPUT_SCHEMA,
            },
            provider,
        }
    }
}

impl Tool for SubagentTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn prepare(&self, arguments: &str) -> Result<Box<dyn PreparedCall>, ToolOutput> {
        let request = decode(arguments)
            .map_err(|code| ToolOutput::failure(SubagentResult::failure(code).encode()));
        let description = CallDescription {
            title: format_subagent_plain_action(TOOL_NAME, arguments, SubagentActionState::Active)
                .unwrap_or_else(|| UNTARGETED_TITLE.to_owned()),
            activity: ToolActivity::Subagent,
            effect: if request.is_ok() {
                ToolEffect::Mutating
            } else {
                ToolEffect::None
            },
            concurrency: Concurrency::Parallel,
        };
        Ok(Box::new(SubagentCall {
            description,
            request,
            provider: Arc::clone(&self.provider),
        }))
    }
}

struct SubagentCall {
    description: CallDescription,
    request: Result<SubagentRequest, ToolOutput>,
    provider: Arc<dyn SubagentProvider>,
}

impl PreparedCall for SubagentCall {
    fn describe(&self) -> CallDescription {
        self.description.clone()
    }

    fn refusal(&self) -> Option<&ToolOutput> {
        self.request.as_ref().err()
    }

    fn execute(self: Box<Self>, context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        match self.request {
            Ok(request) => self.provider.execute(request, context),
            Err(refusal) => Box::pin(async move { refusal }),
        }
    }
}

fn decode(arguments: &str) -> Result<SubagentRequest, &'static str> {
    if parse_tool_args_object(arguments) == Err(ToolArgsError::InvalidJson) {
        return Err("invalid_json");
    }
    let root: Value = serde_json::from_str(arguments).map_err(|_| "invalid_json")?;
    let input = request_input(&root)?;
    SubagentRequest::validate(input).map_err(SubagentRequestError::code)
}

fn request_input(root: &Value) -> Result<SubagentRequestInput<'_>, &'static str> {
    let root = object(root)?;
    let request = match root.get("request") {
        Some(request) => {
            reject_unknown(root, &["request"])?;
            object(request)?
        }
        None => root,
    };
    match required_string(request, "action")? {
        "run" => {
            reject_unknown(request, &RUN_FIELDS)?;
            Ok(SubagentRequestInput::Run {
                task: required_string(request, "task")?,
                model: optional_string(request, "model")?,
                effort: optional_string(request, "effort")?,
            })
        }
        "message" => {
            reject_unknown(request, &MESSAGE_FIELDS)?;
            Ok(SubagentRequestInput::Message {
                agent: required_string(request, "agent")?,
                instructions: optional_string(request, "instructions")?,
                message: required_string(request, "message")?,
                model: optional_string(request, "model")?,
                effort: optional_string(request, "effort")?,
            })
        }
        _ => Err("invalid_enum"),
    }
}

fn object(value: &Value) -> Result<&Map<String, Value>, &'static str> {
    value.as_object().ok_or("invalid_field_type")
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, &'static str> {
    optional_string(object, key)?.ok_or("missing_field")
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, &'static str> {
    object
        .get(key)
        .map(|value| value.as_str().ok_or("invalid_field_type"))
        .transpose()
}

fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), &'static str> {
    if object.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err("unknown_field")
    }
}

#[cfg(test)]
mod tests;
