use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::mem;

use ofx_config::{MAX_MODEL_BYTES, MaxTokensParameter, ToolChoiceMode, is_valid_model_id};
use ofx_contract::{
    ChatMessage, Completion, DuplicateKeys, FinishReason, Json, ModelRequest, Object,
    ToolArgumentIntegrity, ToolCall, ToolCallId, ToolChoice, ToolSpec, Usage, parse_strict_json,
    parse_strict_json_value,
};
use serde::Serialize;

pub(crate) use crate::secret_mask::mask_configured_secrets;
use crate::tool_call_ids::{Projection, ProjectionError};

const MAX_SELECTED_TOOLS: usize = 256;
const MAX_NAME_BYTES: usize = 128;
const MAX_HISTORY_ARGUMENTS_BYTES: usize = 1024 * 1024;
const MAX_JSON_DEPTH: usize = 64;
const MAX_REPLAY_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_DETAIL_BYTES: usize = 64 * 1024;
const DESCRIPTION_MAX_BYTES: usize = 1024;
const INITIAL_BODY_BYTES: usize = 128;
const TRUNCATION_MARKER: &str = "... [truncated]";
const REASONING_FIELDS: [&str; 2] = ["reasoning", "reasoning_content"];
const DETAIL_LIMIT_NOTICE: &str = "Provider error details exceeded the local limit";
const DETAIL_NESTING_NOTICE: &str = "Provider error details exceeded the nesting limit";
const DETAIL_DECODE_NOTICE: &str = "Provider error details could not be decoded";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProtocolError {
    #[error("InvalidProviderPrompt")]
    InvalidProviderPrompt,
    #[error("InvalidModel")]
    InvalidModel,
    #[error("InvalidOutputLimit")]
    InvalidOutputLimit,
    #[error("InvalidToolSelection")]
    InvalidToolSelection,
    #[error("InvalidToolCallId")]
    InvalidToolCallId,
    #[error("InvalidToolName")]
    InvalidToolName,
    #[error("InvalidToolArguments")]
    InvalidToolArguments,
    #[error("InvalidToolHistory")]
    InvalidToolHistory,
    #[error("RequiredToolMissing")]
    RequiredToolMissing,
    #[error("UnexpectedToolCall")]
    UnexpectedToolCall,
    #[error("InvalidChunk")]
    InvalidChunk,
    #[error("ConflictingIdentity")]
    ConflictingIdentity,
    #[error("InconsistentFinishReason")]
    InconsistentFinishReason,
    #[error("IncompleteStream")]
    IncompleteStream,
    #[error("StreamClosed")]
    StreamClosed,
    #[error("ProviderError")]
    ProviderError,
    #[error("OutputTruncated")]
    OutputTruncated,
    #[error("ContentFiltered")]
    ContentFiltered,
    #[error("Refused")]
    Refused,
    #[error("EventTooLarge")]
    EventTooLarge,
    #[error("StreamTooLarge")]
    StreamTooLarge,
    #[error("TooManyEvents")]
    TooManyEvents,
    #[error("TooManyTools")]
    TooManyTools,
    #[error("IdentityTooLarge")]
    IdentityTooLarge,
    #[error("ArgumentsTooLarge")]
    ArgumentsTooLarge,
    #[error("ContentTooLarge")]
    ContentTooLarge,
    #[error("ReplayTooLarge")]
    ReplayTooLarge,
    #[error("JsonTooDeep")]
    JsonTooDeep,
    #[error("ToolCallIdMappingExhausted")]
    ToolCallIdMappingExhausted,
    #[error("Cancelled")]
    Cancelled,
}

impl From<ProjectionError> for ProtocolError {
    fn from(error: ProjectionError) -> Self {
        match error {
            ProjectionError::InvalidToolCallId | ProjectionError::ProtectedToolCallId => {
                Self::InvalidToolCallId
            }
            ProjectionError::ToolCallIdMappingExhausted => Self::ToolCallIdMappingExhausted,
        }
    }
}

type ProtocolResult<T> = Result<T, ProtocolError>;

fn validate_name(name: &str) -> ProtocolResult<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(ProtocolError::InvalidToolName)
    }
}

fn select_functions(tools: &[ToolSpec], choice: ToolChoice) -> ProtocolResult<Vec<&ToolSpec>> {
    if choice == ToolChoice::None {
        return Ok(Vec::new());
    }
    if tools.len() > MAX_SELECTED_TOOLS {
        return Err(ProtocolError::TooManyTools);
    }
    let mut functions: Vec<&ToolSpec> = Vec::with_capacity(tools.len());
    for tool in tools {
        validate_name(&tool.name)?;
        if functions.iter().any(|prior| prior.name == tool.name) {
            return Err(ProtocolError::InvalidToolSelection);
        }
        functions.push(tool);
    }
    if choice == ToolChoice::Required && functions.is_empty() {
        return Err(ProtocolError::RequiredToolMissing);
    }
    Ok(functions)
}

fn validate_request(request: &ModelRequest<'_>) -> ProtocolResult<()> {
    if !is_valid_model_id(request.model) {
        return Err(ProtocolError::InvalidModel);
    }
    if request.max_output_tokens == Some(0) {
        return Err(ProtocolError::InvalidOutputLimit);
    }
    for message in request.messages {
        match message {
            ChatMessage::System { .. } => return Err(ProtocolError::InvalidProviderPrompt),
            ChatMessage::Assistant {
                content: None,
                tool_calls,
                ..
            } if tool_calls.is_empty() => return Err(ProtocolError::InvalidProviderPrompt),
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn check_json_depth(text: &[u8]) -> ProtocolResult<()> {
    let mut depth = 0_usize;
    let mut quoted = false;
    let mut escaped = false;
    for &byte in text {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' | b'[' => {
                if depth == MAX_JSON_DEPTH {
                    return Err(ProtocolError::JsonTooDeep);
                }
                depth += 1;
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

fn validate_arguments(text: &str) -> ProtocolResult<()> {
    check_json_depth(text.as_bytes())?;
    if ToolArgumentIntegrity::classify_function_input(text) == ToolArgumentIntegrity::Valid {
        Ok(())
    } else {
        Err(ProtocolError::InvalidToolArguments)
    }
}

fn validate_history(messages: &[ChatMessage]) -> ProtocolResult<()> {
    let mut pending: HashMap<&str, &str> = HashMap::new();
    for message in messages {
        if let ChatMessage::Tool {
            call_id, tool_name, ..
        } = message
        {
            if call_id.as_str().is_empty() {
                return Err(ProtocolError::InvalidToolCallId);
            }
            let name = pending
                .remove(call_id.as_str())
                .ok_or(ProtocolError::InvalidToolHistory)?;
            if name != tool_name {
                return Err(ProtocolError::InvalidToolHistory);
            }
            continue;
        }
        if !pending.is_empty() {
            return Err(ProtocolError::InvalidToolHistory);
        }
        let ChatMessage::Assistant { tool_calls, .. } = message else {
            continue;
        };
        if tool_calls.len() > MAX_SELECTED_TOOLS {
            return Err(ProtocolError::TooManyTools);
        }
        for call in tool_calls {
            if call.id.as_str().is_empty() {
                return Err(ProtocolError::InvalidToolCallId);
            }
            validate_name(&call.name)?;
            if call.arguments.len() > MAX_HISTORY_ARGUMENTS_BYTES {
                return Err(ProtocolError::ArgumentsTooLarge);
            }
            validate_arguments(&call.arguments)?;
            if pending.insert(call.id.as_str(), &call.name).is_some() {
                return Err(ProtocolError::InvalidToolHistory);
            }
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(ProtocolError::InvalidToolHistory)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestOptions {
    pub(crate) tool_choice_mode: ToolChoiceMode,
    pub(crate) max_tokens_parameter: MaxTokensParameter,
}

#[derive(Debug)]
pub(crate) struct PreparedRequest {
    pub(crate) body: Vec<u8>,
    pub(crate) selection: Selection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    choice: ToolChoice,
    names: Vec<String>,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum WireMessage<'a> {
    System {
        content: &'a str,
    },
    User {
        content: &'a str,
    },
    Assistant {
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<WireToolCall<'a>>,
    },
    Tool {
        content: &'a str,
        tool_call_id: &'a str,
    },
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunctionCall<'a>,
}

#[derive(Serialize)]
struct WireFunctionCall<'a> {
    name: &'a str,
    arguments: &'a str,
}

pub(crate) fn build_request(
    request: &ModelRequest<'_>,
    options: RequestOptions,
) -> ProtocolResult<PreparedRequest> {
    validate_request(request)?;
    validate_history(request.messages)?;
    let functions = select_functions(request.tools, request.tool_choice)?;
    let selection = Selection::of(request.tool_choice, &functions);
    let projection = Projection::new(request.messages)?;
    let messages: Vec<WireMessage<'_>> = request
        .instructions
        .iter()
        .map(|instruction| WireMessage::System {
            content: instruction,
        })
        .chain(
            request
                .messages
                .iter()
                .map(|message| encode_message(message, &projection)),
        )
        .collect();
    let mut body = Vec::with_capacity(INITIAL_BODY_BYTES);
    body.extend_from_slice(b"{\"model\":");
    write_json(&mut body, request.model)?;
    body.extend_from_slice(
        b",\"stream\":true,\"stream_options\":{\"include_usage\":true},\"messages\":",
    );
    write_json(&mut body, &messages)?;
    if !functions.is_empty() {
        write_tools(&mut body, &functions)?;
        if options.tool_choice_mode == ToolChoiceMode::Send {
            body.extend_from_slice(b",\"tool_choice\":");
            write_json(&mut body, request.tool_choice.as_str())?;
        }
    }
    if let Some(limit) = request.max_output_tokens {
        body.push(b',');
        write_json(&mut body, options.max_tokens_parameter.field())?;
        body.push(b':');
        write_json(&mut body, &limit)?;
    }
    body.push(b'}');
    Ok(PreparedRequest { body, selection })
}

pub(crate) fn request_selection(request: &ModelRequest<'_>) -> ProtocolResult<Selection> {
    let functions = select_functions(request.tools, request.tool_choice)?;
    Ok(Selection::of(request.tool_choice, &functions))
}

impl Selection {
    fn of(choice: ToolChoice, functions: &[&ToolSpec]) -> Self {
        Self {
            choice,
            names: functions.iter().map(|tool| tool.name.clone()).collect(),
        }
    }
}

fn encode_message<'a>(message: &'a ChatMessage, projection: &'a Projection) -> WireMessage<'a> {
    match message {
        ChatMessage::System { content } => WireMessage::System { content },
        ChatMessage::User { content, .. } => WireMessage::User { content },
        ChatMessage::Assistant {
            content,
            tool_calls,
            ..
        } => WireMessage::Assistant {
            content: content.as_deref(),
            tool_calls: tool_calls
                .iter()
                .map(|call| WireToolCall {
                    id: projection.resolve(call.id.as_str()),
                    kind: "function",
                    function: WireFunctionCall {
                        name: &call.name,
                        arguments: &call.arguments,
                    },
                })
                .collect(),
        },
        ChatMessage::Tool {
            call_id, content, ..
        } => WireMessage::Tool {
            content,
            tool_call_id: projection.resolve(call_id.as_str()),
        },
    }
}

fn write_tools(body: &mut Vec<u8>, functions: &[&ToolSpec]) -> ProtocolResult<()> {
    let mut separator = ",\"tools\":[";
    for tool in functions {
        body.extend_from_slice(separator.as_bytes());
        body.extend_from_slice(b"{\"type\":\"function\",\"function\":{\"name\":");
        write_json(body, &tool.name)?;
        body.extend_from_slice(b",\"description\":");
        write_json(body, &capped_description(&tool.description))?;
        body.extend_from_slice(b",\"parameters\":");
        body.extend_from_slice(tool.input_schema.as_bytes());
        body.extend_from_slice(b"}}");
        separator = ",";
    }
    body.push(b']');
    Ok(())
}

fn write_json<T: Serialize + ?Sized>(body: &mut Vec<u8>, value: &T) -> ProtocolResult<()> {
    serde_json::to_writer(body, value).map_err(|_| ProtocolError::InvalidProviderPrompt)
}

pub(crate) fn capped_description(text: &str) -> Cow<'_, str> {
    if text.len() <= DESCRIPTION_MAX_BYTES {
        return Cow::Borrowed(text);
    }
    let prefix = text.floor_char_boundary(DESCRIPTION_MAX_BYTES - TRUNCATION_MARKER.len());
    Cow::Owned(format!("{}{TRUNCATION_MARKER}", &text[..prefix]))
}

pub(crate) fn redact_error_detail(raw: &[u8], secrets: &[String]) -> String {
    if raw.len() > MAX_ERROR_DETAIL_BYTES {
        return DETAIL_LIMIT_NOTICE.to_owned();
    }
    if check_json_depth(raw).is_err() {
        return DETAIL_NESTING_NOTICE.to_owned();
    }
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim_matches([' ', '\t', '\r', '\n']);
    let json_shaped = trimmed.starts_with(['{', '[', '"']);
    let detail = match parse_strict_json_value(raw) {
        Ok(value) => value.to_string(),
        Err(_) if json_shaped => return DETAIL_DECODE_NOTICE.to_owned(),
        Err(_) => text.into_owned(),
    };
    if detail.len() > MAX_ERROR_DETAIL_BYTES {
        return DETAIL_LIMIT_NOTICE.to_owned();
    }
    mask_configured_secrets(detail, secrets)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) event_bytes: usize,
    pub(crate) total_wire_bytes: usize,
    pub(crate) events: usize,
    pub(crate) tool_calls: usize,
    pub(crate) identity_bytes: usize,
    pub(crate) arguments_bytes: usize,
    pub(crate) content_bytes: usize,
    pub(crate) reasoning_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            event_bytes: 1024 * 1024,
            total_wire_bytes: 32 * 1024 * 1024,
            events: 100_000,
            tool_calls: 128,
            identity_bytes: 256,
            arguments_bytes: 1024 * 1024,
            content_bytes: 8 * 1024 * 1024,
            reasoning_bytes: MAX_REPLAY_BYTES,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Deltas {
    pub(crate) content: Option<String>,
    pub(crate) reasoning: Vec<String>,
}

#[derive(Debug, Default)]
struct ToolAccumulator {
    id: Option<String>,
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Receiving,
    Finished,
    Done,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireFinish {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Unrecognized,
}

impl WireFinish {
    fn parse(text: &str) -> ProtocolResult<Self> {
        Ok(match text {
            "stop" => Self::Stop,
            "tool_calls" => Self::ToolCalls,
            "length" => Self::Length,
            "content_filter" => Self::ContentFilter,
            "error" => return Err(ProtocolError::ProviderError),
            _ => Self::Unrecognized,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct UsageCounters {
    input: Option<u64>,
    output: Option<u64>,
    total: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FinalFields {
    input: bool,
    output: bool,
    total: bool,
}

#[derive(Debug)]
pub(crate) struct Reducer {
    limits: Limits,
    choice: ToolChoice,
    names: Vec<String>,
    tools: Vec<ToolAccumulator>,
    content: String,
    generation_id: Option<String>,
    response_model: Option<String>,
    usage: UsageCounters,
    final_fields: FinalFields,
    reasoning_bytes: usize,
    refusal_seen: bool,
    finish_reason: Option<(WireFinish, String)>,
    phase: Phase,
    event_count: usize,
    json_bytes: usize,
    failure_detail: Option<String>,
}

impl Reducer {
    pub(crate) fn new(selection: Selection, limits: Limits) -> Self {
        Self {
            limits,
            choice: selection.choice,
            names: selection.names,
            tools: Vec::new(),
            content: String::new(),
            generation_id: None,
            response_model: None,
            usage: UsageCounters::default(),
            final_fields: FinalFields::default(),
            reasoning_bytes: 0,
            refusal_seen: false,
            finish_reason: None,
            phase: Phase::Receiving,
            event_count: 0,
            json_bytes: 0,
            failure_detail: None,
        }
    }

    pub(crate) fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    pub(crate) fn event_count(&self) -> usize {
        self.event_count
    }

    pub(crate) fn take_failure_detail(&mut self) -> Option<String> {
        self.failure_detail.take()
    }

    pub(crate) fn accept(&mut self, data: &[u8], cancelled: bool) -> ProtocolResult<Deltas> {
        let result = self.accept_event(data, cancelled);
        if result.is_err() {
            self.phase = Phase::Closed;
        }
        result
    }

    pub(crate) fn end_of_stream(&mut self) -> ProtocolResult<()> {
        if self.phase == Phase::Finished {
            self.phase = Phase::Done;
            Ok(())
        } else {
            self.phase = Phase::Closed;
            Err(ProtocolError::IncompleteStream)
        }
    }

    pub(crate) fn finish(&mut self, cancelled: bool) -> ProtocolResult<Completion> {
        let result = self.complete(cancelled);
        self.phase = Phase::Closed;
        result
    }

    fn accept_event(&mut self, data: &[u8], cancelled: bool) -> ProtocolResult<Deltas> {
        if cancelled {
            return Err(ProtocolError::Cancelled);
        }
        if matches!(self.phase, Phase::Closed | Phase::Done) {
            return Err(ProtocolError::StreamClosed);
        }
        if data.len() > self.limits.event_bytes {
            return Err(ProtocolError::EventTooLarge);
        }
        if data.len() > self.limits.total_wire_bytes.saturating_sub(self.json_bytes) {
            return Err(ProtocolError::StreamTooLarge);
        }
        self.json_bytes += data.len();
        if self.event_count == self.limits.events {
            return Err(ProtocolError::TooManyEvents);
        }
        self.event_count += 1;
        if data == b"[DONE]" {
            if self.phase != Phase::Finished {
                return Err(ProtocolError::IncompleteStream);
            }
            self.phase = Phase::Done;
            return Ok(Deltas::default());
        }
        check_json_depth(data)?;
        let Some(Json::Object(root)) = parse_strict_json(data, DuplicateKeys::AfterObject).ok()
        else {
            return Err(ProtocolError::InvalidChunk);
        };
        if let Some(error) = non_null(&root, "error") {
            self.failure_detail = Some(compact(error));
            return Err(ProtocolError::ProviderError);
        }
        if root.get("object").and_then(Json::as_str) == Some("error") {
            self.failure_detail = Some(compact(&root));
            return Err(ProtocolError::ProviderError);
        }
        accept_identity(
            &mut self.generation_id,
            non_null(&root, "id"),
            self.limits.identity_bytes,
        )?;
        accept_identity(
            &mut self.response_model,
            non_null(&root, "model"),
            MAX_MODEL_BYTES,
        )?;
        let Some(choices) = root.get("choices").and_then(Json::as_array) else {
            return Err(ProtocolError::InvalidChunk);
        };
        if choices.len() > 1 {
            return Err(ProtocolError::InvalidChunk);
        }
        let Some(choice) = choices.first() else {
            if let Some(usage) = non_null(&root, "usage") {
                self.accept_usage(usage, self.phase == Phase::Finished)?;
            }
            return Ok(Deltas::default());
        };
        self.accept_choice(&root, object(choice)?)
    }

    fn accept_choice(&mut self, root: &Object<'_>, choice: &Object<'_>) -> ProtocolResult<Deltas> {
        if index_value(choice.get("index").ok_or(ProtocolError::InvalidChunk)?)? != 0 {
            return Err(ProtocolError::InvalidChunk);
        }
        let no_delta = Object::default();
        let delta = match choice.get("delta") {
            None | Some(Json::Null) => &no_delta,
            Some(Json::Object(delta)) => delta,
            Some(_) => return Err(ProtocolError::InvalidChunk),
        };
        if self.phase == Phase::Finished {
            self.accept_repeated_finish(root, choice, delta)?;
            return Ok(Deltas::default());
        }
        if let Some(role) = non_null(delta, "role")
            && string(role)? != "assistant"
        {
            return Err(ProtocolError::InvalidChunk);
        }
        if ["function_call", "audio"]
            .iter()
            .any(|key| non_null(delta, key).is_some())
        {
            return Err(ProtocolError::InvalidChunk);
        }
        let reasoning = self.accept_reasoning(delta)?;
        let content = match delta.get("content") {
            None | Some(Json::Null) => None,
            Some(Json::String(text)) => {
                append_bounded(
                    &mut self.content,
                    text,
                    self.limits.content_bytes,
                    ProtocolError::ContentTooLarge,
                )?;
                (!text.is_empty()).then(|| text.clone().into_owned())
            }
            Some(_) => return Err(ProtocolError::InvalidChunk),
        };
        if let Some(value) = non_null(delta, "refusal") {
            self.refusal_seen = self.refusal_seen || !string(value)?.is_empty();
        }
        if let Some(value) = non_null(delta, "tool_calls") {
            self.accept_tools(value)?;
        }
        let finish = non_null(choice, "finish_reason");
        if let Some(usage) = non_null(root, "usage") {
            self.accept_usage(usage, finish.is_some())?;
        }
        if let Some(value) = finish {
            let text = string(value)?;
            self.finish_reason = Some((WireFinish::parse(text)?, text.to_owned()));
            self.phase = Phase::Finished;
        }
        Ok(Deltas { content, reasoning })
    }

    fn accept_repeated_finish(
        &mut self,
        root: &Object<'_>,
        choice: &Object<'_>,
        delta: &Object<'_>,
    ) -> ProtocolResult<()> {
        let inconsistent = ProtocolError::InconsistentFinishReason;
        let reason = non_null(choice, "finish_reason").ok_or(inconsistent)?;
        let expected = self.finish_reason.as_ref().map(|(_, text)| text.as_str());
        if Some(string(reason)?) != expected {
            return Err(inconsistent);
        }
        for (key, value) in delta.iter() {
            let allowed = match key {
                "role" => value.as_str() == Some("assistant"),
                "content" => value.is_null() || value.as_str() == Some(""),
                _ => false,
            };
            if !allowed {
                return Err(inconsistent);
            }
        }
        match non_null(root, "usage") {
            Some(usage) => self.accept_usage(usage, true),
            None => Ok(()),
        }
    }

    fn accept_reasoning(&mut self, fields: &Object<'_>) -> ProtocolResult<Vec<String>> {
        let mut deltas = Vec::new();
        for key in REASONING_FIELDS {
            match fields.get(key) {
                None | Some(Json::Null) => {}
                Some(Json::String(text)) => {
                    self.count_reasoning(encoded_json_string_len(text))?;
                    if !text.is_empty() {
                        deltas.push(text.clone().into_owned());
                    }
                }
                Some(_) => return Err(ProtocolError::InvalidChunk),
            }
        }
        if let Some(details) = fields.get("reasoning_details")
            && !details.is_null()
        {
            let items = details.as_array().ok_or(ProtocolError::InvalidChunk)?;
            for item in items {
                if item.as_object().is_none() {
                    return Err(ProtocolError::InvalidChunk);
                }
                self.count_reasoning(encoded_json_len(item) + 1)?;
            }
        }
        Ok(deltas)
    }

    fn count_reasoning(&mut self, bytes: usize) -> ProtocolResult<()> {
        let limit = self.limits.reasoning_bytes.min(MAX_REPLAY_BYTES);
        if bytes > limit.saturating_sub(self.reasoning_bytes) {
            return Err(ProtocolError::ReplayTooLarge);
        }
        self.reasoning_bytes += bytes;
        Ok(())
    }

    fn accept_tools(&mut self, value: &Json<'_>) -> ProtocolResult<()> {
        let items = value.as_array().ok_or(ProtocolError::InvalidChunk)?;
        if items.len() > self.limits.tool_calls {
            return Err(ProtocolError::TooManyTools);
        }
        if !items.is_empty() && self.choice == ToolChoice::None {
            return Err(ProtocolError::UnexpectedToolCall);
        }
        let mut seen = Vec::with_capacity(items.len());
        for item in items {
            let delta = object(item)?;
            let index = match non_null(delta, "index") {
                Some(index) => index_value(index)?,
                None => self.implied_index(delta)?,
            };
            if index >= self.limits.tool_calls {
                return Err(ProtocolError::TooManyTools);
            }
            if seen.contains(&index) {
                return Err(ProtocolError::ConflictingIdentity);
            }
            seen.push(index);
            if self.tools.len() <= index {
                self.tools.resize_with(index + 1, ToolAccumulator::default);
            }
            if let Some(kind) = non_null(delta, "type")
                && string(kind)? != "function"
            {
                return Err(ProtocolError::InvalidChunk);
            }
            accept_identity(
                &mut self.tools[index].id,
                non_null(delta, "id"),
                self.limits.identity_bytes,
            )?;
            if let Some(id) = &self.tools[index].id
                && self.tools.iter().enumerate().any(|(other_index, other)| {
                    other_index != index && other.id.as_ref() == Some(id)
                })
            {
                return Err(ProtocolError::ConflictingIdentity);
            }
            if let Some(function) = non_null(delta, "function") {
                self.accept_function(index, object(function)?)?;
            }
        }
        Ok(())
    }

    fn implied_index(&self, delta: &Object<'_>) -> ProtocolResult<usize> {
        let id = non_null(delta, "id")
            .map(string)
            .transpose()?
            .filter(|id| !id.is_empty());
        let Some(id) = id else {
            return Ok(self.tools.len().saturating_sub(1));
        };
        Ok(self
            .tools
            .iter()
            .position(|tool| tool.id.as_deref() == Some(id))
            .unwrap_or(self.tools.len()))
    }

    fn accept_function(&mut self, index: usize, function: &Object<'_>) -> ProtocolResult<()> {
        let tool = &mut self.tools[index];
        if let Some(name) = non_null(function, "name") {
            let fragment = string(name)?;
            let echoed = !fragment.is_empty() && fragment == tool.name;
            let extends = extends_known_name(&self.names, &tool.name, fragment);
            if echoed && extends && self.names.contains(&tool.name) {
                return Err(ProtocolError::InvalidToolName);
            }
            if !echoed || extends {
                let limit = MAX_NAME_BYTES.min(self.limits.identity_bytes);
                append_bounded(
                    &mut tool.name,
                    fragment,
                    limit,
                    ProtocolError::IdentityTooLarge,
                )?;
                if !self
                    .names
                    .iter()
                    .any(|known| known.starts_with(tool.name.as_str()))
                {
                    return Err(ProtocolError::InvalidToolName);
                }
            }
        }
        if let Some(arguments) = non_null(function, "arguments") {
            append_bounded(
                &mut tool.arguments,
                string(arguments)?,
                self.limits.arguments_bytes,
                ProtocolError::ArgumentsTooLarge,
            )?;
        }
        Ok(())
    }

    fn accept_usage(&mut self, value: &Json<'_>, is_final: bool) -> ProtocolResult<()> {
        let fields = object(value)?;
        let incoming = UsageCounters {
            input: token_count(fields, "prompt_tokens")?,
            output: token_count(fields, "completion_tokens")?,
            total: token_count(fields, "total_tokens")?,
        };
        let merged = UsageCounters {
            input: incoming.input.or(self.usage.input),
            output: incoming.output.or(self.usage.output),
            total: incoming.total.or(self.usage.total),
        };
        let mut final_fields = self.final_fields;
        if is_final {
            final_fields.input |= incoming.input.is_some();
            final_fields.output |= incoming.output.is_some();
            final_fields.total |= incoming.total.is_some();
        }
        let observations = [
            (self.usage.input, merged.input, self.final_fields.input),
            (self.usage.output, merged.output, self.final_fields.output),
            (self.usage.total, merged.total, self.final_fields.total),
        ];
        for (previous, current, was_final) in observations {
            if let (Some(prior), Some(current)) = (previous, current)
                && (current < prior || (was_final && current != prior))
            {
                return Err(ProtocolError::ConflictingIdentity);
            }
        }
        self.usage = merged;
        self.final_fields = final_fields;
        Ok(())
    }

    fn complete(&mut self, cancelled: bool) -> ProtocolResult<Completion> {
        if cancelled {
            return Err(ProtocolError::Cancelled);
        }
        if self.phase == Phase::Closed {
            return Err(ProtocolError::StreamClosed);
        }
        if self.phase != Phase::Done {
            return Err(ProtocolError::IncompleteStream);
        }
        let (reason, _) = self
            .finish_reason
            .take()
            .ok_or(ProtocolError::IncompleteStream)?;
        let finish_reason = match reason {
            WireFinish::Length => return Err(ProtocolError::OutputTruncated),
            WireFinish::ContentFilter => return Err(ProtocolError::ContentFiltered),
            WireFinish::Stop | WireFinish::Unrecognized if self.tools.is_empty() => {
                FinishReason::Stop
            }
            WireFinish::ToolCalls | WireFinish::Stop | WireFinish::Unrecognized => {
                FinishReason::ToolCalls
            }
        };
        if self.refusal_seen {
            return Err(ProtocolError::Refused);
        }
        if finish_reason == FinishReason::ToolCalls && self.tools.is_empty() {
            self.failure_detail =
                Some("finish_reason tool_calls arrived without any tool calls".to_owned());
            return Err(ProtocolError::InconsistentFinishReason);
        }
        if self.choice == ToolChoice::Required && self.tools.is_empty() {
            return Err(ProtocolError::RequiredToolMissing);
        }
        for tool in &self.tools {
            if tool.id.is_none() {
                return Err(ProtocolError::InvalidToolCallId);
            }
            if !self.names.contains(&tool.name) {
                return Err(ProtocolError::InvalidToolName);
            }
            validate_arguments(&tool.arguments)?;
        }
        let tool_calls = self
            .tools
            .drain(..)
            .map(|tool| ToolCall {
                id: ToolCallId::new(tool.id.unwrap_or_default()),
                name: tool.name,
                arguments: tool.arguments,
            })
            .collect();
        let content = mem::take(&mut self.content);
        Ok(Completion {
            content: (!content.is_empty()).then_some(content),
            tool_calls,
            finish_reason,
            usage: Usage {
                input_tokens: self.usage.input,
                output_tokens: self.usage.output,
            },
            provider_replay: None,
        })
    }
}

fn extends_known_name(names: &[String], prefix: &str, fragment: &str) -> bool {
    names.iter().any(|known| {
        known
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(fragment))
    })
}

fn encoded_json_string_len(text: &str) -> usize {
    2 + text
        .bytes()
        .map(|byte| match byte {
            b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
            0..0x20 => 6,
            _ => 1,
        })
        .sum::<usize>()
}

fn encoded_json_len(value: &Json<'_>) -> usize {
    struct Counter(usize);
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    let _ = serde_json::to_writer(&mut counter, value);
    counter.0
}

fn accept_identity(
    destination: &mut Option<String>,
    value: Option<&Json<'_>>,
    max_bytes: usize,
) -> ProtocolResult<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let text = string(value)?;
    if text.is_empty() {
        return Ok(());
    }
    if text.len() > max_bytes {
        return Err(ProtocolError::IdentityTooLarge);
    }
    match destination {
        Some(prior) if prior != text => Err(ProtocolError::ConflictingIdentity),
        Some(_) => Ok(()),
        None => {
            *destination = Some(text.to_owned());
            Ok(())
        }
    }
}

fn object<'a, 'b>(value: &'a Json<'b>) -> ProtocolResult<&'a Object<'b>> {
    value.as_object().ok_or(ProtocolError::InvalidChunk)
}

fn string<'a>(value: &'a Json<'_>) -> ProtocolResult<&'a str> {
    value.as_str().ok_or(ProtocolError::InvalidChunk)
}

fn non_null<'a, 'b>(fields: &'a Object<'b>, key: &str) -> Option<&'a Json<'b>> {
    fields.get(key).filter(|value| !value.is_null())
}

fn index_value(value: &Json<'_>) -> ProtocolResult<usize> {
    value
        .as_i64()
        .and_then(|number| usize::try_from(number).ok())
        .ok_or(ProtocolError::InvalidChunk)
}

fn token_count(fields: &Object<'_>, key: &str) -> ProtocolResult<Option<u64>> {
    non_null(fields, key)
        .map(|value| {
            value
                .as_i64()
                .and_then(|number| u64::try_from(number).ok())
                .ok_or(ProtocolError::InvalidChunk)
        })
        .transpose()
}

fn append_bounded(
    destination: &mut String,
    text: &str,
    limit: usize,
    failure: ProtocolError,
) -> ProtocolResult<()> {
    if text.len() > limit.saturating_sub(destination.len()) {
        return Err(failure);
    }
    destination.push_str(text);
    Ok(())
}

fn compact(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests;
