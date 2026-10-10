use ofx_contract::parse_strict_json_value;
use ofx_jsonrpc::RpcError;
use serde_json::Value;

use crate::error::McpError;
use crate::features::common::{
    self, Listed, Paging, bounded_json, optional_string, parse_cache_hints, parse_envelope,
    parse_protocol_error, required_string, validate_bounded_json, validate_icons,
    validate_json_depth, validate_prompt_content,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) pages: usize,
    pub(crate) prompts: usize,
    pub(crate) arguments: usize,
    pub(crate) messages: usize,
    pub(crate) cursor_bytes: usize,
    pub(crate) arguments_json_bytes: usize,
    pub(crate) result_json_bytes: usize,
    pub(crate) common: common::Limits,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pages: 64,
            prompts: 4096,
            arguments: 128,
            messages: 256,
            cursor_bytes: 4096,
            arguments_json_bytes: 128 * 1024,
            result_json_bytes: 4 * 1024 * 1024,
            common: common::Limits::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptArgument {
    pub name: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prompt {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) arguments: Vec<PromptArgument>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptRole {
    User,
    Assistant,
}

impl PromptRole {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptContentKind {
    Text,
    Image,
    Audio,
    ResourceLink,
    Resource,
}

impl PromptContentKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::ResourceLink => "resource_link",
            Self::Resource => "resource",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMessage {
    pub role: PromptRole,
    pub content_kind: PromptContentKind,
    pub content_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptGetResult {
    pub description: Option<String>,
    pub messages: Vec<PromptMessage>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GetOutcome {
    Complete(PromptGetResult),
    ProtocolFailure(RpcError),
}

impl Listed for Prompt {
    const LIST_METHOD: &'static str = "prompts/list";
    const ITEMS_FIELD: &'static str = "prompts";
    const COUNTS_ITEMS_FIRST: bool = true;

    type Limits = Limits;

    fn paging(limits: Limits) -> Paging {
        Paging {
            pages: limits.pages,
            items: limits.prompts,
            cursor_bytes: limits.cursor_bytes,
            common: limits.common,
        }
    }

    fn parse_item(value: &Value, limits: Limits) -> Result<Self, McpError> {
        let common = limits.common;
        let invalid = |_| McpError::InvalidPrompt;
        let object = value.as_object().ok_or(McpError::InvalidPrompt)?;
        let name = required_string(object, "name", common.name_bytes).map_err(invalid)?;
        let title = optional_string(object, "title", common.title_bytes).map_err(invalid)?;
        let description =
            optional_string(object, "description", common.description_bytes).map_err(invalid)?;
        if let Some(icons) = object.get("icons") {
            validate_icons(icons, common).map_err(invalid)?;
        }
        if let Some(metadata) = object.get("_meta") {
            if !metadata.is_object() {
                return Err(McpError::InvalidPrompt);
            }
            validate_bounded_json(metadata, common.metadata_bytes, common.json_depth)
                .map_err(invalid)?;
        }
        let arguments = match object.get("arguments") {
            None => &[][..],
            Some(Value::Array(arguments)) if arguments.len() <= limits.arguments => arguments,
            Some(_) => return Err(McpError::ArgumentLimitExceeded),
        };
        let mut parsed: Vec<PromptArgument> = Vec::with_capacity(arguments.len());
        for argument in arguments {
            let argument = parse_argument(argument, common)?;
            if parsed.iter().any(|previous| previous.name == argument.name) {
                return Err(McpError::DuplicateArgument);
            }
            parsed.push(argument);
        }
        if let Some(icons) = object.get("icons") {
            validate_bounded_json(icons, common.metadata_bytes, common.json_depth)?;
        }
        Ok(Self {
            name: name.to_owned(),
            title: title.map(str::to_owned),
            description: description.map(str::to_owned),
            arguments: parsed,
        })
    }

    fn identity(&self) -> &str {
        &self.name
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn limit_exceeded() -> McpError {
        McpError::PromptLimitExceeded
    }

    fn duplicate() -> McpError {
        McpError::DuplicatePrompt
    }
}

fn parse_argument(value: &Value, limits: common::Limits) -> Result<PromptArgument, McpError> {
    let invalid = |_| McpError::InvalidArgument;
    let object = value.as_object().ok_or(McpError::InvalidArgument)?;
    let name = required_string(object, "name", limits.name_bytes).map_err(invalid)?;
    optional_string(object, "description", limits.description_bytes).map_err(invalid)?;
    let required = match object.get("required") {
        None => false,
        Some(Value::Bool(required)) => *required,
        Some(_) => return Err(McpError::InvalidArgument),
    };
    Ok(PromptArgument {
        name: name.to_owned(),
        required,
    })
}

pub(crate) fn parse_get_outcome(response: &str, limits: Limits) -> Result<GetOutcome, McpError> {
    let envelope = parse_envelope(response, limits.common)?;
    if let Some(error) = envelope.get("error") {
        return parse_protocol_error(error, limits.common).map(GetOutcome::ProtocolFailure);
    }
    let result = envelope
        .get("result")
        .and_then(Value::as_object)
        .ok_or(McpError::InvalidGetResult)?;
    match result.get("resultType") {
        None => {}
        Some(Value::String(kind)) if kind == "complete" => {}
        Some(Value::String(_)) => return Err(McpError::UnsupportedResultType),
        Some(_) => return Err(McpError::InvalidGetResult),
    }
    let description = optional_string(result, "description", limits.common.description_bytes)?;
    let items = result
        .get("messages")
        .ok_or(McpError::InvalidGetResult)?
        .as_array()
        .filter(|items| items.len() <= limits.messages)
        .ok_or(McpError::MessageLimitExceeded)?;
    let mut messages = Vec::with_capacity(items.len());
    let mut total_bytes = description.map_or(0, str::len);
    for item in items {
        let message = parse_message(item, limits)?;
        total_bytes = total_bytes.saturating_add(message.content_json.len());
        if total_bytes > limits.common.total_content_bytes {
            return Err(McpError::MessageLimitExceeded);
        }
        messages.push(message);
    }
    parse_cache_hints(result)?;
    Ok(GetOutcome::Complete(PromptGetResult {
        description: description.map(str::to_owned),
        messages,
    }))
}

fn parse_message(value: &Value, limits: Limits) -> Result<PromptMessage, McpError> {
    let object = value.as_object().ok_or(McpError::InvalidMessage)?;
    let role = match object.get("role").and_then(Value::as_str) {
        Some("user") => PromptRole::User,
        Some("assistant") => PromptRole::Assistant,
        _ => return Err(McpError::InvalidMessage),
    };
    let content = object.get("content").ok_or(McpError::InvalidMessage)?;
    validate_prompt_content(content, limits.common)?;
    let content_kind = match content.get("type").and_then(Value::as_str) {
        Some("text") => PromptContentKind::Text,
        Some("image") => PromptContentKind::Image,
        Some("audio") => PromptContentKind::Audio,
        Some("resource_link") => PromptContentKind::ResourceLink,
        _ => PromptContentKind::Resource,
    };
    Ok(PromptMessage {
        role,
        content_kind,
        content_json: bounded_json(content, limits.result_json_bytes, limits.common.json_depth)?,
    })
}

pub(crate) fn validate_arguments_json(
    prompt: &Prompt,
    arguments_json: &str,
    limits: Limits,
) -> Result<Value, McpError> {
    if arguments_json.len() > limits.arguments_json_bytes {
        return Err(McpError::InvalidArguments);
    }
    let Ok(Value::Object(arguments)) = parse_strict_json_value(arguments_json.as_bytes()) else {
        return Err(McpError::InvalidArguments);
    };
    if arguments.len() > limits.arguments {
        return Err(McpError::InvalidArguments);
    }
    for argument in &prompt.arguments {
        match arguments.get(&argument.name) {
            None if argument.required => return Err(McpError::InvalidArguments),
            Some(value) if !value.is_string() => return Err(McpError::InvalidArguments),
            _ => {}
        }
    }
    let declared = |name: &String| {
        prompt
            .arguments
            .iter()
            .any(|argument| argument.name == *name)
    };
    if !arguments.keys().all(declared) {
        return Err(McpError::InvalidArguments);
    }
    let arguments = Value::Object(arguments);
    validate_json_depth(&arguments, limits.common.json_depth)
        .map_err(|_| McpError::InvalidArguments)?;
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::features::common::{Catalog, CatalogBuilder, Page, parse_page};

    fn page_with(result: &Value, limits: Limits) -> Result<Page<Prompt>, McpError> {
        parse_page(
            &json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string(),
            limits,
        )
    }

    fn page(result: &Value) -> Result<Page<Prompt>, McpError> {
        page_with(result, Limits::default())
    }

    fn catalog(pages: &[(Value, u64)], limits: Limits) -> Result<Catalog<Prompt>, McpError> {
        let mut builder = CatalogBuilder::default();
        for (result, received_at_ms) in pages {
            builder.append_page(page(result)?, *received_at_ms, limits)?;
        }
        builder.finish()
    }

    fn argument(name: &str, required: bool) -> PromptArgument {
        PromptArgument {
            name: name.to_owned(),
            required,
        }
    }

    #[test]
    fn prompt_pagination_owns_arguments_and_sorts_stable_names() {
        let prompts = catalog(
            &[
                (
                    json!({"prompts": [{"name": "review", "description": "Review code", "arguments": [{"name": "focus", "required": true}]}], "nextCursor": "next", "ttlMs": 100}),
                    1000,
                ),
                (json!({"prompts": [{"name": "explain", "arguments": []}], "ttlMs": 50}), 1010),
            ],
            Limits::default(),
        )
        .unwrap();
        assert_eq!(
            prompts.items,
            [
                Prompt {
                    name: "explain".to_owned(),
                    title: None,
                    description: None,
                    arguments: Vec::new(),
                },
                Prompt {
                    name: "review".to_owned(),
                    title: None,
                    description: Some("Review code".to_owned()),
                    arguments: vec![argument("focus", true)],
                },
            ]
        );
        assert_eq!(prompts.expires_at_ms, 1060);
        assert_eq!(
            validate_arguments_json(&prompts.items[1], "{}", Limits::default()),
            Err(McpError::InvalidArguments)
        );
    }

    #[test]
    fn a_prompt_with_every_listed_field_keeps_its_labels_and_argument_order() {
        let listed = page(&json!({"prompts": [{
            "name": "review",
            "title": "Review",
            "description": "Review code",
            "arguments": [
                {"name": "focus", "description": "Area", "required": true},
                {"name": "depth", "required": false},
                {"name": "style"}
            ],
            "icons": [{"src": "data:image/png;base64,AA=="}],
            "_meta": {"version": 1}
        }]}))
        .unwrap();
        let prompts = {
            let mut builder = CatalogBuilder::default();
            builder.append_page(listed, 1, Limits::default()).unwrap();
            builder.finish().unwrap()
        };
        assert_eq!(
            prompts.items,
            [Prompt {
                name: "review".to_owned(),
                title: Some("Review".to_owned()),
                description: Some("Review code".to_owned()),
                arguments: vec![
                    argument("focus", true),
                    argument("depth", false),
                    argument("style", false)
                ],
            }]
        );
        assert_eq!(prompts.expires_at_ms, u64::MAX);
    }

    #[test]
    fn prompt_catalog_metadata_enforces_the_shared_json_depth_boundary() {
        let limits = Limits {
            common: common::Limits {
                json_depth: 6,
                ..common::Limits::default()
            },
            ..Limits::default()
        };
        let parse = |meta: Value| {
            page_with(&json!({"prompts": [{"name": "x", "_meta": meta}]}), limits)
                .map(|page| page.items.len())
        };
        assert_eq!(parse(json!({"value": 1})), Ok(1));
        assert_eq!(parse(json!({"nested": {"value": 1}})), Ok(1));
        assert_eq!(
            parse(json!({"nested": {"again": {"value": 1}}})),
            Err(McpError::JsonDepthLimitExceeded)
        );
    }

    #[test]
    fn prompts_and_arguments_fail_with_upstreams_error_names() {
        let large = "x".repeat(64 * 1024 + 1);
        let cases = [
            (json!({}), McpError::InvalidListResult),
            (json!({"prompts": {}}), McpError::InvalidListResult),
            (
                json!({"prompts": [], "nextCursor": 1}),
                McpError::InvalidListResult,
            ),
            (
                json!({"prompts": [], "resultType": "input_required"}),
                McpError::UnsupportedResultType,
            ),
            (
                json!({"prompts": [], "ttlMs": 0.5}),
                McpError::InvalidResult,
            ),
            (json!({"prompts": ["review"]}), McpError::InvalidPrompt),
            (json!({"prompts": [{}]}), McpError::InvalidPrompt),
            (json!({"prompts": [{"name": ""}]}), McpError::InvalidPrompt),
            (
                json!({"prompts": [{"name": "a", "title": 1}]}),
                McpError::InvalidPrompt,
            ),
            (
                json!({"prompts": [{"name": "a", "description": large}]}),
                McpError::InvalidPrompt,
            ),
            (
                json!({"prompts": [{"name": "a", "icons": [{"src": ""}]}]}),
                McpError::InvalidPrompt,
            ),
            (
                json!({"prompts": [{"name": "a", "_meta": []}]}),
                McpError::InvalidPrompt,
            ),
            (
                json!({"prompts": [{"name": "a", "_meta": {"big": "x".repeat(128 * 1024)}}]}),
                McpError::InvalidPrompt,
            ),
            (
                json!({"prompts": [{"name": "a", "arguments": {}}]}),
                McpError::ArgumentLimitExceeded,
            ),
            (
                json!({"prompts": [{"name": "a", "arguments": vec![json!({"name": "x"}); 129]}]}),
                McpError::ArgumentLimitExceeded,
            ),
            (
                json!({"prompts": [{"name": "a", "arguments": ["x"]}]}),
                McpError::InvalidArgument,
            ),
            (
                json!({"prompts": [{"name": "a", "arguments": [{"required": true}]}]}),
                McpError::InvalidArgument,
            ),
            (
                json!({"prompts": [{"name": "a", "arguments": [{"name": "x", "description": 1}]}]}),
                McpError::InvalidArgument,
            ),
            (
                json!({"prompts": [{"name": "a", "arguments": [{"name": "x", "required": "yes"}]}]}),
                McpError::InvalidArgument,
            ),
            (
                json!({"prompts": [{"name": "a", "arguments": [{"name": "x"}, {"name": "x", "required": true}]}]}),
                McpError::DuplicateArgument,
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(page(&result).err(), Some(expected), "{result}");
        }
        let failure =
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "no"}});
        assert_eq!(
            parse_page::<Prompt>(&failure.to_string(), Limits::default()).err(),
            Some(McpError::ProtocolFailure)
        );
        assert_eq!(
            page(&json!({"prompts": vec![json!({"name": "a"}); 4097]})).err(),
            Some(McpError::InvalidListResult)
        );
    }

    #[test]
    fn serialized_prompt_icons_stay_within_the_metadata_limit_after_the_arguments() {
        let icon = json!({"src": format!("data:image/png;base64,{}", "A".repeat(60 * 1024))});
        let icons = json!([icon, icon, icon]);
        assert_eq!(
            page(&json!({"prompts": [{"name": "a", "icons": icons}]})).err(),
            Some(McpError::MetadataLimitExceeded)
        );
        assert_eq!(
            page(&json!({"prompts": [{"name": "a", "icons": icons, "arguments": [{}]}]})).err(),
            Some(McpError::InvalidArgument)
        );
        assert_eq!(
            page(&json!({"prompts": [{"name": "a", "icons": [icon]}]}))
                .map(|page| page.items.len()),
            Ok(1)
        );
    }

    #[test]
    fn prompt_pages_must_agree_and_count_prompts_before_cursors() {
        let first = json!({"prompts": [{"name": "a"}], "nextCursor": "next"});
        let limits = Limits::default();
        assert_eq!(
            catalog(&[(first.clone(), 0), (first.clone(), 0)], limits),
            Err(McpError::DuplicateCursor)
        );
        assert_eq!(
            catalog(
                &[(first.clone(), 0), (json!({"prompts": [{"name": "a"}]}), 0)],
                limits
            ),
            Err(McpError::DuplicatePrompt)
        );
        assert_eq!(
            catalog(
                &[(json!({"prompts": [{"name": "a"}, {"name": "a"}]}), 0)],
                limits
            ),
            Err(McpError::DuplicatePrompt)
        );
        assert_eq!(
            catalog(
                &[
                    (first.clone(), 0),
                    (json!({"prompts": [], "cacheScope": "public"}), 0)
                ],
                limits
            ),
            Err(McpError::InconsistentCacheScope)
        );
        let one = Limits {
            prompts: 1,
            ..Limits::default()
        };
        assert_eq!(
            catalog(&[(first.clone(), 0), (first.clone(), 0)], one),
            Err(McpError::PromptLimitExceeded)
        );
        assert_eq!(
            catalog(
                &[
                    (first.clone(), 0),
                    (
                        json!({"prompts": [{"name": "b"}], "cacheScope": "public"}),
                        0
                    )
                ],
                one
            ),
            Err(McpError::PromptLimitExceeded)
        );
        let single_page = Limits {
            pages: 1,
            ..Limits::default()
        };
        assert_eq!(
            catalog(&[(first, 0), (json!({"prompts": []}), 0)], single_page),
            Err(McpError::PaginationLimitExceeded)
        );
    }

    fn get(result: &Value, limits: Limits) -> Result<GetOutcome, McpError> {
        parse_get_outcome(
            &json!({"jsonrpc": "2.0", "id": 3, "result": result}).to_string(),
            limits,
        )
    }

    fn messages(outcome: Result<GetOutcome, McpError>) -> Vec<PromptMessage> {
        match outcome {
            Ok(GetOutcome::Complete(result)) => result.messages,
            other => panic!("not a complete prompt: {other:?}"),
        }
    }

    fn text_message(text: &str) -> Value {
        json!({"role": "user", "content": {"type": "text", "text": text}})
    }

    #[test]
    fn prompt_get_preserves_every_permitted_content_type() {
        let outcome = get(
            &json!({"description": "fixture", "messages": [
                {"role": "user", "content": {"type": "text", "text": "hello"}},
                {"role": "assistant", "content": {"type": "image", "mimeType": "image/png", "data": "aGVsbG8="}},
                {"role": "user", "content": {"type": "audio", "mimeType": "audio/wav", "data": "aGVsbG8="}},
                {"role": "assistant", "content": {"type": "resource_link", "uri": "git://repo", "name": "repo"}},
                {"role": "user", "content": {"type": "resource", "resource": {"uri": "memory://one", "text": "body"}}}
            ]}),
            Limits::default(),
        );
        let Ok(GetOutcome::Complete(result)) = outcome else {
            panic!("not a complete prompt: {outcome:?}");
        };
        assert_eq!(result.description.as_deref(), Some("fixture"));
        let kinds: Vec<_> = result
            .messages
            .iter()
            .map(|message| (message.role, message.content_kind))
            .collect();
        assert_eq!(
            kinds,
            [
                (PromptRole::User, PromptContentKind::Text),
                (PromptRole::Assistant, PromptContentKind::Image),
                (PromptRole::User, PromptContentKind::Audio),
                (PromptRole::Assistant, PromptContentKind::ResourceLink),
                (PromptRole::User, PromptContentKind::Resource),
            ]
        );
        assert_eq!(
            result.messages[4].content_json,
            r#"{"type":"resource","resource":{"uri":"memory://one","text":"body"}}"#
        );
        let limits = Limits {
            common: common::Limits {
                total_content_bytes: 4,
                ..common::Limits::default()
            },
            ..Limits::default()
        };
        assert_eq!(
            get(&json!({"messages": [text_message("hello")]}), limits),
            Err(McpError::MessageLimitExceeded)
        );
    }

    #[test]
    fn prompt_content_preserves_schema_valid_empty_text_and_zero_byte_media() {
        let parsed = messages(get(
            &json!({"messages": [
                text_message(""),
                {"role": "assistant", "content": {"type": "image", "mimeType": "image/png", "data": ""}},
                {"role": "user", "content": {"type": "audio", "mimeType": "audio/wav", "data": ""}}
            ]}),
            Limits::default(),
        ));
        assert_eq!(parsed.len(), 3);
        assert!(parsed[0].content_json.contains(r#""text":"""#));
        assert!(parsed[1].content_json.contains(r#""data":"""#));
    }

    #[test]
    fn prompt_content_keeps_its_fields_in_the_order_the_server_wrote_them() {
        let parsed = messages(get(
            &json!({"messages": [{"role": "user", "content": {
                "text": "line one\nline \u{1}two \u{e9}",
                "_meta": {"z": 1, "a": true},
                "annotations": {"priority": 1, "audience": ["user"]},
                "type": "text"
            }}]}),
            Limits::default(),
        ));
        assert_eq!(
            parsed[0].content_json,
            "{\"text\":\"line one\\nline \\u0001two \u{e9}\",\"_meta\":{\"z\":1,\"a\":true},\"annotations\":{\"priority\":1,\"audience\":[\"user\"]},\"type\":\"text\"}"
        );
    }

    #[test]
    fn prompt_content_writes_numbers_back_as_serde_json_holds_them() {
        let outcome = parse_get_outcome(
            r#"{"jsonrpc":"2.0","id":1,"result":{"messages":[{"role":"user","content":{"type":"text","text":"x","_meta":{"n":1e3,"r":0.50,"i":7}}}]}}"#,
            Limits::default(),
        );
        assert_eq!(
            messages(outcome)[0].content_json,
            r#"{"type":"text","text":"x","_meta":{"n":1000.0,"r":0.5,"i":7}}"#
        );
    }

    #[test]
    fn prompt_content_enforces_the_shared_json_depth_boundary() {
        let limits = Limits {
            common: common::Limits {
                json_depth: 7,
                ..common::Limits::default()
            },
            ..Limits::default()
        };
        let parse = |meta: Value| {
            get(
                &json!({"messages": [{"role": "user", "content": {"type": "text", "text": "ok", "_meta": meta}}]}),
                limits,
            )
            .map(|outcome| matches!(outcome, GetOutcome::Complete(_)))
        };
        assert_eq!(parse(json!({"value": 1})), Ok(true));
        assert_eq!(parse(json!({"nested": {"value": 1}})), Ok(true));
        assert_eq!(
            parse(json!({"nested": {"again": {"value": 1}}})),
            Err(McpError::JsonDepthLimitExceeded)
        );
    }

    #[test]
    fn prompt_get_results_fail_with_upstreams_error_names() {
        let limits = Limits::default();
        let cases = [
            (json!({}), McpError::InvalidGetResult),
            (
                json!({"resultType": 1, "messages": []}),
                McpError::InvalidGetResult,
            ),
            (
                json!({"resultType": "input_required", "messages": []}),
                McpError::UnsupportedResultType,
            ),
            (
                json!({"resultType": "partial", "messages": []}),
                McpError::UnsupportedResultType,
            ),
            (
                json!({"description": 1, "messages": []}),
                McpError::InvalidContent,
            ),
            (json!({"messages": {}}), McpError::MessageLimitExceeded),
            (
                json!({"messages": vec![text_message("x"); 257]}),
                McpError::MessageLimitExceeded,
            ),
            (json!({"messages": ["hello"]}), McpError::InvalidMessage),
            (
                json!({"messages": [{"content": {"type": "text", "text": "x"}}]}),
                McpError::InvalidMessage,
            ),
            (
                json!({"messages": [{"role": "system", "content": {"type": "text", "text": "x"}}]}),
                McpError::InvalidMessage,
            ),
            (
                json!({"messages": [{"role": "user"}]}),
                McpError::InvalidMessage,
            ),
            (
                json!({"messages": [], "ttlMs": 0.5}),
                McpError::InvalidResult,
            ),
            (
                json!({"messages": [], "cacheScope": "shared"}),
                McpError::InvalidResult,
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(get(&result, limits).err(), Some(expected), "{result}");
        }
        let small = Limits {
            result_json_bytes: 30,
            ..Limits::default()
        };
        assert_eq!(
            get(
                &json!({"messages": [text_message("x".repeat(20).as_str())]}),
                small
            ),
            Err(McpError::MetadataLimitExceeded)
        );
        assert!(get(&json!({"messages": [text_message("x")]}), small).is_ok());
    }

    #[test]
    fn prompt_message_contents_fail_with_upstreams_error_names() {
        let content = |content: Value| json!({"messages": [{"role": "user", "content": content}]});
        let cases = [
            (content(json!("hello")), McpError::InvalidContent),
            (content(json!({"text": "x"})), McpError::InvalidContent),
            (
                content(json!({"type": "video", "text": "x"})),
                McpError::InvalidContent,
            ),
            (content(json!({"type": "text"})), McpError::InvalidContent),
            (
                content(json!({"type": "image", "mimeType": "image/png", "data": "abc"})),
                McpError::InvalidContent,
            ),
            (
                content(json!({"type": "audio", "mimeType": "", "data": ""})),
                McpError::InvalidContent,
            ),
            (
                content(json!({"type": "resource_link", "uri": "git://repo"})),
                McpError::InvalidContent,
            ),
            (
                content(
                    json!({"type": "resource_link", "uri": "git://repo", "name": "repo", "size": -1}),
                ),
                McpError::InvalidContent,
            ),
            (
                content(json!({"type": "resource"})),
                McpError::InvalidContent,
            ),
            (
                content(
                    json!({"type": "resource", "resource": {"uri": "a://", "text": "x", "blob": ""}}),
                ),
                McpError::InvalidContent,
            ),
            (
                content(json!({"type": "text", "text": "x", "annotations": {"priority": 2}})),
                McpError::InvalidContent,
            ),
            (
                content(json!({"type": "text", "text": "x", "_meta": []})),
                McpError::InvalidContent,
            ),
            (
                content(json!({"type": "video", "_meta": {"big": "x".repeat(128 * 1024)}})),
                McpError::MetadataLimitExceeded,
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(
                get(&result, Limits::default()).err(),
                Some(expected),
                "{result}"
            );
        }
    }

    #[test]
    fn prompt_get_protocol_errors_keep_their_code_message_and_data() {
        let failure = json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -32602, "message": "Unknown prompt", "data": {"name": "x"}}});
        assert_eq!(
            parse_get_outcome(&failure.to_string(), Limits::default()),
            Ok(GetOutcome::ProtocolFailure(RpcError {
                code: -32602,
                message: "Unknown prompt".to_owned(),
                data: Some(json!({"name": "x"})),
            }))
        );
        assert_eq!(
            parse_get_outcome(
                r#"{"jsonrpc":"2.0","id":3,"error":{"code":"x","message":"no"}}"#,
                Limits::default()
            ),
            Err(McpError::InvalidEnvelope)
        );
    }

    fn review() -> Prompt {
        Prompt {
            name: "review".to_owned(),
            title: None,
            description: None,
            arguments: vec![argument("focus", true), argument("depth", false)],
        }
    }

    #[test]
    fn prompt_arguments_must_be_declared_strings_with_every_required_one() {
        let limits = Limits::default();
        let check = |arguments: &str| validate_arguments_json(&review(), arguments, limits);
        assert_eq!(
            check(r#" {"depth":"2", "focus":"security"} "#),
            Ok(json!({"depth": "2", "focus": "security"}))
        );
        assert_eq!(check(r#"{"focus":""}"#), Ok(json!({"focus": ""})));
        for invalid in [
            "{}",
            r#"{"depth":"2"}"#,
            r#"{"focus":1}"#,
            r#"{"focus":"a","depth":null}"#,
            r#"{"focus":"a","style":"terse"}"#,
            r#"{"focus":"a","focus":"b"}"#,
            r#"["focus"]"#,
            r#""focus""#,
            r#"{"focus":"a"} {}"#,
            r#"{"focus":"a""#,
            "focus=a",
            "",
        ] {
            assert_eq!(check(invalid), Err(McpError::InvalidArguments), "{invalid}");
        }
        let large = format!(r#"{{"focus":"{}"}}"#, "x".repeat(128 * 1024));
        assert_eq!(check(&large), Err(McpError::InvalidArguments));
        let open = Prompt {
            arguments: Vec::new(),
            ..review()
        };
        assert_eq!(validate_arguments_json(&open, "{}", limits), Ok(json!({})));
        let few = Limits {
            arguments: 1,
            ..Limits::default()
        };
        assert_eq!(
            validate_arguments_json(&review(), r#"{"focus":"a","depth":"b"}"#, few),
            Err(McpError::InvalidArguments)
        );
    }

    #[test]
    fn prompt_arguments_enforce_the_shared_json_depth_boundary() {
        let prompt = Prompt {
            arguments: vec![argument("focus", false)],
            ..review()
        };
        let limits = Limits {
            common: common::Limits {
                json_depth: 1,
                ..common::Limits::default()
            },
            ..Limits::default()
        };
        assert_eq!(
            validate_arguments_json(&prompt, "{}", limits),
            Ok(json!({}))
        );
        assert_eq!(
            validate_arguments_json(&prompt, r#"{"focus":"security"}"#, limits),
            Ok(json!({"focus": "security"}))
        );
        assert_eq!(
            validate_arguments_json(&prompt, r#"{"focus":{"nested":"no"}}"#, limits),
            Err(McpError::InvalidArguments)
        );
        let flat = Limits {
            common: common::Limits {
                json_depth: 0,
                ..common::Limits::default()
            },
            ..Limits::default()
        };
        assert_eq!(
            validate_arguments_json(&prompt, r#"{"focus":"security"}"#, flat),
            Err(McpError::InvalidArguments)
        );
    }
}
