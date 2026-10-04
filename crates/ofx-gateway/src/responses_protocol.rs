use std::collections::HashMap;

use ofx_contract::{
    ChatMessage, ModelFailureDiagnostic, ToolArgumentIntegrity, ToolCall, ToolCallId, ToolExecutionProvenance,
    ToolSpec, Usage,
};
use ofx_contract::{DuplicateKeys, Json, Object, parse_strict_json, parse_strict_json_value};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::chat_completions_protocol::capped_description;
use crate::tool_call_ids::{Projection, ProjectionError};

type IdentityHash = [u8; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ResponsesError {
    #[error("InvalidProviderPrompt")]
    InvalidProviderPrompt,
    #[error("InvalidProviderState")]
    InvalidProviderState,
    #[error("ProviderStateTooLarge")]
    ProviderStateTooLarge,
    #[error("ToolCallLimitExceeded")]
    ToolCallLimitExceeded,
    #[error("ToolArgumentsTooLarge")]
    ToolArgumentsTooLarge,
    #[error("InvalidToolArguments")]
    InvalidToolArguments,
    #[error("InvalidToolCallId")]
    InvalidToolCallId,
    #[error("ToolCallIdMappingExhausted")]
    ToolCallIdMappingExhausted,
    #[error("ProtectedToolCallId")]
    ProtectedToolCallId,
    #[error("InvalidToolSchema")]
    InvalidToolSchema,
    #[error("InvalidModel")]
    InvalidModel,
    #[error("EventTooLarge")]
    EventTooLarge,
    #[error("InvalidEvent")]
    InvalidEvent,
    #[error("StreamIncomplete")]
    StreamIncomplete,
    #[error("ResourceLimitExceeded")]
    ResourceLimitExceeded,
    #[error("ResponsesToolCallConflict")]
    ToolCallConflict,
    #[error("ResponsesTextConflict")]
    TextConflict,
    #[error("ResponsesReasoningConflict")]
    ReasoningConflict,
    #[error("ResponsesOutputItemConflict")]
    OutputItemConflict,
    #[error("Cancelled")]
    Cancelled,
}

impl From<ProjectionError> for ResponsesError {
    fn from(error: ProjectionError) -> Self {
        match error {
            ProjectionError::InvalidToolCallId => Self::InvalidToolCallId,
            ProjectionError::ToolCallIdMappingExhausted => Self::ToolCallIdMappingExhausted,
            ProjectionError::ProtectedToolCallId => Self::ProtectedToolCallId,
        }
    }
}

type Result<T> = std::result::Result<T, ResponsesError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rejection {
    pub(crate) error: ResponsesError,
    pub(crate) event_type: Option<String>,
}

impl From<ResponsesError> for Rejection {
    fn from(error: ResponsesError) -> Self {
        Self {
            error,
            event_type: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReplayLimits {
    pub(crate) tool_calls: usize,
    pub(crate) tool_identity_bytes: usize,
    pub(crate) tool_arguments_bytes: usize,
    pub(crate) provider_state_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamLimits {
    pub(crate) aggregate_bytes: usize,
    pub(crate) events: usize,
    pub(crate) tool_calls: usize,
    pub(crate) tool_identity_bytes: usize,
    pub(crate) tool_arguments_bytes: usize,
    pub(crate) provider_state_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssistantMessagePhase {
    Commentary,
    FinalAnswer,
}

impl AssistantMessagePhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Commentary => "commentary",
            Self::FinalAnswer => "final_answer",
        }
    }
}

fn assistant_message_phase(fields: &Object<'_>) -> Result<Option<AssistantMessagePhase>> {
    match fields.get("phase") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(phase)) => Ok(match phase.as_ref() {
            "commentary" => Some(AssistantMessagePhase::Commentary),
            "final_answer" => Some(AssistantMessagePhase::FinalAnswer),
            _ => None,
        }),
        Some(_) => Err(ResponsesError::InvalidEvent),
    }
}

pub(crate) fn select_replay_parts(
    parts: &str,
    max_bytes: usize,
    text: bool,
    reasoning: bool,
) -> Result<Option<String>> {
    if text && reasoning {
        return Ok(Some(parts.to_owned()));
    }
    if !text && !reasoning {
        return Ok(None);
    }
    if parts.len() > max_bytes {
        return Err(ResponsesError::ProviderStateTooLarge);
    }
    let Ok(Json::Array(items)) = parse_strict_json(parts.as_bytes(), DuplicateKeys::AfterValue)
    else {
        return Err(ResponsesError::InvalidProviderState);
    };
    let total = items.len();
    let mut kept = Vec::with_capacity(total);
    for item in items {
        let kind = item
            .as_object()
            .and_then(|fields| fields.get("type"))
            .and_then(Json::as_str);
        let keep = match kind {
            Some("reasoning") => reasoning,
            Some("message") => text,
            _ => return Err(ResponsesError::InvalidProviderState),
        };
        if keep {
            kept.push(item);
        }
    }
    if kept.is_empty() {
        return Ok(None);
    }
    if kept.len() == total {
        return Ok(Some(parts.to_owned()));
    }
    serde_json::to_string(&kept)
        .map(Some)
        .map_err(|_| ResponsesError::InvalidProviderState)
}

pub(crate) fn push_json_string(out: &mut String, text: &str) {
    out.push_str(&Value::String(text.to_owned()).to_string());
}

fn push_comma(out: &mut String, first: &mut bool) {
    if !*first {
        out.push(',');
    }
    *first = false;
}

pub(crate) fn write_input(
    out: &mut String,
    messages: &[ChatMessage],
    replays: &[Option<&str>],
    limits: ReplayLimits,
) -> Result<()> {
    for (message, replay) in messages.iter().zip(replays) {
        match message {
            ChatMessage::System { .. } => return Err(ResponsesError::InvalidProviderPrompt),
            ChatMessage::Assistant { tool_calls, .. } => {
                validate_replay_message(tool_calls, *replay, limits)?;
            }
            _ => {}
        }
    }
    let replayed: Vec<bool> = replays.iter().map(Option::is_some).collect();
    let ids = Projection::protecting(messages, &replayed)?;
    let mut first = true;
    for (message, replay) in messages.iter().zip(replays) {
        match message {
            ChatMessage::System { .. } => {}
            ChatMessage::User { content, .. } => {
                push_comma(out, &mut first);
                out.push_str("{\"role\":\"user\",\"content\":[");
                if !content.is_empty() {
                    out.push_str("{\"type\":\"input_text\",\"text\":");
                    push_json_string(out, content);
                    out.push('}');
                }
                out.push_str("]}");
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                write_assistant(
                    out,
                    &mut first,
                    content.as_deref().unwrap_or_default(),
                    *replay,
                )?;
                for call in tool_calls {
                    push_comma(out, &mut first);
                    out.push_str("{\"type\":\"function_call\",\"call_id\":");
                    push_json_string(out, ids.resolve(call.id.as_str()));
                    out.push_str(",\"name\":");
                    push_json_string(out, &call.name);
                    out.push_str(",\"arguments\":");
                    push_json_string(out, &call.arguments);
                    out.push('}');
                }
            }
            ChatMessage::Tool {
                call_id, content, ..
            } => {
                push_comma(out, &mut first);
                out.push_str("{\"type\":\"function_call_output\",\"call_id\":");
                push_json_string(out, ids.resolve(call_id.as_str()));
                out.push_str(",\"output\":");
                push_json_string(out, content);
                out.push('}');
            }
        }
    }
    Ok(())
}

fn write_assistant(
    out: &mut String,
    first: &mut bool,
    content: &str,
    replay: Option<&str>,
) -> Result<()> {
    let mut legacy_phase = None;
    let mut span_end: Option<usize> = None;
    if let Some(parts) = replay {
        let Ok(Json::Array(items)) = parse_strict_json(parts.as_bytes(), DuplicateKeys::AfterValue)
        else {
            return Err(ResponsesError::InvalidProviderState);
        };
        for item in &items {
            let Json::Object(fields) = item else {
                return Err(ResponsesError::InvalidProviderState);
            };
            match fields.get("type").and_then(Json::as_str) {
                Some("message") => {
                    let phase = assistant_message_phase(fields)
                        .map_err(|_| ResponsesError::InvalidProviderState)?;
                    if fields.contains_key("offset") || fields.contains_key("length") {
                        if legacy_phase.is_some() {
                            return Err(ResponsesError::InvalidProviderState);
                        }
                        let end = replay_span(content, fields, span_end)?;
                        write_assistant_text(out, first, &content[end.0..end.1], phase);
                        span_end = Some(end.1);
                    } else {
                        let Some(phase) = phase.filter(|_| span_end.is_none()) else {
                            return Err(ResponsesError::InvalidProviderState);
                        };
                        if legacy_phase.is_some_and(|prior| prior != phase) {
                            return Err(ResponsesError::InvalidProviderState);
                        }
                        legacy_phase = Some(phase);
                    }
                }
                Some("reasoning") => {
                    push_comma(out, first);
                    let item = serde_json::to_string(item)
                        .map_err(|_| ResponsesError::InvalidProviderState)?;
                    out.push_str(&item);
                }
                _ => return Err(ResponsesError::InvalidProviderState),
            }
        }
    }
    match span_end {
        Some(end) => {
            let tail = &content[end..];
            if tail.len() > 2 || !"\n\n".starts_with(tail) {
                return Err(ResponsesError::InvalidProviderState);
            }
        }
        None if !content.is_empty() => write_assistant_text(out, first, content, legacy_phase),
        None => {}
    }
    Ok(())
}

fn replay_span(
    content: &str,
    fields: &Object<'_>,
    span_end: Option<usize>,
) -> Result<(usize, usize)> {
    let invalid = ResponsesError::InvalidProviderState;
    let index = |name: &str| {
        fields
            .get(name)
            .and_then(Json::as_u64)
            .filter(|_| fields.get(name).is_some_and(Json::is_u64))
            .and_then(|value| usize::try_from(value).ok())
    };
    let (Some(offset), Some(length)) = (index("offset"), index("length")) else {
        return Err(invalid);
    };
    if offset > content.len() || length == 0 || length > content.len() - offset {
        return Err(invalid);
    }
    match span_end {
        Some(end) if offset < end || content.get(end..offset) != Some("\n\n") => {
            return Err(invalid);
        }
        None if offset != 0 => return Err(invalid),
        _ => {}
    }
    let end = offset + length;
    if content.get(offset..end).is_none() {
        return Err(invalid);
    }
    Ok((offset, end))
}

fn write_assistant_text(
    out: &mut String,
    first: &mut bool,
    content: &str,
    phase: Option<AssistantMessagePhase>,
) {
    push_comma(out, first);
    out.push_str(
        "{\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":",
    );
    push_json_string(out, content);
    out.push_str(",\"annotations\":[]}]");
    if let Some(phase) = phase {
        out.push_str(",\"phase\":");
        push_json_string(out, phase.label());
    }
    out.push('}');
}

fn validate_replay_message(
    tool_calls: &[ToolCall],
    replay: Option<&str>,
    limits: ReplayLimits,
) -> Result<()> {
    if replay.is_some_and(|parts| parts.len() > limits.provider_state_bytes) {
        return Err(ResponsesError::ProviderStateTooLarge);
    }
    if tool_calls.len() > limits.tool_calls {
        return Err(ResponsesError::ToolCallLimitExceeded);
    }
    for call in tool_calls {
        let id = call.id.as_str();
        if id.is_empty()
            || id.len() > limits.tool_identity_bytes
            || call.name.is_empty()
            || call.name.len() > limits.tool_identity_bytes
        {
            return Err(ResponsesError::ToolCallLimitExceeded);
        }
        if call.arguments.len() > limits.tool_arguments_bytes {
            return Err(ResponsesError::ToolArgumentsTooLarge);
        }
        if call.provenance != ToolExecutionProvenance::ProviderExecuted
            && ToolArgumentIntegrity::classify_function_input(&call.arguments)
                != ToolArgumentIntegrity::Valid
        {
            return Err(ResponsesError::InvalidToolArguments);
        }
    }
    Ok(())
}

pub(crate) fn write_tools(out: &mut String, tools: &[ToolSpec]) -> Result<usize> {
    let mut written: Vec<&str> = Vec::with_capacity(tools.len());
    let mut list = String::from(",\"tools\":[");
    for tool in tools {
        if written.contains(&tool.name.as_str()) {
            continue;
        }
        if !written.is_empty() {
            list.push(',');
        }
        write_function_tool(&mut list, tool)?;
        written.push(&tool.name);
    }
    list.push(']');
    if !written.is_empty() {
        out.push_str(&list);
    }
    Ok(written.len())
}

fn write_function_tool(out: &mut String, tool: &ToolSpec) -> Result<()> {
    if tool.name.is_empty() {
        return Err(ResponsesError::InvalidToolSchema);
    }
    out.push_str("{\"type\":\"function\",\"name\":");
    push_json_string(out, &tool.name);
    if !tool.description.is_empty() {
        out.push_str(",\"description\":");
        push_json_string(out, &capped_description(&tool.description));
    }
    out.push_str(",\"parameters\":");
    out.push_str(&tool.input_schema);
    out.push_str(",\"strict\":false}");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponsesFinish {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    ProviderError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureCause {
    Retryable,
    RateLimited,
    NonRetryable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderFailure {
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) cause: FailureCause,
}

impl ProviderFailure {
    pub(crate) fn detail(self, mask: impl Fn(String) -> String) -> String {
        let text = format!(
            "{}: {}",
            ModelFailureDiagnostic::new(&mask(self.code)).as_str(),
            ModelFailureDiagnostic::new(&mask(self.message)).as_str()
        );
        ModelFailureDiagnostic::new(&text).as_str().to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delta {
    Text(String),
    Reasoning(String),
    ToolCallStarted {
        call_id: ToolCallId,
        tool_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResponsesCompletion {
    pub(crate) content: Option<String>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) provider_state: Option<String>,
    pub(crate) finish: ResponsesFinish,
    pub(crate) failure: Option<ProviderFailure>,
    pub(crate) usage: Usage,
}

#[derive(Debug)]
struct ToolAccumulator {
    output_index: i64,
    id: String,
    name: String,
    item_id: Option<String>,
    arguments: String,
    arguments_finalized: bool,
}

impl ToolAccumulator {
    fn reconcile_identity(
        &mut self,
        fields: &Object<'_>,
        item_id_key: &str,
        limits: StreamLimits,
    ) -> Result<()> {
        check_optional_identity(fields, "call_id", &self.id)?;
        check_optional_identity(fields, "name", &self.name)?;
        if let Some(value) = fields.get(item_id_key) {
            let Some(item_id) = value.as_str().filter(|item_id| !item_id.is_empty()) else {
                return Err(ResponsesError::InvalidEvent);
            };
            if item_id.len() > limits.tool_identity_bytes {
                return Err(ResponsesError::ToolCallLimitExceeded);
            }
            match &self.item_id {
                Some(known) if known != item_id => return Err(ResponsesError::ToolCallConflict),
                Some(_) => {}
                None => self.item_id = Some(item_id.to_owned()),
            }
        }
        Ok(())
    }

    fn finalize_arguments(&mut self, arguments: &str, limits: StreamLimits) -> Result<()> {
        if arguments.len() > limits.tool_arguments_bytes {
            return Err(ResponsesError::ToolArgumentsTooLarge);
        }
        if self.arguments_finalized {
            if !serialized_equal(&self.arguments, arguments) {
                return Err(ResponsesError::ToolCallConflict);
            }
            return Ok(());
        }
        if let Some(suffix) = arguments.strip_prefix(self.arguments.as_str()) {
            append_tool_arguments(&mut self.arguments, suffix, limits.tool_arguments_bytes)?;
        } else {
            arguments.clone_into(&mut self.arguments);
        }
        self.arguments_finalized = true;
        Ok(())
    }
}

fn string_member<'a>(fields: &'a Object<'_>, key: &str) -> Option<&'a str> {
    fields.get(key).and_then(Json::as_str)
}

fn error_event_failure<'a>(event: &'a Object<'_>) -> (Option<&'a str>, Option<&'a str>) {
    let code = string_member(event, "code");
    let message = string_member(event, "message");
    match event.get("error") {
        Some(Json::Object(error)) if code.is_none() && message.is_none() => (
            string_member(error, "code").or_else(|| string_member(error, "type")),
            string_member(error, "message"),
        ),
        _ => (code, message),
    }
}

fn check_optional_identity(fields: &Object<'_>, key: &str, expected: &str) -> Result<()> {
    match fields.get(key) {
        None => Ok(()),
        Some(Json::String(value)) if value == expected => Ok(()),
        Some(Json::String(_)) => Err(ResponsesError::ToolCallConflict),
        Some(_) => Err(ResponsesError::InvalidEvent),
    }
}

fn serialized_equal(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (
        parse_strict_json_value(left.as_bytes()),
        parse_strict_json_value(right.as_bytes()),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn reasoning_digest(fields: &Object<'_>) -> IdentityHash {
    let mut canonical = String::new();
    write_canonical_object(
        &mut canonical,
        fields.iter().filter(|(key, _)| *key != "encrypted_content"),
    );
    Sha256::digest(canonical.as_bytes()).into()
}

fn write_canonical(out: &mut String, value: &Json<'_>) {
    match value {
        Json::Object(fields) => write_canonical_object(out, fields.iter()),
        Json::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Json::Null => out.push_str("null"),
        Json::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Json::Number(number) => out.push_str(&number.to_string()),
        Json::String(text) => push_json_string(out, text),
    }
}

fn write_canonical_object<'a, 'b: 'a>(
    out: &mut String,
    fields: impl Iterator<Item = (&'a str, &'a Json<'b>)>,
) {
    let mut sorted: Vec<_> = fields.collect();
    sorted.sort_unstable_by(|left, right| left.0.cmp(right.0));
    out.push('{');
    for (index, (key, value)) in sorted.into_iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_json_string(out, key);
        out.push(':');
        write_canonical(out, value);
    }
    out.push('}');
}

fn optional_index(fields: &Object<'_>, name: &str) -> Result<Option<i64>> {
    match fields.get(name) {
        None => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|index| value.is_i64() && *index >= 0)
            .map(Some)
            .ok_or(ResponsesError::InvalidEvent),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TextKey {
    output_index: i64,
    content_index: i64,
}

impl TextKey {
    fn precedes(self, other: Self) -> bool {
        (self.output_index, self.content_index) < (other.output_index, other.content_index)
    }
}

fn text_key(fields: &Object<'_>) -> Result<TextKey> {
    Ok(TextKey {
        output_index: optional_index(fields, "output_index")?.unwrap_or(0),
        content_index: optional_index(fields, "content_index")?.unwrap_or(0),
    })
}

fn text_identity(fields: &Object<'_>, name: &str) -> Result<Option<IdentityHash>> {
    match fields.get(name) {
        None => Ok(None),
        Some(Json::String(value)) if !value.is_empty() => {
            Ok(Some(Sha256::digest(value.as_bytes()).into()))
        }
        Some(_) => Err(ResponsesError::InvalidEvent),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Text,
    Refusal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextMode {
    Delta,
    Final,
}

struct TextUpdate<'a> {
    key: TextKey,
    kind: TextKind,
    item_id_hash: Option<IdentityHash>,
    text: &'a str,
    mode: TextMode,
}

#[derive(Debug)]
struct TextPart {
    kind: TextKind,
    start: usize,
    received: usize,
    finalized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    FunctionCall,
    Reasoning,
    Message,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evidence {
    Identity,
    Completed,
}

#[derive(Debug)]
struct MessageItem {
    output_index: i64,
    id_hash: Option<IdentityHash>,
    phase: Option<AssistantMessagePhase>,
    offset: usize,
    length: usize,
}

impl MessageItem {
    fn replay_json(&self) -> String {
        let mut json = format!(
            "{{\"type\":\"message\",\"offset\":{},\"length\":{}",
            self.offset, self.length
        );
        if let Some(phase) = self.phase {
            json.push_str(",\"phase\":");
            push_json_string(&mut json, phase.label());
        }
        json.push('}');
        json
    }
}

#[derive(Debug)]
struct ReasoningItem {
    output_index: i64,
    id_hash: Option<IdentityHash>,
    json: Option<String>,
    completed_digest: Option<IdentityHash>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalStatus {
    Completed,
    Incomplete,
    Failed,
    Cancelled,
}

#[derive(Debug)]
pub(crate) struct Reducer {
    limits: StreamLimits,
    content: String,
    message_items: Vec<MessageItem>,
    reasoning_items: Vec<ReasoningItem>,
    reasoning_bytes: usize,
    tools: Vec<ToolAccumulator>,
    finish: Option<ResponsesFinish>,
    usage: Usage,
    failure: Option<ProviderFailure>,
    terminal_seen: bool,
    text_parts: HashMap<TextKey, TextPart>,
    last_text_key: Option<TextKey>,
    text_bytes: usize,
    event_count: usize,
    aggregate_bytes: usize,
    deltas: Vec<Delta>,
}

impl Reducer {
    pub(crate) fn new(limits: StreamLimits) -> Self {
        Self {
            limits,
            content: String::new(),
            message_items: Vec::new(),
            reasoning_items: Vec::new(),
            reasoning_bytes: 0,
            tools: Vec::new(),
            finish: None,
            usage: Usage::default(),
            failure: None,
            terminal_seen: false,
            text_parts: HashMap::new(),
            last_text_key: None,
            text_bytes: 0,
            event_count: 0,
            aggregate_bytes: 0,
            deltas: Vec::new(),
        }
    }

    pub(crate) fn apply(
        &mut self,
        json: &[u8],
        cancelled: bool,
    ) -> (std::result::Result<bool, Rejection>, Vec<Delta>) {
        let terminal = self.apply_event(json, cancelled);
        (terminal, std::mem::take(&mut self.deltas))
    }

    fn apply_event(
        &mut self,
        json: &[u8],
        cancelled: bool,
    ) -> std::result::Result<bool, Rejection> {
        if cancelled {
            return Err(ResponsesError::Cancelled.into());
        }
        if self.terminal_seen {
            return Ok(true);
        }
        self.event_count = checked_accumulated_size(self.event_count, 1, self.limits.events)?;
        self.aggregate_bytes = checked_accumulated_size(
            self.aggregate_bytes,
            json.len(),
            self.limits.aggregate_bytes,
        )?;
        let parsed = parse_strict_json(json, DuplicateKeys::AfterValue)
            .map_err(|_| ResponsesError::InvalidEvent)?;
        let Json::Object(event) = parsed else {
            return Ok(false);
        };
        let Some(event_type) = event.get("type").and_then(Json::as_str) else {
            return Ok(false);
        };
        self.dispatch(event_type, &event)
            .map_err(|error| Rejection {
                error,
                event_type: Some(event_type.to_owned()),
            })
    }

    fn dispatch(&mut self, event_type: &str, event: &Object<'_>) -> Result<bool> {
        match event_type {
            "response.output_item.added" => self.item_added(event)?,
            "response.output_text.delta" | "response.refusal.delta" => {
                let text = event
                    .get("delta")
                    .and_then(Json::as_str)
                    .ok_or(ResponsesError::InvalidEvent)?;
                self.accept_text(&TextUpdate {
                    key: text_key(event)?,
                    kind: text_kind(event_type == "response.refusal.delta"),
                    item_id_hash: text_identity(event, "item_id")?,
                    text,
                    mode: TextMode::Delta,
                })?;
            }
            "response.output_text.done" | "response.refusal.done" => {
                let refusal = event_type == "response.refusal.done";
                let text = event
                    .get(if refusal { "refusal" } else { "text" })
                    .and_then(Json::as_str)
                    .ok_or(ResponsesError::InvalidEvent)?;
                self.accept_text(&TextUpdate {
                    key: text_key(event)?,
                    kind: text_kind(refusal),
                    item_id_hash: text_identity(event, "item_id")?,
                    text,
                    mode: TextMode::Final,
                })?;
            }
            "response.content_part.done" => {
                let Some(Json::Object(part)) = event.get("part") else {
                    return Err(ResponsesError::InvalidEvent);
                };
                self.finalize_text_part(text_key(event)?, text_identity(event, "item_id")?, part)?;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(index) = optional_index(event, "output_index")? {
                    self.check_output_kind(index, OutputKind::Reasoning)?;
                }
                if let Some(delta) = event.get("delta").and_then(Json::as_str) {
                    self.deltas.push(Delta::Reasoning(delta.to_owned()));
                }
            }
            "response.reasoning_summary_part.done" => {
                if let Some(index) = optional_index(event, "output_index")? {
                    self.check_output_kind(index, OutputKind::Reasoning)?;
                }
                self.deltas.push(Delta::Reasoning("\n\n".to_owned()));
            }
            "response.function_call_arguments.delta" => self.arguments_delta(event)?,
            "response.function_call_arguments.done" => self.arguments_done(event)?,
            "response.output_item.done" => self.item_done(event)?,
            "response.completed" | "response.done" | "response.incomplete" | "response.failed" => {
                self.terminal(event_type, event)?;
                return Ok(true);
            }
            "error" => {
                let (code, message) = error_event_failure(event);
                self.accept_failure(code, message);
                self.terminal_seen = true;
                self.finish = Some(ResponsesFinish::ProviderError);
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn item_added(&mut self, event: &Object<'_>) -> Result<()> {
        let Some(output_index) = optional_index(event, "output_index")? else {
            return Ok(());
        };
        let Some(Json::Object(item)) = event.get("item") else {
            return Ok(());
        };
        match item.get("type").and_then(Json::as_str) {
            Some("function_call") => {
                self.check_output_kind(output_index, OutputKind::FunctionCall)?;
                let (Some(call_id), Some(name)) = (
                    item.get("call_id").and_then(Json::as_str),
                    item.get("name").and_then(Json::as_str),
                ) else {
                    return Ok(());
                };
                let limits = self.limits;
                if let Some(index) = self.find_tool(output_index) {
                    return self.tools[index].reconcile_identity(item, "id", limits);
                }
                self.append_tool(output_index, call_id, name)?;
                let tool = self
                    .tools
                    .last_mut()
                    .ok_or(ResponsesError::ToolCallConflict)?;
                tool.reconcile_identity(item, "id", limits)?;
                if let Some(arguments) = item.get("arguments") {
                    let arguments = arguments.as_str().ok_or(ResponsesError::InvalidEvent)?;
                    append_tool_arguments(
                        &mut tool.arguments,
                        arguments,
                        limits.tool_arguments_bytes,
                    )?;
                }
                self.deltas.push(Delta::ToolCallStarted {
                    call_id: ToolCallId::new(call_id),
                    tool_name: name.to_owned(),
                });
                Ok(())
            }
            Some("reasoning") => self.reconcile_reasoning(output_index, item, Evidence::Identity),
            Some("message") => self
                .reconcile_message(
                    output_index,
                    text_identity(item, "id")?,
                    assistant_message_phase(item)?,
                )
                .map(|_| ()),
            Some(_) => self.check_output_kind(output_index, OutputKind::Unknown),
            None => Ok(()),
        }
    }

    fn arguments_delta(&mut self, event: &Object<'_>) -> Result<()> {
        let Some(output_index) = optional_index(event, "output_index")? else {
            return Ok(());
        };
        self.check_output_kind(output_index, OutputKind::FunctionCall)?;
        let Some(delta) = event.get("delta").and_then(Json::as_str) else {
            return Ok(());
        };
        let Some(index) = self.find_tool(output_index) else {
            return Ok(());
        };
        let limits = self.limits;
        let tool = &mut self.tools[index];
        tool.reconcile_identity(event, "item_id", limits)?;
        if tool.arguments_finalized && !delta.is_empty() {
            return Err(ResponsesError::ToolCallConflict);
        }
        append_tool_arguments(&mut tool.arguments, delta, limits.tool_arguments_bytes)
    }

    fn arguments_done(&mut self, event: &Object<'_>) -> Result<()> {
        let Some(output_index) = optional_index(event, "output_index")? else {
            return Ok(());
        };
        self.check_output_kind(output_index, OutputKind::FunctionCall)?;
        let arguments = event
            .get("arguments")
            .and_then(Json::as_str)
            .ok_or(ResponsesError::InvalidEvent)?;
        let index = self
            .find_tool(output_index)
            .ok_or(ResponsesError::ToolCallConflict)?;
        let limits = self.limits;
        let tool = &mut self.tools[index];
        tool.reconcile_identity(event, "item_id", limits)?;
        tool.finalize_arguments(arguments, limits)
    }

    fn item_done(&mut self, event: &Object<'_>) -> Result<()> {
        let Some(output_index) = optional_index(event, "output_index")? else {
            return Ok(());
        };
        let Some(Json::Object(item)) = event.get("item") else {
            return Ok(());
        };
        self.reconcile_output_item(output_index, item)
    }

    fn reconcile_output_item(&mut self, output_index: i64, item: &Object<'_>) -> Result<()> {
        match item.get("type").and_then(Json::as_str) {
            Some("function_call") => self.reconcile_tool_item(output_index, item),
            Some("reasoning") => self.reconcile_reasoning(output_index, item, Evidence::Completed),
            Some("message") => self.finalize_text_message(output_index, item),
            Some(_) => self.check_output_kind(output_index, OutputKind::Unknown),
            None => Ok(()),
        }
    }

    fn terminal(&mut self, event_type: &str, event: &Object<'_>) -> Result<()> {
        let Some(Json::Object(response)) = event.get("response") else {
            return Err(ResponsesError::InvalidEvent);
        };
        let status = terminal_status(event_type, response)?;
        let output = response.get("output").unwrap_or(&Json::Null);
        if status == TerminalStatus::Failed {
            let (code, message) = match response.get("error") {
                Some(Json::Object(failure)) => (
                    string_member(failure, "code"),
                    string_member(failure, "message"),
                ),
                _ => (None, None),
            };
            self.accept_failure(code, message);
        } else if !output.is_null() {
            let Json::Array(items) = output else {
                return Err(ResponsesError::InvalidEvent);
            };
            for (position, item) in items.iter().enumerate() {
                let Json::Object(item) = item else {
                    continue;
                };
                if item.get("type").and_then(Json::as_str).is_none() {
                    continue;
                }
                let index =
                    i64::try_from(position).map_err(|_| ResponsesError::ResourceLimitExceeded)?;
                self.reconcile_output_item(index, item)?;
            }
        }
        self.terminal_seen = true;
        self.finish = Some(finish_reason(status, response, !self.tools.is_empty()));
        self.usage = parse_usage(response);
        Ok(())
    }

    fn check_output_kind(&self, output_index: i64, kind: OutputKind) -> Result<()> {
        let conflict = (kind != OutputKind::FunctionCall && self.find_tool(output_index).is_some())
            || (kind != OutputKind::Reasoning
                && self
                    .reasoning_items
                    .iter()
                    .any(|item| item.output_index == output_index))
            || (kind != OutputKind::Message
                && self
                    .message_items
                    .iter()
                    .any(|item| item.output_index == output_index));
        if conflict {
            Err(ResponsesError::OutputItemConflict)
        } else {
            Ok(())
        }
    }

    fn reconcile_reasoning(
        &mut self,
        output_index: i64,
        fields: &Object<'_>,
        evidence: Evidence,
    ) -> Result<()> {
        self.check_output_kind(output_index, OutputKind::Reasoning)?;
        let id_hash = text_identity(fields, "id")?;
        let position = self
            .reasoning_items
            .partition_point(|item| item.output_index < output_index);
        let found = self
            .reasoning_items
            .get(position)
            .is_some_and(|item| item.output_index == output_index);
        if let Some(id) = id_hash {
            for prior in &self.reasoning_items {
                let Some(prior_id) = prior.id_hash else {
                    continue;
                };
                let same_id = prior_id == id;
                if (prior.output_index == output_index) != same_id {
                    return Err(ResponsesError::ReasoningConflict);
                }
            }
        }
        let encrypted = fields.get("encrypted_content").unwrap_or(&Json::Null);
        if !encrypted.is_null() && !encrypted.is_string() {
            return Err(ResponsesError::InvalidEvent);
        }
        let completed_digest = (evidence == Evidence::Completed).then(|| reasoning_digest(fields));
        if found {
            let prior = &mut self.reasoning_items[position];
            if let (Some(prior_digest), Some(digest)) = (prior.completed_digest, completed_digest)
                && prior_digest != digest
            {
                return Err(ResponsesError::ReasoningConflict);
            }
            if prior.json.is_some() {
                if id_hash.is_some() {
                    prior.id_hash = id_hash;
                }
                return Ok(());
            }
        }
        let has_ciphertext = encrypted.as_str().is_some_and(|text| !text.is_empty());
        let json = (evidence == Evidence::Completed && has_ciphertext)
            .then(|| serde_json::to_string(fields))
            .transpose()
            .map_err(|_| ResponsesError::InvalidEvent)?;
        let mut total = self.reasoning_bytes;
        if let Some(json) = &json {
            let overhead = if total == 0 { 2 } else { 1 };
            let size =
                checked_accumulated_size(json.len(), overhead, self.limits.provider_state_bytes)?;
            total = checked_accumulated_size(total, size, self.limits.provider_state_bytes)?;
        }
        if found {
            let prior = &mut self.reasoning_items[position];
            if id_hash.is_some() {
                prior.id_hash = id_hash;
            }
            prior.json = json;
            prior.completed_digest = prior.completed_digest.or(completed_digest);
        } else {
            if self.reasoning_items.len() >= self.limits.events {
                return Err(ResponsesError::ResourceLimitExceeded);
            }
            self.reasoning_items.insert(
                position,
                ReasoningItem {
                    output_index,
                    id_hash,
                    json,
                    completed_digest,
                },
            );
        }
        self.reasoning_bytes = total;
        Ok(())
    }

    fn reconcile_message(
        &mut self,
        output_index: i64,
        id_hash: Option<IdentityHash>,
        phase: Option<AssistantMessagePhase>,
    ) -> Result<usize> {
        self.check_output_kind(output_index, OutputKind::Message)?;
        let position = self
            .message_items
            .partition_point(|item| item.output_index < output_index);
        if let Some(id) = id_hash {
            for prior in &self.message_items {
                let Some(prior_id) = prior.id_hash else {
                    continue;
                };
                if (prior.output_index == output_index) != (prior_id == id) {
                    return Err(ResponsesError::TextConflict);
                }
            }
        }
        if let Some(prior) = self
            .message_items
            .get_mut(position)
            .filter(|item| item.output_index == output_index)
        {
            if let Some(phase) = phase {
                if prior.phase.is_some_and(|old| old != phase) {
                    return Err(ResponsesError::TextConflict);
                }
                prior.phase = Some(phase);
            }
            if id_hash.is_some() {
                prior.id_hash = id_hash;
            }
            return Ok(position);
        }
        if self.message_items.len() >= self.limits.events {
            return Err(ResponsesError::ResourceLimitExceeded);
        }
        self.message_items.insert(
            position,
            MessageItem {
                output_index,
                id_hash,
                phase,
                offset: 0,
                length: 0,
            },
        );
        Ok(position)
    }

    fn accept_failure(&mut self, code: Option<&str>, message: Option<&str>) {
        let code = code.unwrap_or("provider_error");
        let message = message.unwrap_or("Provider response failed");
        let cause = match code {
            "server_error" => FailureCause::Retryable,
            "rate_limit_exceeded" => FailureCause::RateLimited,
            _ => FailureCause::NonRetryable,
        };
        self.failure = Some(ProviderFailure {
            code: code.to_owned(),
            message: message.to_owned(),
            cause,
        });
    }

    fn accept_text(&mut self, update: &TextUpdate<'_>) -> Result<()> {
        let message = self.reconcile_message(update.key.output_index, update.item_id_hash, None)?;
        if self.text_parts.len() >= self.limits.events && !self.text_parts.contains_key(&update.key)
        {
            return Err(ResponsesError::ResourceLimitExceeded);
        }
        let part = self.text_parts.entry(update.key).or_insert(TextPart {
            kind: update.kind,
            start: 0,
            received: 0,
            finalized: false,
        });
        if part.kind != update.kind {
            return Err(ResponsesError::TextConflict);
        }
        let suffix = match update.mode {
            TextMode::Final => final_suffix(part, &self.content, update.text)?,
            TextMode::Delta if part.finalized && !update.text.is_empty() => {
                return Err(ResponsesError::TextConflict);
            }
            TextMode::Delta => update.text,
        };
        if !suffix.is_empty() {
            if self
                .last_text_key
                .is_some_and(|last| update.key.precedes(last))
            {
                return Err(ResponsesError::TextConflict);
            }
            let boundary = self
                .last_text_key
                .is_some_and(|last| last.output_index != update.key.output_index);
            let with_boundary = checked_accumulated_size(
                self.text_bytes,
                if boundary { 2 } else { 0 },
                self.limits.aggregate_bytes,
            )?;
            let total =
                checked_accumulated_size(with_boundary, suffix.len(), self.limits.aggregate_bytes)?;
            if boundary {
                self.content.push_str("\n\n");
                self.deltas.push(Delta::Text("\n\n".to_owned()));
            }
            let before = self.content.len();
            self.content.push_str(suffix);
            let item = &mut self.message_items[message];
            if item.length == 0 {
                item.offset = before;
            }
            item.length += suffix.len();
            if part.received == 0 {
                part.start = before;
            }
            part.received += suffix.len();
            self.text_bytes = total;
            self.last_text_key = Some(update.key);
            self.deltas.push(Delta::Text(suffix.to_owned()));
        }
        if update.mode == TextMode::Final {
            part.finalized = true;
        }
        Ok(())
    }

    fn finalize_text_part(
        &mut self,
        key: TextKey,
        item_id_hash: Option<IdentityHash>,
        fields: &Object<'_>,
    ) -> Result<()> {
        let kind = fields
            .get("type")
            .and_then(Json::as_str)
            .ok_or(ResponsesError::InvalidEvent)?;
        let refusal = kind == "refusal";
        if !refusal && kind != "output_text" {
            return Ok(());
        }
        let text = fields
            .get(if refusal { "refusal" } else { "text" })
            .and_then(Json::as_str)
            .ok_or(ResponsesError::InvalidEvent)?;
        self.accept_text(&TextUpdate {
            key,
            kind: text_kind(refusal),
            item_id_hash,
            text,
            mode: TextMode::Final,
        })
    }

    fn finalize_text_message(&mut self, output_index: i64, fields: &Object<'_>) -> Result<()> {
        let identity = text_identity(fields, "id")?;
        self.reconcile_message(output_index, identity, assistant_message_phase(fields)?)?;
        let Some(Json::Array(parts)) = fields.get("content") else {
            return Err(ResponsesError::InvalidEvent);
        };
        for (position, part) in parts.iter().enumerate() {
            let Json::Object(part) = part else {
                return Err(ResponsesError::InvalidEvent);
            };
            let key = TextKey {
                output_index,
                content_index: i64::try_from(position)
                    .map_err(|_| ResponsesError::ResourceLimitExceeded)?,
            };
            self.finalize_text_part(key, identity, part)?;
        }
        Ok(())
    }

    fn reconcile_tool_item(&mut self, output_index: i64, fields: &Object<'_>) -> Result<()> {
        self.check_output_kind(output_index, OutputKind::FunctionCall)?;
        let index = self
            .find_tool(output_index)
            .ok_or(ResponsesError::ToolCallConflict)?;
        let limits = self.limits;
        let tool = &mut self.tools[index];
        tool.reconcile_identity(fields, "id", limits)?;
        if let Some(arguments) = fields.get("arguments") {
            let arguments = arguments.as_str().ok_or(ResponsesError::InvalidEvent)?;
            tool.finalize_arguments(arguments, limits)?;
        }
        Ok(())
    }

    fn find_tool(&self, output_index: i64) -> Option<usize> {
        self.tools
            .iter()
            .position(|tool| tool.output_index == output_index)
    }

    fn append_tool(&mut self, output_index: i64, call_id: &str, name: &str) -> Result<()> {
        let limits = self.limits;
        if self.tools.len() >= limits.tool_calls
            || call_id.is_empty()
            || call_id.len() > limits.tool_identity_bytes
            || name.is_empty()
            || name.len() > limits.tool_identity_bytes
        {
            return Err(ResponsesError::ToolCallLimitExceeded);
        }
        self.tools.push(ToolAccumulator {
            output_index,
            id: call_id.to_owned(),
            name: name.to_owned(),
            item_id: None,
            arguments: String::new(),
            arguments_finalized: false,
        });
        Ok(())
    }

    pub(crate) fn finish(self, cancelled: bool) -> Result<ResponsesCompletion> {
        if cancelled {
            return Err(ResponsesError::Cancelled);
        }
        if !self.terminal_seen {
            return Err(ResponsesError::StreamIncomplete);
        }
        let provider_state = self.provider_state()?;
        let tool_calls: Vec<ToolCall> = self
            .tools
            .into_iter()
            .map(|tool| {
                let arguments = if tool.arguments_finalized || !tool.arguments.is_empty() {
                    tool.arguments
                } else {
                    "{}".to_owned()
                };
                ToolCall::new(tool.id, tool.name, arguments)
            })
            .collect();
        let finish = self.finish.unwrap_or(if tool_calls.is_empty() {
            ResponsesFinish::Stop
        } else {
            ResponsesFinish::ToolCalls
        });
        Ok(ResponsesCompletion {
            content: (!self.content.is_empty()).then_some(self.content),
            tool_calls,
            provider_state,
            finish,
            failure: self.failure,
            usage: self.usage,
        })
    }

    fn provider_state(&self) -> Result<Option<String>> {
        let maximum = self.limits.provider_state_bytes;
        let mut out = String::new();
        let mut reasoning = self.reasoning_items.iter().peekable();
        for message in &self.message_items {
            while let Some(item) =
                reasoning.next_if(|item| item.output_index <= message.output_index)
            {
                if let Some(json) = &item.json {
                    append_replay_item(&mut out, json, maximum)?;
                }
            }
            if message.length == 0 || (self.message_items.len() == 1 && message.phase.is_none()) {
                continue;
            }
            append_replay_item(&mut out, &message.replay_json(), maximum)?;
        }
        for item in reasoning {
            if let Some(json) = &item.json {
                append_replay_item(&mut out, json, maximum)?;
            }
        }
        if out.is_empty() {
            return Ok(None);
        }
        out.push(']');
        Ok(Some(out))
    }
}

fn final_suffix<'a>(part: &TextPart, content: &str, text: &'a str) -> Result<&'a str> {
    if text.len() < part.received || (part.finalized && text.len() != part.received) {
        return Err(ResponsesError::TextConflict);
    }
    let received = &content.as_bytes()[part.start..part.start + part.received];
    if &text.as_bytes()[..part.received] != received {
        return Err(ResponsesError::TextConflict);
    }
    text.get(part.received..)
        .ok_or(ResponsesError::TextConflict)
}

const fn text_kind(refusal: bool) -> TextKind {
    if refusal {
        TextKind::Refusal
    } else {
        TextKind::Text
    }
}

fn append_replay_item(out: &mut String, json: &str, maximum: usize) -> Result<()> {
    let item_size = checked_accumulated_size(json.len(), 2, maximum)?;
    checked_accumulated_size(out.len(), item_size, maximum)?;
    out.push(if out.is_empty() { '[' } else { ',' });
    out.push_str(json);
    Ok(())
}

fn append_tool_arguments(arguments: &mut String, delta: &str, maximum: usize) -> Result<()> {
    checked_accumulated_size(arguments.len(), delta.len(), maximum)
        .map_err(|_| ResponsesError::ToolArgumentsTooLarge)?;
    arguments.push_str(delta);
    Ok(())
}

pub(crate) fn checked_accumulated_size(
    current: usize,
    additional: usize,
    maximum: usize,
) -> Result<usize> {
    current
        .checked_add(additional)
        .filter(|next| *next <= maximum)
        .ok_or(ResponsesError::ResourceLimitExceeded)
}

fn terminal_status(event_type: &str, response: &Object<'_>) -> Result<TerminalStatus> {
    let expected = match event_type {
        "response.completed" => Some(TerminalStatus::Completed),
        "response.incomplete" => Some(TerminalStatus::Incomplete),
        "response.failed" => Some(TerminalStatus::Failed),
        _ => None,
    };
    if let Some(value) = response.get("status") {
        let status = match value.as_str() {
            Some("completed") => TerminalStatus::Completed,
            Some("incomplete") => TerminalStatus::Incomplete,
            Some("failed") => TerminalStatus::Failed,
            Some("cancelled") => TerminalStatus::Cancelled,
            _ => return Err(ResponsesError::InvalidEvent),
        };
        if expected.is_some_and(|kind| kind != status) {
            return Err(ResponsesError::InvalidEvent);
        }
        return Ok(status);
    }
    if let Some(kind) = expected {
        return Ok(kind);
    }
    if response.get("error").is_some_and(|value| !value.is_null()) {
        return Ok(TerminalStatus::Failed);
    }
    if response
        .get("incomplete_details")
        .is_some_and(|value| !value.is_null())
    {
        return Ok(TerminalStatus::Incomplete);
    }
    Ok(TerminalStatus::Completed)
}

fn finish_reason(
    status: TerminalStatus,
    response: &Object<'_>,
    has_tools: bool,
) -> ResponsesFinish {
    match status {
        TerminalStatus::Completed if has_tools => ResponsesFinish::ToolCalls,
        TerminalStatus::Completed => ResponsesFinish::Stop,
        TerminalStatus::Incomplete => match response
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Json::as_str)
        {
            Some("max_output_tokens") => ResponsesFinish::Length,
            Some("content_filter") => ResponsesFinish::ContentFilter,
            _ => ResponsesFinish::ProviderError,
        },
        TerminalStatus::Failed | TerminalStatus::Cancelled => ResponsesFinish::ProviderError,
    }
}

fn parse_usage(response: &Object<'_>) -> Usage {
    let Some(Json::Object(usage)) = response.get("usage") else {
        return Usage::default();
    };
    let counter = |key: &str| usage.get(key).and_then(Json::as_u64);
    Usage {
        input_tokens: counter("input_tokens"),
        output_tokens: counter("output_tokens"),
    }
}

#[cfg(test)]
mod tests;
