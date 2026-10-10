use std::collections::HashSet;

use ofx_contract::ArgumentShape;
use ofx_jsonrpc::RpcError;
use serde_json::{Map, Value};

use crate::error::McpError;
use crate::json_number::{non_negative_u64, ttl_milliseconds};
use crate::mcp_contract::validate_json_rpc_response_envelope;

const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_VALUE_NODES: usize = 4096;
const RESPONSE_OVERHEAD_BYTES: usize = 16 * 1024;
const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";
const DRAFT_07: &str = "http://json-schema.org/draft-07/schema";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) pages: usize,
    pub(crate) tools: usize,
    pub(crate) cursor_bytes: usize,
    pub(crate) name_bytes: usize,
    pub(crate) title_bytes: usize,
    pub(crate) description_bytes: usize,
    pub(crate) icons: usize,
    pub(crate) icon_sizes: usize,
    pub(crate) metadata_bytes: usize,
    pub(crate) content_items: usize,
    pub(crate) content_field_bytes: usize,
    pub(crate) image_bytes: usize,
    pub(crate) schema_bytes: usize,
    pub(crate) argument_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pages: 64,
            tools: 2048,
            cursor_bytes: 4096,
            name_bytes: 256,
            title_bytes: 4096,
            description_bytes: 64 * 1024,
            icons: 16,
            icon_sizes: 16,
            metadata_bytes: 128 * 1024,
            content_items: 256,
            content_field_bytes: 1024 * 1024,
            image_bytes: 5 * 1024 * 1024,
            schema_bytes: 256 * 1024,
            argument_bytes: 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tool {
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub icons: Option<Value>,
    pub annotations: Option<Value>,
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ToolCatalog {
    pub tools: Vec<Tool>,
}

impl ToolCatalog {
    pub(crate) fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|tool| tool.name == name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ResourceContents {
    Text(String),
    Blob(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToolContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime_type: String,
    },
    Audio {
        data: String,
        mime_type: String,
    },
    ResourceLink {
        uri: String,
        name: String,
        title: Option<String>,
        description: Option<String>,
        mime_type: Option<String>,
    },
    Resource {
        uri: String,
        mime_type: Option<String>,
        contents: ResourceContents,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolCallResult {
    pub content: Vec<ToolContent>,
    pub is_error: bool,
    pub structured_content: Option<Value>,
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToolCallOutcome {
    Complete(ToolCallResult),
    ProtocolFailure(RpcError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheScope {
    Private,
    Public,
}

#[derive(Debug)]
pub(crate) struct Page {
    tools: Vec<Tool>,
    next_cursor: Option<String>,
    cache_scope: CacheScope,
}

#[derive(Debug, Default)]
pub(crate) struct CatalogBuilder {
    tools: Vec<Tool>,
    names: HashSet<String>,
    cursors: HashSet<String>,
    next_cursor: Option<String>,
    pages: usize,
    cache_scope: Option<CacheScope>,
}

impl CatalogBuilder {
    pub(crate) fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }

    pub(crate) fn append_response(
        &mut self,
        response: &str,
        limits: Limits,
    ) -> Result<bool, McpError> {
        let page = parse_list_page(response, limits)?;
        self.append_page(page, limits)?;
        Ok(self.next_cursor.is_none())
    }

    fn append_page(&mut self, page: Page, limits: Limits) -> Result<(), McpError> {
        self.pages += 1;
        if self.pages > limits.pages {
            return Err(McpError::PaginationLimitExceeded);
        }
        if self.tools.len() + page.tools.len() > limits.tools {
            return Err(McpError::ToolLimitExceeded);
        }
        if page
            .next_cursor
            .as_ref()
            .is_some_and(|cursor| self.cursors.contains(cursor))
        {
            return Err(McpError::DuplicateCursor);
        }
        match self.cache_scope {
            Some(scope) if scope != page.cache_scope => {
                return Err(McpError::InconsistentCacheScope);
            }
            Some(_) => {}
            None => self.cache_scope = Some(page.cache_scope),
        }
        for tool in &page.tools {
            if !self.names.insert(tool.name.clone()) {
                return Err(McpError::DuplicateTool);
            }
        }
        self.tools.extend(page.tools);
        if let Some(cursor) = &page.next_cursor {
            self.cursors.insert(cursor.clone());
        }
        self.next_cursor = page.next_cursor;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<ToolCatalog, McpError> {
        if self.pages == 0 {
            return Err(McpError::InvalidListResult);
        }
        self.tools
            .sort_unstable_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
        Ok(ToolCatalog { tools: self.tools })
    }
}

pub(crate) fn parse_list_page(response: &str, limits: Limits) -> Result<Page, McpError> {
    let value: Value = serde_json::from_str(response).map_err(|_| McpError::InvalidEnvelope)?;
    validate_json_rpc_response_envelope(&value).map_err(|_| McpError::InvalidEnvelope)?;
    if value.get("error").is_some() {
        return Err(McpError::ProtocolFailure);
    }
    let result = value
        .get("result")
        .and_then(Value::as_object)
        .ok_or(McpError::InvalidListResult)?;
    require_complete_result(result)?;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .filter(|tools| tools.len() <= limits.tools)
        .ok_or(McpError::InvalidListResult)?;
    let tools = tools
        .iter()
        .map(|tool| parse_tool(tool, limits))
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = match result.get("nextCursor") {
        None => None,
        Some(Value::String(cursor)) if cursor.len() <= limits.cursor_bytes => Some(cursor.clone()),
        Some(_) => return Err(McpError::InvalidListResult),
    };
    if let Some(ttl) = result.get("ttlMs")
        && ttl_milliseconds(ttl).is_none()
    {
        return Err(McpError::InvalidListResult);
    }
    let cache_scope = match result.get("cacheScope") {
        None => CacheScope::Private,
        Some(Value::String(scope)) if scope == "private" => CacheScope::Private,
        Some(Value::String(scope)) if scope == "public" => CacheScope::Public,
        Some(_) => return Err(McpError::InvalidListResult),
    };
    Ok(Page {
        tools,
        next_cursor,
        cache_scope,
    })
}

fn require_complete_result(result: &Map<String, Value>) -> Result<(), McpError> {
    match result.get("resultType") {
        None => Ok(()),
        Some(Value::String(kind)) if kind == "complete" => Ok(()),
        Some(_) => Err(McpError::UnsupportedResultType),
    }
}

fn parse_tool(value: &Value, limits: Limits) -> Result<Tool, McpError> {
    let object = value.as_object().ok_or(McpError::InvalidTool)?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && name.len() <= limits.name_bytes)
        .ok_or(McpError::InvalidTool)?;
    let description = optional_bounded_string(object, "description", limits.description_bytes)
        .map_err(|_| McpError::InvalidTool)?;
    let title = optional_bounded_string(object, "title", limits.title_bytes)
        .map_err(|_| McpError::InvalidTool)?;
    let input_schema = object.get("inputSchema").ok_or(McpError::InvalidTool)?;
    prepare_schema(input_schema, true)?;
    if let Some(output_schema) = object.get("outputSchema") {
        prepare_schema(output_schema, false)?;
    }
    if let Some(icons) = object.get("icons") {
        validate_icons(icons, limits)?;
    }
    if let Some(annotations) = object.get("annotations") {
        validate_tool_annotations(annotations, limits)?;
    }
    if object.get("_meta").is_some_and(|meta| !meta.is_object()) {
        return Err(McpError::InvalidTool);
    }
    let bounded = |key: &str, max_bytes: usize| -> Result<Option<Value>, McpError> {
        object
            .get(key)
            .map(|value| bounded_clone(value, max_bytes))
            .transpose()
    };
    Ok(Tool {
        name: name.to_owned(),
        title: title.map(str::to_owned),
        description: description.unwrap_or_default().to_owned(),
        input_schema: bounded_clone(input_schema, limits.schema_bytes)?,
        output_schema: bounded("outputSchema", limits.schema_bytes)?,
        icons: bounded("icons", limits.metadata_bytes)?,
        annotations: bounded("annotations", limits.metadata_bytes)?,
        meta: bounded("_meta", limits.metadata_bytes)?,
    })
}

fn prepare_schema(schema: &Value, object_root: bool) -> Result<(), McpError> {
    let object = schema.as_object().ok_or(McpError::InvalidSchema)?;
    if object_root && object.get("type").and_then(Value::as_str) != Some("object") {
        return Err(McpError::InvalidSchema);
    }
    if let Some(dialect) = object.get("$schema") {
        let dialect = dialect.as_str().ok_or(McpError::InvalidSchema)?;
        let uri = dialect.trim_end_matches('#');
        if uri != DRAFT_2020_12 && uri != DRAFT_07 {
            return Err(McpError::UnsupportedDialect);
        }
    }
    check_value_bounds(schema).map_err(|()| McpError::SchemaLimitExceeded)
}

fn check_value_bounds(value: &Value) -> Result<(), ()> {
    let mut remaining = MAX_VALUE_NODES;
    check_node(value, 0, &mut remaining)
}

fn check_node(value: &Value, depth: usize, remaining: &mut usize) -> Result<(), ()> {
    if depth > MAX_SCHEMA_DEPTH || *remaining == 0 {
        return Err(());
    }
    *remaining -= 1;
    match value {
        Value::Array(items) => items
            .iter()
            .try_for_each(|item| check_node(item, depth + 1, remaining)),
        Value::Object(object) => object
            .values()
            .try_for_each(|item| check_node(item, depth + 1, remaining)),
        _ => Ok(()),
    }
}

pub(crate) fn validate_arguments(arguments_json: &str, limits: Limits) -> Result<(), McpError> {
    if arguments_json.len() > limits.argument_bytes {
        return Err(McpError::InstanceLimitExceeded);
    }
    let shape = ArgumentShape::of_function_input(arguments_json).ok_or(McpError::InvalidJson)?;
    if shape.depth > MAX_SCHEMA_DEPTH || shape.values > MAX_VALUE_NODES {
        return Err(McpError::InstanceLimitExceeded);
    }
    Ok(())
}

fn validate_icons(value: &Value, limits: Limits) -> Result<(), McpError> {
    let icons = value
        .as_array()
        .filter(|icons| icons.len() <= limits.icons)
        .ok_or(McpError::InvalidTool)?;
    for icon in icons {
        let icon = icon.as_object().ok_or(McpError::InvalidTool)?;
        require_bounded_string(icon, "src", limits.content_field_bytes)
            .map_err(|_| McpError::InvalidTool)?;
        optional_bounded_string(icon, "mimeType", limits.title_bytes)
            .map_err(|_| McpError::InvalidTool)?;
        if let Some(sizes) = icon.get("sizes") {
            let sizes = sizes
                .as_array()
                .filter(|sizes| sizes.len() <= limits.icon_sizes)
                .ok_or(McpError::InvalidTool)?;
            let valid = sizes.iter().all(|size| {
                size.as_str()
                    .is_some_and(|size| size.len() <= limits.title_bytes)
            });
            if !valid {
                return Err(McpError::InvalidTool);
            }
        }
        if let Some(theme) = icon.get("theme")
            && !matches!(theme.as_str(), Some("light" | "dark"))
        {
            return Err(McpError::InvalidTool);
        }
    }
    Ok(())
}

fn validate_tool_annotations(value: &Value, limits: Limits) -> Result<(), McpError> {
    let object = value.as_object().ok_or(McpError::InvalidTool)?;
    optional_bounded_string(object, "title", limits.title_bytes)
        .map_err(|_| McpError::InvalidTool)?;
    for hint in [
        "readOnlyHint",
        "destructiveHint",
        "idempotentHint",
        "openWorldHint",
    ] {
        if object.get(hint).is_some_and(|value| !value.is_boolean()) {
            return Err(McpError::InvalidTool);
        }
    }
    bounded_length(value, limits.metadata_bytes)
}

pub(crate) fn parse_call_outcome(
    response: &str,
    max_result_bytes: usize,
    limits: Limits,
) -> Result<ToolCallOutcome, McpError> {
    let mut value: Value = serde_json::from_str(response).map_err(|_| McpError::InvalidEnvelope)?;
    validate_json_rpc_response_envelope(&value).map_err(|_| McpError::InvalidEnvelope)?;
    if let Some(error) = value.get("error") {
        return parse_protocol_error(error, limits).map(ToolCallOutcome::ProtocolFailure);
    }
    let result = value
        .get_mut("result")
        .map(Value::take)
        .filter(Value::is_object)
        .ok_or(McpError::InvalidCallResult)?;
    let object = result.as_object().ok_or(McpError::InvalidCallResult)?;
    match object.get("resultType") {
        None => {}
        Some(Value::String(kind)) if kind == "complete" => {}
        Some(Value::String(_)) => return Err(McpError::UnsupportedResultType),
        Some(_) => return Err(McpError::InvalidCallResult),
    }
    let items = object
        .get("content")
        .ok_or(McpError::InvalidCallResult)?
        .as_array()
        .filter(|items| items.len() <= limits.content_items)
        .ok_or(McpError::InvalidContent)?;
    let content = items
        .iter()
        .map(|item| parse_content_item(item, limits))
        .collect::<Result<Vec<_>, _>>()?;
    let is_error = match object.get("isError") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(McpError::InvalidCallResult),
    };
    bounded_length(
        &result,
        max_result_bytes.saturating_add(RESPONSE_OVERHEAD_BYTES),
    )?;
    Ok(ToolCallOutcome::Complete(ToolCallResult {
        content,
        is_error,
        structured_content: object.get("structuredContent").cloned(),
        result,
    }))
}

fn parse_protocol_error(value: &Value, limits: Limits) -> Result<RpcError, McpError> {
    let object = value.as_object().ok_or(McpError::InvalidEnvelope)?;
    let code = match object.get("code") {
        Some(Value::Number(number)) => number.as_i64().ok_or(McpError::InvalidEnvelope)?,
        _ => return Err(McpError::InvalidEnvelope),
    };
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| message.len() <= limits.description_bytes)
        .ok_or(McpError::InvalidEnvelope)?;
    let data = object
        .get("data")
        .map(|data| bounded_clone(data, limits.metadata_bytes))
        .transpose()?;
    Ok(RpcError {
        code,
        message: message.to_owned(),
        data,
    })
}

fn parse_content_item(value: &Value, limits: Limits) -> Result<ToolContent, McpError> {
    let object = value.as_object().ok_or(McpError::InvalidContent)?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or(McpError::InvalidContent)?;
    if let Some(annotations) = object.get("annotations") {
        validate_annotations(annotations, limits)?;
    }
    if let Some(meta) = object.get("_meta") {
        if !meta.is_object() {
            return Err(McpError::InvalidContent);
        }
        bounded_length(meta, limits.metadata_bytes)?;
    }
    match kind {
        "text" => Ok(ToolContent::Text {
            text: require_bounded_string(object, "text", limits.content_field_bytes)?.to_owned(),
        }),
        "image" | "audio" => {
            let max_data = if kind == "image" {
                limits.image_bytes
            } else {
                limits.content_field_bytes
            };
            let data = require_bounded_string(object, "data", max_data)?.to_owned();
            let mime_type =
                require_bounded_string(object, "mimeType", limits.title_bytes)?.to_owned();
            if !is_valid_base64(&data) {
                return Err(McpError::InvalidContent);
            }
            Ok(if kind == "image" {
                ToolContent::Image { data, mime_type }
            } else {
                ToolContent::Audio { data, mime_type }
            })
        }
        "resource_link" => parse_resource_link(object, limits),
        "resource" => {
            let resource = object.get("resource").ok_or(McpError::InvalidContent)?;
            parse_embedded_resource(resource, limits)
        }
        _ => Err(McpError::InvalidContent),
    }
}

fn parse_resource_link(
    object: &Map<String, Value>,
    limits: Limits,
) -> Result<ToolContent, McpError> {
    let uri = require_bounded_string(object, "uri", limits.content_field_bytes)?;
    let name = require_bounded_string(object, "name", limits.title_bytes)?;
    let title = optional_bounded_string(object, "title", limits.title_bytes)?;
    let description = optional_bounded_string(object, "description", limits.description_bytes)?;
    let mime_type = optional_bounded_string(object, "mimeType", limits.title_bytes)?;
    if let Some(icons) = object.get("icons") {
        validate_icons(icons, limits)?;
    }
    if object
        .get("size")
        .is_some_and(|size| non_negative_u64(size).is_none())
    {
        return Err(McpError::InvalidContent);
    }
    Ok(ToolContent::ResourceLink {
        uri: uri.to_owned(),
        name: name.to_owned(),
        title: title.map(str::to_owned),
        description: description.map(str::to_owned),
        mime_type: mime_type.map(str::to_owned),
    })
}

fn parse_embedded_resource(value: &Value, limits: Limits) -> Result<ToolContent, McpError> {
    let object = value.as_object().ok_or(McpError::InvalidContent)?;
    let uri = require_bounded_string(object, "uri", limits.content_field_bytes)?;
    let mime_type = optional_bounded_string(object, "mimeType", limits.title_bytes)?;
    if let Some(meta) = object.get("_meta") {
        if !meta.is_object() {
            return Err(McpError::InvalidContent);
        }
        bounded_length(meta, limits.metadata_bytes)?;
    }
    let field = |key: &str| {
        object.get(key).map(|value| {
            value
                .as_str()
                .filter(|text| text.len() <= limits.content_field_bytes)
                .ok_or(McpError::InvalidContent)
        })
    };
    let contents = match (field("text"), field("blob")) {
        (Some(text), None) => ResourceContents::Text(text?.to_owned()),
        (None, Some(blob)) => {
            let blob = blob?;
            if !is_valid_base64(blob) {
                return Err(McpError::InvalidContent);
            }
            ResourceContents::Blob(blob.to_owned())
        }
        _ => return Err(McpError::InvalidContent),
    };
    Ok(ToolContent::Resource {
        uri: uri.to_owned(),
        mime_type: mime_type.map(str::to_owned),
        contents,
    })
}

fn validate_annotations(value: &Value, limits: Limits) -> Result<(), McpError> {
    let object = value.as_object().ok_or(McpError::InvalidContent)?;
    if let Some(audience) = object.get("audience") {
        let valid = audience.as_array().is_some_and(|roles| {
            roles.len() <= 2
                && roles
                    .iter()
                    .all(|role| matches!(role.as_str(), Some("user" | "assistant")))
        });
        if !valid {
            return Err(McpError::InvalidContent);
        }
    }
    if let Some(priority) = object.get("priority") {
        let valid = priority
            .as_f64()
            .is_some_and(|priority| (0.0..=1.0).contains(&priority));
        if !valid {
            return Err(McpError::InvalidContent);
        }
    }
    optional_bounded_string(object, "lastModified", limits.title_bytes)?;
    bounded_length(value, limits.metadata_bytes)
}

fn require_bounded_string<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    max_bytes: usize,
) -> Result<&'a str, McpError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|text| text.len() <= max_bytes)
        .ok_or(McpError::InvalidContent)
}

fn optional_bounded_string<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    max_bytes: usize,
) -> Result<Option<&'a str>, McpError> {
    object
        .get(name)
        .map(|value| {
            value
                .as_str()
                .filter(|text| text.len() <= max_bytes)
                .ok_or(McpError::InvalidContent)
        })
        .transpose()
}

fn serialized_len(value: &Value) -> usize {
    value.to_string().len()
}

fn bounded_length(value: &Value, max_bytes: usize) -> Result<(), McpError> {
    if serialized_len(value) > max_bytes {
        Err(McpError::MetadataLimitExceeded)
    } else {
        Ok(())
    }
}

fn bounded_clone(value: &Value, max_bytes: usize) -> Result<Value, McpError> {
    bounded_length(value, max_bytes)?;
    Ok(value.clone())
}

fn is_valid_base64(value: &str) -> bool {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return false;
    }
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 {
        return false;
    }
    let data = bytes.get(..bytes.len() - padding).unwrap_or_default();
    let Some(indices) = data
        .iter()
        .map(|byte| base64_index(*byte))
        .collect::<Option<Vec<u8>>>()
    else {
        return false;
    };
    match (padding, indices.last()) {
        (1, Some(last)) => last.trailing_zeros() >= 2,
        (2, Some(last)) => last.trailing_zeros() >= 4,
        (0, _) => true,
        _ => false,
    }
}

fn base64_index(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn page(response: &str) -> Page {
        parse_list_page(response, Limits::default()).unwrap()
    }

    #[test]
    fn tools_list_pages_preserve_complete_metadata_validate_schemas_and_sort_deterministically() {
        let first = page(
            r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","nextCursor":"p2","ttlMs":10,"cacheScope":"private","tools":[{"name":"zeta","title":"Zeta","description":"z","icons":[{"src":"data:image/png;base64,AA==","mimeType":"image/png","sizes":["16x16"],"theme":"dark"}],"annotations":{"readOnlyHint":true},"inputSchema":{"type":"object"},"outputSchema":{"type":"array","items":{"type":"integer"}},"_meta":{"vendor":"x"}}]}}"#,
        );
        let second = page(
            r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"alpha","inputSchema":{"type":"object","properties":{"n":{"type":"integer"}}}}]}}"#,
        );
        let mut builder = CatalogBuilder::default();
        builder.append_page(first, Limits::default()).unwrap();
        assert_eq!(builder.next_cursor(), Some("p2"));
        builder.append_page(second, Limits::default()).unwrap();
        let catalog = builder.finish().unwrap();
        assert_eq!(catalog.tools[0].name, "alpha");
        assert_eq!(catalog.tools[1].name, "zeta");
        assert_eq!(catalog.tools[1].title.as_deref(), Some("Zeta"));
        assert!(catalog.tools[1].icons.is_some());
        assert!(catalog.tools[1].annotations.is_some());
        assert!(catalog.tools[1].output_schema.is_some());
        assert!(catalog.tools[1].meta.is_some());
        assert_eq!(catalog.get("alpha").unwrap().description, "");
    }

    #[test]
    fn tools_list_retains_schemas_delegated_to_the_mcp_server() {
        let parsed = page(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"simple","inputSchema":{"type":"object"}},{"name":"provider_pattern","inputSchema":{"type":"object","properties":{"email":{"type":"string","pattern":"^(?!\\.)(?!.*\\.\\.)[A-Za-z0-9_+.-]+@[A-Za-z0-9.-]+$"}}}}]}}"#,
        );
        assert_eq!(parsed.tools.len(), 2);
        assert_eq!(
            parsed.tools[1].input_schema["properties"]["email"]["pattern"],
            "^(?!\\.)(?!.*\\.\\.)[A-Za-z0-9_+.-]+@[A-Za-z0-9.-]+$"
        );
    }

    #[test]
    fn tools_list_tolerates_negative_ttl_and_accepts_an_empty_opaque_cursor() {
        let parsed = page(
            r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","nextCursor":"","ttlMs":-25,"cacheScope":"public","tools":[]}}"#,
        );
        assert_eq!(parsed.next_cursor.as_deref(), Some(""));
        let mut builder = CatalogBuilder::default();
        builder.append_page(parsed, Limits::default()).unwrap();
        assert_eq!(builder.next_cursor(), Some(""));
        assert!(builder.finish().unwrap().tools.is_empty());
    }

    #[test]
    fn tools_list_accepts_integral_ttl_in_any_notation() {
        for ttl in ["1000.0", "1e3", "-0.5"] {
            let response =
                format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"ttlMs":{ttl},"tools":[]}}}}"#);
            assert!(
                parse_list_page(&response, Limits::default()).is_ok(),
                "{ttl}"
            );
        }
        let fractional = r#"{"jsonrpc":"2.0","id":1,"result":{"ttlMs":1.5,"tools":[]}}"#;
        assert_eq!(
            parse_list_page(fractional, Limits::default()).err(),
            Some(McpError::InvalidListResult)
        );
    }

    #[test]
    fn catalog_response_assembly_retains_its_cursor_after_parsed_page_cleanup() {
        let mut builder = CatalogBuilder::default();
        assert!(!builder
            .append_response(
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"second","inputSchema":{"type":"object"}}],"nextCursor":"page two"}}"#,
                Limits::default(),
            )
            .unwrap());
        assert_eq!(builder.next_cursor(), Some("page two"));
        assert!(builder
            .append_response(
                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"first","inputSchema":{"type":"object"}}]}}"#,
                Limits::default(),
            )
            .unwrap());
        let catalog = builder.finish().unwrap();
        let names: Vec<_> = catalog
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names, ["first", "second"]);
    }

    #[test]
    fn tools_catalog_rejects_cache_scope_changes_across_pages() {
        let mut builder = CatalogBuilder::default();
        builder
            .append_page(
                page(r#"{"jsonrpc":"2.0","id":1,"result":{"ttlMs":100,"cacheScope":"private","tools":[],"nextCursor":"n"}}"#),
                Limits::default(),
            )
            .unwrap();
        assert_eq!(
            builder.append_page(
                page(r#"{"jsonrpc":"2.0","id":2,"result":{"ttlMs":100,"cacheScope":"public","tools":[]}}"#),
                Limits::default()
            ),
            Err(McpError::InconsistentCacheScope)
        );
    }

    #[test]
    fn tools_list_rejects_repeated_cursors_and_duplicate_tools_while_retaining_schema_references() {
        let mut builder = CatalogBuilder::default();
        builder
            .append_page(
                page(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[],"nextCursor":"repeat"}}"#),
                Limits::default(),
            )
            .unwrap();
        assert_eq!(
            builder.append_page(
                page(r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[],"nextCursor":"repeat"}}"#),
                Limits::default()
            ),
            Err(McpError::DuplicateCursor)
        );
        assert_eq!(
            builder.append_page(
                page(r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"same","inputSchema":{"type":"object"}},{"name":"same","inputSchema":{"type":"object"}}]}}"#),
                Limits::default()
            ),
            Err(McpError::DuplicateTool)
        );
        let references = page(
            r#"{"jsonrpc":"2.0","id":4,"result":{"tools":[{"name":"remote","inputSchema":{"type":"object","properties":{"x":{"$ref":"https://example.com/x"}}}}]}}"#,
        );
        assert_eq!(
            references.tools[0].input_schema["properties"]["x"]["$ref"],
            "https://example.com/x"
        );
    }

    #[test]
    fn one_invalid_tool_definition_fails_the_whole_page() {
        let cases = [
            (
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"ok","inputSchema":{"type":"object"}},{"name":"bad"}]}}"#,
                McpError::InvalidTool,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"bad","inputSchema":{"type":"string"}}]}}"#,
                McpError::InvalidSchema,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"bad","inputSchema":{"type":"object","$schema":"http://json-schema.org/draft-04/schema#"}}]}}"#,
                McpError::UnsupportedDialect,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"","inputSchema":{"type":"object"}}]}}"#,
                McpError::InvalidTool,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"bad","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":"yes"}}]}}"#,
                McpError::InvalidTool,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"bad","inputSchema":{"type":"object"},"_meta":[]}]}}"#,
                McpError::InvalidTool,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#,
                McpError::ProtocolFailure,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"input_required","tools":[]}}"#,
                McpError::UnsupportedResultType,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                McpError::InvalidListResult,
            ),
            ("not json", McpError::InvalidEnvelope),
        ];
        for (response, expected) in cases {
            assert_eq!(
                parse_list_page(response, Limits::default()).map(|_| ()),
                Err(expected),
                "{response}"
            );
        }
        let draft = page(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"ok","inputSchema":{"type":"object","$schema":"https://json-schema.org/draft/2020-12/schema#"}}]}}"#,
        );
        assert_eq!(draft.tools.len(), 1);
    }

    #[test]
    fn deep_or_wide_schemas_exceed_structural_bounds() {
        let mut deep = json!({"type": "object"});
        for _ in 0..70 {
            deep = json!({"type": "object", "properties": {"x": deep}});
        }
        assert_eq!(
            prepare_schema(&deep, true),
            Err(McpError::SchemaLimitExceeded)
        );
        let wide: Map<String, Value> = (0..5000)
            .map(|index| (format!("p{index}"), json!(1)))
            .collect();
        let wide = json!({"type": "object", "properties": wide});
        assert_eq!(
            prepare_schema(&wide, true),
            Err(McpError::SchemaLimitExceeded)
        );
    }

    #[test]
    fn catalogs_enforce_page_and_tool_caps() {
        let limits = Limits {
            pages: 1,
            ..Limits::default()
        };
        let mut builder = CatalogBuilder::default();
        builder
            .append_page(
                page(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[],"nextCursor":"a"}}"#),
                limits,
            )
            .unwrap();
        assert_eq!(
            builder.append_page(
                page(r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#),
                limits
            ),
            Err(McpError::PaginationLimitExceeded)
        );
        let tools: Vec<Value> = (0..2049)
            .map(|index| json!({"name": format!("t{index}"), "inputSchema": {"type": "object"}}))
            .collect();
        let response = json!({"jsonrpc":"2.0","id":1,"result":{"tools": tools}}).to_string();
        assert_eq!(
            parse_list_page(&response, Limits::default()).map(|_| ()),
            Err(McpError::InvalidListResult)
        );
        let half: Vec<Value> = (0..1500)
            .map(|index| json!({"name": format!("a{index}"), "inputSchema": {"type": "object"}}))
            .collect();
        let other: Vec<Value> = (0..1500)
            .map(|index| json!({"name": format!("b{index}"), "inputSchema": {"type": "object"}}))
            .collect();
        let mut builder = CatalogBuilder::default();
        builder
            .append_response(
                &json!({"jsonrpc":"2.0","id":1,"result":{"tools": half, "nextCursor": "n"}})
                    .to_string(),
                Limits::default(),
            )
            .unwrap();
        assert_eq!(
            builder.append_response(
                &json!({"jsonrpc":"2.0","id":2,"result":{"tools": other}}).to_string(),
                Limits::default()
            ),
            Err(McpError::ToolLimitExceeded)
        );
        assert_eq!(
            CatalogBuilder::default().finish(),
            Err(McpError::InvalidListResult)
        );
    }

    #[test]
    fn mcp_tool_call_results_preserve_content_types_errors_and_server_output() {
        let outcome = parse_call_outcome(
            r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[{"type":"text","text":"ok"},{"type":"image","data":"AA==","mimeType":"image/png"},{"type":"audio","data":"AA==","mimeType":"audio/wav"},{"type":"resource_link","uri":"file:///a","name":"a"},{"type":"resource","resource":{"uri":"file:///b","text":"body"}},{"type":"resource","resource":{"uri":"file:///c","blob":"AA=="}}],"structuredContent":[1,2],"isError":false}}"#,
            64 * 1024,
            Limits::default(),
        )
        .unwrap();
        let ToolCallOutcome::Complete(result) = outcome else {
            panic!("expected a complete result");
        };
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(json!([1, 2])));
        assert_eq!(
            result.content,
            vec![
                ToolContent::Text {
                    text: "ok".to_owned()
                },
                ToolContent::Image {
                    data: "AA==".to_owned(),
                    mime_type: "image/png".to_owned()
                },
                ToolContent::Audio {
                    data: "AA==".to_owned(),
                    mime_type: "audio/wav".to_owned()
                },
                ToolContent::ResourceLink {
                    uri: "file:///a".to_owned(),
                    name: "a".to_owned(),
                    title: None,
                    description: None,
                    mime_type: None,
                },
                ToolContent::Resource {
                    uri: "file:///b".to_owned(),
                    mime_type: None,
                    contents: ResourceContents::Text("body".to_owned()),
                },
                ToolContent::Resource {
                    uri: "file:///c".to_owned(),
                    mime_type: None,
                    contents: ResourceContents::Blob("AA==".to_owned()),
                },
            ]
        );
        assert_eq!(result.result["content"][3]["type"], "resource_link");

        let error_outcome = parse_call_outcome(
            r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"No active project"}],"isError":true}}"#,
            64 * 1024,
            Limits::default(),
        )
        .unwrap();
        assert!(matches!(
            error_outcome,
            ToolCallOutcome::Complete(ToolCallResult { is_error: true, .. })
        ));
    }

    #[test]
    fn tool_argument_boundary_leaves_semantic_validation_to_the_server() {
        let limits = Limits::default();
        assert_eq!(
            validate_arguments("{\"email\":42,\"optional_header\":null}", limits),
            Ok(())
        );
        assert_eq!(validate_arguments("[]", limits), Err(McpError::InvalidJson));
        assert_eq!(validate_arguments("{", limits), Err(McpError::InvalidJson));
        let tight = Limits {
            argument_bytes: 1,
            ..limits
        };
        assert_eq!(
            validate_arguments("{}", tight),
            Err(McpError::InstanceLimitExceeded)
        );
        let exact = Limits {
            argument_bytes: 2,
            ..limits
        };
        assert_eq!(validate_arguments("{}", exact), Ok(()));
        assert_eq!(
            validate_arguments("{ }", exact),
            Err(McpError::InstanceLimitExceeded)
        );
    }

    #[test]
    fn tool_argument_numbers_are_checked_as_written_without_reading_their_value() {
        assert_eq!(
            validate_arguments(
                r#"{"big":1e400,"tiny":-1e-400,"id":12345678901234567890123}"#,
                Limits::default()
            ),
            Ok(())
        );
    }

    fn nested_arrays(depth: usize) -> String {
        format!("{}0{}", "[".repeat(depth), "]".repeat(depth))
    }

    fn zeros(count: usize) -> String {
        vec!["0"; count].join(",")
    }

    #[test]
    fn tool_arguments_with_a_duplicate_key_are_rejected_as_upstream_parses_them() {
        let limits = Limits::default();
        for arguments in [
            "{\"a\":1,\"a\":2}".to_owned(),
            "{\"x\":1,\"\\u0078\":2}".to_owned(),
            "{\"a\":{\"b\":1,\"b\":2}}".to_owned(),
            format!("{{\"x\":{},\"x\":0}}", nested_arrays(64)),
            format!("{{\"x\":[{}],\"x\":0}}", zeros(5000)),
            format!(
                "{{\"x\":{}{{\"a\":1,\"a\":2}}{}}}",
                "[".repeat(100),
                "]".repeat(100)
            ),
        ] {
            assert_eq!(
                validate_arguments(&arguments, limits),
                Err(McpError::InvalidJson),
                "{arguments}"
            );
        }
    }

    #[test]
    fn tool_argument_bounds_count_every_value_written() {
        let limits = Limits::default();
        assert_eq!(
            validate_arguments(&format!("{{\"x\":{}}}", nested_arrays(63)), limits),
            Ok(())
        );
        assert_eq!(
            validate_arguments(&format!("{{\"x\":{}}}", nested_arrays(64)), limits),
            Err(McpError::InstanceLimitExceeded)
        );
        assert_eq!(
            validate_arguments(&format!("{{\"x\":{}}}", nested_arrays(300)), limits),
            Err(McpError::InstanceLimitExceeded)
        );
        assert_eq!(
            validate_arguments(&format!("{{\"x\":[{}]}}", zeros(4094)), limits),
            Ok(())
        );
        assert_eq!(
            validate_arguments(&format!("{{\"x\":[{}]}}", zeros(4095)), limits),
            Err(McpError::InstanceLimitExceeded)
        );
        assert_eq!(
            validate_arguments(&format!("[{}]", nested_arrays(300)), limits),
            Err(McpError::InvalidJson)
        );
        assert_eq!(
            validate_arguments(&format!("{{\"x\":{}}} 0", nested_arrays(300)), limits),
            Err(McpError::InvalidJson)
        );
    }

    #[test]
    fn tool_call_content_rejects_malformed_payloads_and_enforces_item_limits() {
        let malformed = [
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text"}]}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"image","data":"A..A","mimeType":"image/png"}]}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"audio","data":"AA=="}]}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"resource_link","uri":"file:///a"}]}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"resource","resource":{"uri":"file:///a","text":"x","blob":"AA=="}}]}}"#,
        ];
        for response in malformed {
            assert_eq!(
                parse_call_outcome(response, 64 * 1024, Limits::default()),
                Err(McpError::InvalidContent),
                "{response}"
            );
        }
        let limits = Limits {
            content_items: 1,
            ..Limits::default()
        };
        assert_eq!(
            parse_call_outcome(
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#,
                64 * 1024,
                limits
            ),
            Err(McpError::InvalidContent)
        );
    }

    #[test]
    fn resource_link_sizes_accept_integral_values_in_any_notation() {
        let outcome = |size: &str| {
            parse_call_outcome(
                &format!(
                    r#"{{"jsonrpc":"2.0","id":1,"result":{{"content":[{{"type":"resource_link","uri":"file:///a","name":"a","size":{size}}}]}}}}"#
                ),
                64 * 1024,
                Limits::default(),
            )
            .err()
        };
        for size in ["1024", "1024.0", "1.024e3", "0"] {
            assert_eq!(outcome(size), None, "{size}");
        }
        for size in ["-1", "1.5", "\"1\"", "1e30"] {
            assert_eq!(outcome(size), Some(McpError::InvalidContent), "{size}");
        }
        assert_eq!(outcome("1e400"), Some(McpError::InvalidEnvelope));
    }

    #[test]
    fn tool_call_outcomes_distinguish_protocol_and_tool_results() {
        let protocol_failure = parse_call_outcome(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad args","data":{"field":"x"}}}"#,
            4096,
            Limits::default(),
        )
        .unwrap();
        assert_eq!(
            protocol_failure,
            ToolCallOutcome::ProtocolFailure(RpcError {
                code: -32602,
                message: "bad args".to_owned(),
                data: Some(json!({"field": "x"})),
            })
        );
        assert_eq!(
            parse_call_outcome(
                r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"input_required","inputRequests":{}}}"#,
                4096,
                Limits::default()
            ),
            Err(McpError::UnsupportedResultType)
        );
        assert_eq!(
            parse_call_outcome(
                r#"{"jsonrpc":"2.0","id":3,"result":{"content":[],"isError":"no"}}"#,
                4096,
                Limits::default()
            ),
            Err(McpError::InvalidCallResult)
        );
    }

    #[test]
    fn base64_validation_requires_canonical_padding() {
        for valid in ["", "AA==", "AAA=", "AAAA", "QUJD"] {
            assert!(is_valid_base64(valid), "{valid}");
        }
        for invalid in ["A", "A===", "AB==", "AAB=", "AA=A", "A..A", "AAAA=", "===="] {
            assert!(!is_valid_base64(invalid), "{invalid}");
        }
    }
}
