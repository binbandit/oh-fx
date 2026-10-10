use std::fmt::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ofx_contract::{
    ChatMessage, DuplicateKeys, ImageAttachment, Json, ModelRequest, ProviderReplay, ReplaySource,
    ToolArgumentIntegrity, ToolCall, ToolExecutionProvenance, ToolResultStatus, ToolSpec,
    parse_strict_json, tool_permission_denial_reason,
};
use ofx_images::{AttachmentError, load_verified_snapshot};

use crate::chat_completions_protocol::capped_description;
use crate::responses_protocol::push_json_string;
use crate::tool_call_ids::{Projection, ProjectionError};
use crate::vercel_model_policy::parallel_tool_calls;

pub(crate) const REPLAY_PROVIDER: &str = "gateway";
const MAX_REPLAY_BYTES: usize = 4 * 1024 * 1024;
const USER_AGENT_MODEL: &str = "zai/glm-5.2";
const HIGHEST_REASONING_TIER: &str = "xhigh";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RequestError {
    #[error("InvalidProviderPrompt")]
    InvalidProviderPrompt,
    #[error("InvalidGatewayHistory")]
    InvalidGatewayHistory,
    #[error("InvalidProviderState")]
    InvalidProviderState,
    #[error("ProviderStateTooLarge")]
    ProviderStateTooLarge,
    #[error("InvalidToolCallId")]
    InvalidToolCallId,
    #[error("ToolCallIdMappingExhausted")]
    ToolCallIdMappingExhausted,
    #[error("ProtectedToolCallId")]
    ProtectedToolCallId,
    #[error("{0}")]
    Image(AttachmentError),
}

impl From<ProjectionError> for RequestError {
    fn from(error: ProjectionError) -> Self {
        match error {
            ProjectionError::InvalidToolCallId => Self::InvalidToolCallId,
            ProjectionError::ToolCallIdMappingExhausted => Self::ToolCallIdMappingExhausted,
            ProjectionError::ProtectedToolCallId => Self::ProtectedToolCallId,
        }
    }
}

type Result<T> = std::result::Result<T, RequestError>;

pub(crate) fn replay_source(model: &str) -> ReplaySource {
    ReplaySource {
        provider: REPLAY_PROVIDER.to_owned(),
        model: model.to_owned(),
        binding: None,
    }
}

pub(crate) fn build_request(request: &ModelRequest<'_>, user_agent: &str) -> Result<String> {
    if request
        .messages
        .iter()
        .any(|message| matches!(message, ChatMessage::System { .. }))
    {
        return Err(RequestError::InvalidProviderPrompt);
    }
    let replays = matching_replays(request);
    validate_history(request.messages)?;
    let replayed: Vec<bool> = replays.iter().map(Option::is_some).collect();
    let projection = Projection::protecting(request.messages, &replayed)?;
    let mut out = String::from("{\"prompt\":[");
    let mut writer = PromptWriter {
        out: &mut out,
        projection: &projection,
        first: true,
    };
    for instruction in request.instructions {
        writer.separate();
        writer.system(instruction);
    }
    writer.messages(request.messages, &replays)?;
    out.push_str("],\"tools\":");
    write_tools(&mut out, request.tools);
    out.push_str(",\"toolChoice\":{\"type\":");
    push_json_string(&mut out, request.tool_choice.as_str());
    out.push('}');
    if let Some(limit) = request.max_output_tokens {
        let _ = write!(out, ",\"maxOutputTokens\":{limit}");
    }
    if let Some(effort) = request.provider_options.reasoning_effort {
        out.push_str(",\"reasoning\":");
        push_json_string(&mut out, reasoning_wire_value(effort));
    }
    write_provider_options(
        &mut out,
        request.provider_options.fast,
        request.provider_options.prompt_caching,
        parallel_tool_calls(request.model),
    );
    if request.model == USER_AGENT_MODEL {
        out.push_str(",\"headers\":{\"user-agent\":");
        push_json_string(&mut out, user_agent);
        out.push('}');
    }
    out.push('}');
    Ok(out)
}

fn matching_replays<'a>(request: &ModelRequest<'a>) -> Vec<Option<&'a ProviderReplay>> {
    let source = replay_source(request.model);
    let mut omitted = false;
    let replays = request
        .messages
        .iter()
        .map(|message| match message {
            ChatMessage::Assistant {
                provider_replay: Some(replay),
                ..
            } => {
                let matches = replay.matches(&source);
                omitted |= !matches;
                matches.then_some(replay)
            }
            _ => None,
        })
        .collect();
    if omitted {
        ofx_trace::trace_log!(
            "gateway",
            "provider_replay_omitted provider=gateway reason=source_mismatch"
        );
    }
    replays
}

fn reasoning_wire_value(effort: &str) -> &str {
    if effort == "max" {
        HIGHEST_REASONING_TIER
    } else {
        effort
    }
}

fn write_tools(out: &mut String, tools: &[ToolSpec]) {
    out.push('[');
    let mut written: Vec<&str> = Vec::with_capacity(tools.len());
    for tool in tools {
        if written.contains(&tool.name.as_str()) {
            continue;
        }
        if !written.is_empty() {
            out.push(',');
        }
        out.push_str("{\"type\":\"function\",\"name\":");
        push_json_string(out, &tool.name);
        out.push_str(",\"description\":");
        push_json_string(out, &capped_description(&tool.description));
        out.push_str(",\"inputSchema\":");
        out.push_str(&tool.input_schema);
        out.push('}');
        written.push(&tool.name);
    }
    out.push(']');
}

fn write_provider_options(
    out: &mut String,
    fast: bool,
    prompt_caching: bool,
    parallel_tool_calls: Option<bool>,
) {
    let gateway = fast || prompt_caching;
    if !gateway && parallel_tool_calls.is_none() {
        return;
    }
    out.push_str(",\"providerOptions\":{");
    if gateway {
        out.push_str("\"gateway\":{");
        if fast {
            out.push_str("\"speed\":\"fast\"");
        }
        if prompt_caching {
            if fast {
                out.push(',');
            }
            out.push_str("\"caching\":\"auto\"");
        }
        out.push('}');
    }
    if let Some(parallel) = parallel_tool_calls {
        if gateway {
            out.push(',');
        }
        out.push_str("\"xai\":{\"parallelToolCalls\":");
        out.push_str(if parallel { "true" } else { "false" });
        out.push('}');
    }
    out.push('}');
}

fn validate_history(messages: &[ChatMessage]) -> Result<()> {
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            ChatMessage::Tool { .. } => return Err(RequestError::InvalidGatewayHistory),
            ChatMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                validate_calls(tool_calls)?;
                index = validate_results(messages, index + 1, tool_calls)?;
            }
            _ => index += 1,
        }
    }
    Ok(())
}

fn validate_calls(calls: &[ToolCall]) -> Result<()> {
    for (index, call) in calls.iter().enumerate() {
        if call.id.as_str().is_empty() || call.name.is_empty() || call.arguments.is_empty() {
            return Err(RequestError::InvalidGatewayHistory);
        }
        let integrity = if call.provenance == ToolExecutionProvenance::ProviderExecuted {
            ToolArgumentIntegrity::classify_serialized(&call.arguments)
        } else {
            ToolArgumentIntegrity::classify_function_input(&call.arguments)
        };
        if integrity != ToolArgumentIntegrity::Valid
            || calls[index + 1..].iter().any(|later| later.id == call.id)
        {
            return Err(RequestError::InvalidGatewayHistory);
        }
    }
    Ok(())
}

fn validate_results(messages: &[ChatMessage], start: usize, calls: &[ToolCall]) -> Result<usize> {
    let mut seen = vec![false; calls.len()];
    let mut index = start;
    for _ in calls {
        let Some(ChatMessage::Tool {
            call_id, tool_name, ..
        }) = messages.get(index)
        else {
            return Err(RequestError::InvalidGatewayHistory);
        };
        let matched = calls
            .iter()
            .position(|call| call.id == *call_id)
            .ok_or(RequestError::InvalidGatewayHistory)?;
        if seen[matched] || calls[matched].name != *tool_name {
            return Err(RequestError::InvalidGatewayHistory);
        }
        seen[matched] = true;
        index += 1;
    }
    Ok(index)
}

struct PromptWriter<'a> {
    out: &'a mut String,
    projection: &'a Projection,
    first: bool,
}

impl PromptWriter<'_> {
    fn separate(&mut self) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
    }

    fn system(&mut self, content: &str) {
        self.out.push_str("{\"role\":\"system\",\"content\":");
        push_json_string(self.out, content);
        self.out.push('}');
    }

    fn messages(
        &mut self,
        messages: &[ChatMessage],
        replays: &[Option<&ProviderReplay>],
    ) -> Result<()> {
        let mut index = 0;
        while index < messages.len() {
            self.separate();
            match &messages[index] {
                ChatMessage::Tool { .. } => {
                    let end = messages[index..]
                        .iter()
                        .position(|message| !matches!(message, ChatMessage::Tool { .. }))
                        .map_or(messages.len(), |offset| index + offset);
                    self.tool_results(&messages[index..end]);
                    index = end;
                    continue;
                }
                ChatMessage::System { content } => self.system(content),
                ChatMessage::User {
                    content, images, ..
                } => self.user(content, images)?,
                ChatMessage::Assistant {
                    content,
                    tool_calls,
                    ..
                } => match replays.get(index).copied().flatten() {
                    Some(replay) => self.replayed(content.as_deref(), tool_calls, replay)?,
                    None => self.assistant(content.as_deref(), tool_calls),
                },
            }
            index += 1;
        }
        Ok(())
    }

    fn user(&mut self, content: &str, images: &[ImageAttachment]) -> Result<()> {
        self.out.push_str("{\"role\":\"user\",\"content\":[");
        let mut wrote = false;
        if !content.is_empty() {
            self.text_part(content);
            wrote = true;
        }
        for attachment in images {
            let image =
                load_verified_snapshot(attachment, &|| Ok(())).map_err(RequestError::Image)?;
            if wrote {
                self.out.push(',');
            }
            self.out.push_str("{\"type\":\"file\",\"mediaType\":\"");
            self.out.push_str(image.media_type);
            self.out
                .push_str("\",\"data\":{\"type\":\"data\",\"data\":\"");
            STANDARD.encode_string(&image.bytes, self.out);
            self.out.push_str("\"}}");
            wrote = true;
        }
        self.out.push_str("]}");
        Ok(())
    }

    fn assistant(&mut self, content: Option<&str>, calls: &[ToolCall]) {
        self.out.push_str("{\"role\":\"assistant\",\"content\":[");
        let mut wrote = false;
        if let Some(text) = content.filter(|text| !text.is_empty()) {
            self.text_part(text);
            wrote = true;
        }
        for call in calls {
            if wrote {
                self.out.push(',');
            }
            self.tool_call(call, None);
            wrote = true;
        }
        self.out.push_str("]}");
    }

    fn text_part(&mut self, text: &str) {
        self.out.push_str("{\"type\":\"text\",\"text\":");
        push_json_string(self.out, text);
        self.out.push('}');
    }

    fn tool_call(&mut self, call: &ToolCall, metadata: Option<&str>) {
        self.out.push_str("{\"type\":\"tool-call\",\"toolCallId\":");
        push_json_string(self.out, self.projection.resolve(call.id.as_str()));
        self.out.push_str(",\"toolName\":");
        push_json_string(self.out, &call.name);
        self.out.push_str(",\"input\":");
        self.out.push_str(&call.arguments);
        self.metadata(metadata);
        self.out.push('}');
    }

    fn metadata(&mut self, metadata: Option<&str>) {
        if let Some(options) = metadata {
            self.out.push_str(",\"providerOptions\":");
            self.out.push_str(options);
        }
    }

    fn tool_results(&mut self, results: &[ChatMessage]) {
        self.out.push_str("{\"role\":\"tool\",\"content\":[");
        for (index, result) in results.iter().enumerate() {
            let ChatMessage::Tool {
                call_id,
                tool_name,
                content,
                status,
            } = result
            else {
                continue;
            };
            if index > 0 {
                self.out.push(',');
            }
            self.out
                .push_str("{\"type\":\"tool-result\",\"toolCallId\":");
            push_json_string(self.out, self.projection.resolve(call_id.as_str()));
            self.out.push_str(",\"toolName\":");
            push_json_string(self.out, tool_name);
            let failed = *status == ToolResultStatus::Failure;
            self.out.push_str(
                if failed && tool_permission_denial_reason(content).is_some() {
                    ",\"output\":{\"type\":\"execution-denied\",\"reason\":"
                } else if failed {
                    ",\"output\":{\"type\":\"error-text\",\"value\":"
                } else {
                    ",\"output\":{\"type\":\"text\",\"value\":"
                },
            );
            push_json_string(self.out, content);
            self.out.push_str("}}");
        }
        self.out.push_str("]}");
    }

    fn replayed(
        &mut self,
        content: Option<&str>,
        calls: &[ToolCall],
        replay: &ProviderReplay,
    ) -> Result<()> {
        if replay.parts_json.len() > MAX_REPLAY_BYTES {
            return Err(RequestError::ProviderStateTooLarge);
        }
        let Ok(Json::Array(parts)) =
            parse_strict_json(replay.parts_json.as_bytes(), DuplicateKeys::AfterValue)
        else {
            return Err(RequestError::InvalidProviderState);
        };
        let content = content.unwrap_or("");
        let mut seen = vec![false; calls.len()];
        let mut text_end = 0;
        let mut wrote = false;
        self.out.push_str("{\"role\":\"assistant\",\"content\":[");
        for part in &parts {
            let fields = part.as_object().ok_or(RequestError::InvalidProviderState)?;
            let kind = fields
                .get("type")
                .and_then(Json::as_str)
                .ok_or(RequestError::InvalidProviderState)?;
            if wrote {
                self.out.push(',');
            }
            let metadata = replay_metadata(fields.get("providerOptions"))?;
            match kind {
                "reasoning" => {
                    let text = fields
                        .get("text")
                        .and_then(Json::as_str)
                        .ok_or(RequestError::InvalidProviderState)?;
                    self.out.push_str("{\"type\":\"reasoning\",\"text\":");
                    push_json_string(self.out, text);
                    self.metadata(metadata.as_deref());
                    self.out.push('}');
                }
                "text" => {
                    let offset = replay_index(fields.get("offset"))?;
                    let length = replay_index(fields.get("length"))?;
                    let end = offset
                        .checked_add(length)
                        .filter(|end| offset == text_end && *end <= content.len())
                        .ok_or(RequestError::InvalidProviderState)?;
                    let text = content
                        .get(offset..end)
                        .ok_or(RequestError::InvalidProviderState)?;
                    text_end = end;
                    self.out.push_str("{\"type\":\"text\",\"text\":");
                    push_json_string(self.out, text);
                    self.metadata(metadata.as_deref());
                    self.out.push('}');
                }
                "tool-call" => {
                    let id = fields
                        .get("toolCallId")
                        .and_then(Json::as_str)
                        .ok_or(RequestError::InvalidProviderState)?;
                    let index = calls
                        .iter()
                        .position(|call| call.id.as_str() == id)
                        .filter(|index| !seen[*index])
                        .ok_or(RequestError::InvalidProviderState)?;
                    seen[index] = true;
                    self.tool_call(&calls[index], metadata.as_deref());
                }
                _ => return Err(RequestError::InvalidProviderState),
            }
            wrote = true;
        }
        if text_end < content.len() {
            if wrote {
                self.out.push(',');
            }
            self.text_part(&content[text_end..]);
            wrote = true;
        }
        for (call, seen) in calls.iter().zip(seen) {
            if seen {
                continue;
            }
            if wrote {
                self.out.push(',');
            }
            self.tool_call(call, None);
            wrote = true;
        }
        self.out.push_str("]}");
        Ok(())
    }
}

fn replay_metadata(metadata: Option<&Json<'_>>) -> Result<Option<String>> {
    let Some(value) = metadata else {
        return Ok(None);
    };
    let fields = value
        .as_object()
        .ok_or(RequestError::InvalidProviderState)?;
    if fields
        .iter()
        .any(|(_, options)| options.as_object().is_none())
    {
        return Err(RequestError::InvalidProviderState);
    }
    serde_json::to_string(value)
        .map(Some)
        .map_err(|_| RequestError::InvalidProviderState)
}

fn replay_index(value: Option<&Json<'_>>) -> Result<usize> {
    value
        .and_then(Json::as_i64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or(RequestError::InvalidProviderState)
}

#[cfg(test)]
mod tests;
