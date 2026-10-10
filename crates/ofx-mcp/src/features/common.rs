use std::collections::HashSet;

use ofx_contract::parse_strict_json_value;
use ofx_jsonrpc::RpcError;
use serde_json::{Map, Value};

use crate::catalog_freshness::{CacheScope, earliest_expiry, page_expiry};
use crate::error::McpError;
use crate::json_number::{non_negative_u64, ttl_milliseconds};
use crate::mcp_contract::validate_json_rpc_response_envelope;

const MAX_SUPPORTED_JSON_DEPTH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) name_bytes: usize,
    pub(crate) uri_bytes: usize,
    pub(crate) title_bytes: usize,
    pub(crate) description_bytes: usize,
    pub(crate) metadata_bytes: usize,
    pub(crate) icons: usize,
    pub(crate) icon_sizes: usize,
    pub(crate) content_items: usize,
    pub(crate) content_field_bytes: usize,
    pub(crate) total_content_bytes: usize,
    pub(crate) json_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            name_bytes: 256,
            uri_bytes: 64 * 1024,
            title_bytes: 4096,
            description_bytes: 64 * 1024,
            metadata_bytes: 128 * 1024,
            icons: 16,
            icon_sizes: 16,
            content_items: 256,
            content_field_bytes: 1024 * 1024,
            total_content_bytes: 4 * 1024 * 1024,
            json_depth: 32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheHints {
    pub(crate) ttl_ms: Option<u64>,
    pub(crate) scope: CacheScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceData {
    Text(String),
    Blob(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceContent {
    pub uri: String,
    pub mime_type: Option<String>,
    pub annotations_json: Option<String>,
    pub metadata_json: Option<String>,
    pub data: ResourceData,
}

impl ResourceContent {
    pub(crate) fn content_bytes(&self) -> usize {
        let data = match &self.data {
            ResourceData::Text(value) | ResourceData::Blob(value) => value.len(),
        };
        [
            self.mime_type.as_ref(),
            self.annotations_json.as_ref(),
            self.metadata_json.as_ref(),
        ]
        .into_iter()
        .flatten()
        .fold(self.uri.len().saturating_add(data), |total, field| {
            total.saturating_add(field.len())
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Paging {
    pub(crate) pages: usize,
    pub(crate) items: usize,
    pub(crate) cursor_bytes: usize,
    pub(crate) common: Limits,
}

pub(crate) trait Listed: Sized {
    const LIST_METHOD: &'static str;
    const ITEMS_FIELD: &'static str;
    const COUNTS_ITEMS_FIRST: bool = false;

    type Limits: Copy + Default;

    fn paging(limits: Self::Limits) -> Paging;

    fn parse_item(value: &Value, limits: Self::Limits) -> Result<Self, McpError>;

    fn identity(&self) -> &str;

    fn name(&self) -> &str;

    fn limit_exceeded() -> McpError;

    fn duplicate() -> McpError;
}

#[derive(Debug)]
pub(crate) struct Page<T> {
    pub(crate) items: Vec<T>,
    next_cursor: Option<String>,
    cache: CacheHints,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Catalog<T> {
    pub(crate) items: Vec<T>,
    pub(crate) expires_at_ms: u64,
}

#[derive(Debug)]
pub(crate) struct CatalogBuilder<T> {
    items: Vec<T>,
    cursors: HashSet<String>,
    pages: usize,
    expires_at_ms: Option<u64>,
    cache_scope: Option<CacheScope>,
}

impl<T> Default for CatalogBuilder<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            cursors: HashSet::new(),
            pages: 0,
            expires_at_ms: None,
            cache_scope: None,
        }
    }
}

impl<T: Listed> CatalogBuilder<T> {
    pub(crate) fn append_page(
        &mut self,
        page: Page<T>,
        received_at_ms: u64,
        limits: T::Limits,
    ) -> Result<Option<String>, McpError> {
        let paging = T::paging(limits);
        if self.pages + 1 > paging.pages {
            return Err(McpError::PaginationLimitExceeded);
        }
        if T::COUNTS_ITEMS_FIRST {
            self.check_count(&page, paging)?;
        }
        if let Some(cursor) = &page.next_cursor
            && self.cursors.contains(cursor)
        {
            return Err(McpError::DuplicateCursor);
        }
        if self
            .cache_scope
            .is_some_and(|scope| scope != page.cache.scope)
        {
            return Err(McpError::InconsistentCacheScope);
        }
        if let Some(cursor) = &page.next_cursor {
            self.cursors.insert(cursor.clone());
        }
        self.pages += 1;
        self.cache_scope.get_or_insert(page.cache.scope);
        self.expires_at_ms = Some(earliest_expiry(
            self.expires_at_ms,
            page_expiry(received_at_ms, page.cache.ttl_ms),
        ));
        if !T::COUNTS_ITEMS_FIRST {
            self.check_count(&page, paging)?;
        }
        for (index, item) in page.items.iter().enumerate() {
            let identity = item.identity();
            if self
                .items
                .iter()
                .chain(&page.items[..index])
                .any(|seen| seen.identity() == identity)
            {
                return Err(T::duplicate());
            }
        }
        self.items.extend(page.items);
        Ok(page.next_cursor)
    }

    fn check_count(&self, page: &Page<T>, paging: Paging) -> Result<(), McpError> {
        if self.items.len() + page.items.len() > paging.items {
            return Err(T::limit_exceeded());
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<Catalog<T>, McpError> {
        let Some(expires_at_ms) = self.expires_at_ms else {
            return Err(McpError::InvalidListResult);
        };
        self.items.sort_by(|left, right| {
            left.identity()
                .as_bytes()
                .cmp(right.identity().as_bytes())
                .then_with(|| left.name().as_bytes().cmp(right.name().as_bytes()))
        });
        Ok(Catalog {
            items: self.items,
            expires_at_ms,
        })
    }
}

pub(crate) fn parse_page<T: Listed>(
    response: &str,
    limits: T::Limits,
) -> Result<Page<T>, McpError> {
    let paging = T::paging(limits);
    let envelope = parse_envelope(response, paging.common)?;
    let result = complete_result(&envelope).map_err(list_result_error)?;
    let items = result
        .get(T::ITEMS_FIELD)
        .and_then(Value::as_array)
        .filter(|items| items.len() <= paging.items)
        .ok_or(McpError::InvalidListResult)?
        .iter()
        .map(|item| T::parse_item(item, limits))
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = match result.get("nextCursor") {
        None => None,
        Some(Value::String(cursor)) if cursor.len() <= paging.cursor_bytes => Some(cursor.clone()),
        Some(_) => return Err(McpError::InvalidListResult),
    };
    Ok(Page {
        items,
        next_cursor,
        cache: parse_cache_hints(result)?,
    })
}

fn list_result_error(error: McpError) -> McpError {
    match error {
        McpError::ProtocolFailure | McpError::UnsupportedResultType => error,
        _ => McpError::InvalidListResult,
    }
}

pub(crate) fn parse_envelope(response: &str, limits: Limits) -> Result<Value, McpError> {
    let value =
        parse_strict_json_value(response.as_bytes()).map_err(|_| McpError::InvalidEnvelope)?;
    validate_json_depth(&value, limits.json_depth)?;
    validate_json_rpc_response_envelope(&value).map_err(|_| McpError::InvalidEnvelope)?;
    Ok(value)
}

pub(crate) fn complete_result(value: &Value) -> Result<&Map<String, Value>, McpError> {
    if value.get("error").is_some() {
        return Err(McpError::ProtocolFailure);
    }
    let result = value
        .get("result")
        .and_then(Value::as_object)
        .ok_or(McpError::InvalidResult)?;
    match result.get("resultType") {
        None => Ok(result),
        Some(Value::String(kind)) if kind == "complete" => Ok(result),
        Some(_) => Err(McpError::UnsupportedResultType),
    }
}

pub(crate) fn parse_cache_hints(result: &Map<String, Value>) -> Result<CacheHints, McpError> {
    let ttl_ms = result
        .get("ttlMs")
        .map(|value| ttl_milliseconds(value).ok_or(McpError::InvalidResult))
        .transpose()?;
    let scope = match result.get("cacheScope") {
        None => CacheScope::Private,
        Some(Value::String(scope)) if scope == "private" => CacheScope::Private,
        Some(Value::String(scope)) if scope == "public" => CacheScope::Public,
        Some(_) => return Err(McpError::InvalidResult),
    };
    Ok(CacheHints { ttl_ms, scope })
}

pub(crate) fn parse_protocol_error(value: &Value, limits: Limits) -> Result<RpcError, McpError> {
    let object = value.as_object().ok_or(McpError::InvalidEnvelope)?;
    let code = match object.get("code") {
        Some(Value::Number(code)) => code.as_i64().ok_or(McpError::InvalidEnvelope)?,
        _ => return Err(McpError::InvalidEnvelope),
    };
    let message = match object.get("message") {
        Some(Value::String(message)) if message.len() <= limits.description_bytes => message,
        _ => return Err(McpError::InvalidEnvelope),
    };
    let data = object
        .get("data")
        .map(|data| {
            bounded_json(data, limits.metadata_bytes, limits.json_depth).map(|_| data.clone())
        })
        .transpose()?;
    Ok(RpcError {
        code,
        message: message.clone(),
        data,
    })
}

pub(crate) fn parse_resource_content(
    value: &Value,
    limits: Limits,
) -> Result<ResourceContent, McpError> {
    let object = value.as_object().ok_or(McpError::InvalidContent)?;
    let uri = required_string(object, "uri", limits.uri_bytes)?;
    let (field, is_blob) = match (object.get("text"), object.get("blob")) {
        (Some(text), None) => (text, false),
        (None, Some(blob)) => (blob, true),
        _ => return Err(McpError::InvalidContent),
    };
    let mime_type = optional_string(object, "mimeType", limits.title_bytes)?;
    if let Some(annotations) = object.get("annotations") {
        validate_annotations(annotations, limits)?;
    }
    if let Some(metadata) = object.get("_meta") {
        if !metadata.is_object() {
            return Err(McpError::InvalidContent);
        }
        validate_bounded_json(metadata, limits.metadata_bytes, limits.json_depth)?;
    }
    let serialized = |name: &str| {
        object
            .get(name)
            .map(|value| bounded_json(value, limits.metadata_bytes, limits.json_depth))
            .transpose()
    };
    let annotations_json = serialized("annotations")?;
    let metadata_json = serialized("_meta")?;
    let value = match field {
        Value::String(value) if value.len() <= limits.content_field_bytes => value.clone(),
        _ => return Err(McpError::InvalidContent),
    };
    let data = if !is_blob {
        ResourceData::Text(value)
    } else if is_valid_base64(&value) {
        ResourceData::Blob(value)
    } else {
        return Err(McpError::InvalidContent);
    };
    Ok(ResourceContent {
        uri: uri.to_owned(),
        mime_type: mime_type.map(str::to_owned),
        annotations_json,
        metadata_json,
        data,
    })
}

pub(crate) fn validate_prompt_content(value: &Value, limits: Limits) -> Result<(), McpError> {
    let object = value.as_object().ok_or(McpError::InvalidContent)?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or(McpError::InvalidContent)?;
    if let Some(annotations) = object.get("annotations") {
        validate_annotations(annotations, limits)?;
    }
    if let Some(metadata) = object.get("_meta") {
        if !metadata.is_object() {
            return Err(McpError::InvalidContent);
        }
        validate_bounded_json(metadata, limits.metadata_bytes, limits.json_depth)?;
    }
    match kind {
        "text" => required_string_allow_empty(object, "text", limits.content_field_bytes).map(drop),
        "image" | "audio" => {
            let data = required_string_allow_empty(object, "data", limits.content_field_bytes)?;
            required_string(object, "mimeType", limits.title_bytes)?;
            if is_valid_base64(data) {
                Ok(())
            } else {
                Err(McpError::InvalidContent)
            }
        }
        "resource_link" => validate_resource_link(object, limits),
        "resource" => {
            let resource = object.get("resource").ok_or(McpError::InvalidContent)?;
            parse_resource_content(resource, limits).map(drop)
        }
        _ => Err(McpError::InvalidContent),
    }
}

fn validate_resource_link(object: &Map<String, Value>, limits: Limits) -> Result<(), McpError> {
    required_string(object, "uri", limits.uri_bytes)?;
    required_string(object, "name", limits.title_bytes)?;
    optional_string(object, "title", limits.title_bytes)?;
    optional_string(object, "description", limits.description_bytes)?;
    optional_string(object, "mimeType", limits.title_bytes)?;
    if let Some(icons) = object.get("icons") {
        validate_icons(icons, limits)?;
    }
    if object
        .get("size")
        .is_some_and(|size| non_negative_u64(size).is_none())
    {
        return Err(McpError::InvalidContent);
    }
    Ok(())
}

pub(crate) fn validate_annotations(value: &Value, limits: Limits) -> Result<(), McpError> {
    let object = value.as_object().ok_or(McpError::InvalidContent)?;
    if let Some(audience) = object.get("audience") {
        let roles = audience
            .as_array()
            .filter(|roles| roles.len() <= 2)
            .ok_or(McpError::InvalidContent)?;
        if !roles
            .iter()
            .all(|role| matches!(role.as_str(), Some("user" | "assistant")))
        {
            return Err(McpError::InvalidContent);
        }
    }
    if let Some(priority) = object.get("priority")
        && !priority
            .as_f64()
            .is_some_and(|priority| (0.0..=1.0).contains(&priority))
    {
        return Err(McpError::InvalidContent);
    }
    optional_string(object, "lastModified", limits.title_bytes)?;
    validate_bounded_json(value, limits.metadata_bytes, limits.json_depth)
}

pub(crate) fn validate_icons(value: &Value, limits: Limits) -> Result<(), McpError> {
    let icons = value
        .as_array()
        .filter(|icons| icons.len() <= limits.icons)
        .ok_or(McpError::InvalidContent)?;
    for icon in icons {
        let icon = icon.as_object().ok_or(McpError::InvalidContent)?;
        required_string(icon, "src", limits.uri_bytes)?;
        optional_string(icon, "mimeType", limits.title_bytes)?;
        if let Some(sizes) = icon.get("sizes") {
            let sizes = sizes
                .as_array()
                .filter(|sizes| sizes.len() <= limits.icon_sizes)
                .ok_or(McpError::InvalidContent)?;
            if !sizes.iter().all(|size| {
                size.as_str()
                    .is_some_and(|size| size.len() <= limits.title_bytes)
            }) {
                return Err(McpError::InvalidContent);
            }
        }
        if let Some(theme) = icon.get("theme")
            && !matches!(theme.as_str(), Some("light" | "dark"))
        {
            return Err(McpError::InvalidContent);
        }
    }
    Ok(())
}

pub(crate) fn required_string<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    max_bytes: usize,
) -> Result<&'a str, McpError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= max_bytes)
        .ok_or(McpError::InvalidContent)
}

fn required_string_allow_empty<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    max_bytes: usize,
) -> Result<&'a str, McpError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| value.len() <= max_bytes)
        .ok_or(McpError::InvalidContent)
}

pub(crate) fn optional_string<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    max_bytes: usize,
) -> Result<Option<&'a str>, McpError> {
    match object.get(name) {
        None => Ok(None),
        Some(Value::String(value)) if value.len() <= max_bytes => Ok(Some(value)),
        Some(_) => Err(McpError::InvalidContent),
    }
}

pub(crate) fn validate_json_depth(value: &Value, max_depth: usize) -> Result<(), McpError> {
    if max_depth > MAX_SUPPORTED_JSON_DEPTH || deeper_than(value, max_depth) {
        return Err(McpError::JsonDepthLimitExceeded);
    }
    Ok(())
}

fn deeper_than(value: &Value, remaining: usize) -> bool {
    let deeper = |child: &Value| remaining == 0 || deeper_than(child, remaining - 1);
    match value {
        Value::Array(items) => items.iter().any(deeper),
        Value::Object(members) => members.values().any(deeper),
        _ => false,
    }
}

pub(crate) fn validate_bounded_json(
    value: &Value,
    max_bytes: usize,
    max_depth: usize,
) -> Result<(), McpError> {
    bounded_json(value, max_bytes, max_depth).map(drop)
}

pub(crate) fn bounded_json(
    value: &Value,
    max_bytes: usize,
    max_depth: usize,
) -> Result<String, McpError> {
    validate_json_depth(value, max_depth)?;
    let json = value.to_string();
    if json.len() > max_bytes {
        return Err(McpError::MetadataLimitExceeded);
    }
    Ok(json)
}

pub(crate) fn is_valid_base64(value: &str) -> bool {
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

    fn nested(depth: usize) -> Value {
        (0..depth).fold(json!(1), |inner, _| json!({ "a": inner }))
    }

    #[test]
    fn iterative_json_depth_validation_accepts_the_limit_and_rejects_the_next_level() {
        let below = json!({"a": {"b": 1}});
        assert_eq!(validate_json_depth(&below, 2), Ok(()));
        assert_eq!(
            validate_json_depth(&below, 1),
            Err(McpError::JsonDepthLimitExceeded)
        );
        assert_eq!(validate_json_depth(&json!(1), 0), Ok(()));
        assert_eq!(validate_json_depth(&json!([]), 0), Ok(()));
        assert_eq!(validate_json_depth(&nested(32), 32), Ok(()));
        assert_eq!(
            validate_json_depth(&nested(33), 32),
            Err(McpError::JsonDepthLimitExceeded)
        );
        assert_eq!(
            validate_json_depth(&json!(1), MAX_SUPPORTED_JSON_DEPTH + 1),
            Err(McpError::JsonDepthLimitExceeded)
        );
    }

    #[test]
    fn envelopes_and_results_fail_with_upstreams_error_names() {
        let limits = Limits::default();
        assert_eq!(parse_envelope("{", limits), Err(McpError::InvalidEnvelope));
        assert_eq!(
            parse_envelope(r#"{"jsonrpc":"1.0","id":1,"result":{}}"#, limits),
            Err(McpError::InvalidEnvelope)
        );
        for duplicate in [
            r#"{"jsonrpc":"2.0","id":1,"id":1,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"items":[{"type":"text","text":"a","text":"b"}]}}"#,
        ] {
            assert_eq!(
                parse_envelope(duplicate, limits),
                Err(McpError::InvalidEnvelope),
                "{duplicate}"
            );
        }
        let failure = parse_envelope(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"no"}}"#,
            limits,
        )
        .unwrap();
        assert_eq!(complete_result(&failure), Err(McpError::ProtocolFailure));
        let scalar = parse_envelope(r#"{"jsonrpc":"2.0","id":1,"result":1}"#, limits).unwrap();
        assert_eq!(complete_result(&scalar), Err(McpError::InvalidResult));
        let pending = parse_envelope(
            r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"input_required"}}"#,
            limits,
        )
        .unwrap();
        assert_eq!(
            complete_result(&pending),
            Err(McpError::UnsupportedResultType)
        );
    }

    #[test]
    fn cache_hints_default_to_private_without_a_lifetime() {
        let hints = |result: Value| parse_cache_hints(result.as_object().unwrap());
        assert_eq!(
            hints(json!({})),
            Ok(CacheHints {
                ttl_ms: None,
                scope: CacheScope::Private
            })
        );
        assert_eq!(
            hints(json!({"ttlMs": 2.5e3, "cacheScope": "public"})),
            Ok(CacheHints {
                ttl_ms: Some(2500),
                scope: CacheScope::Public
            })
        );
        assert_eq!(hints(json!({"ttlMs": 0.5})), Err(McpError::InvalidResult));
        assert_eq!(hints(json!({"ttlMs": "5"})), Err(McpError::InvalidResult));
        assert_eq!(
            hints(json!({"cacheScope": "shared"})),
            Err(McpError::InvalidResult)
        );
    }

    #[test]
    fn annotations_and_icons_follow_the_shared_bounds() {
        let limits = Limits::default();
        assert_eq!(
            validate_annotations(
                &json!({"audience": ["user", "assistant"], "priority": 0.5, "lastModified": "2025-01-01"}),
                limits
            ),
            Ok(())
        );
        for invalid in [
            json!([]),
            json!({"audience": ["system"]}),
            json!({"audience": ["user", "user", "user"]}),
            json!({"priority": 1.5}),
            json!({"priority": "high"}),
            json!({"lastModified": 1}),
        ] {
            assert_eq!(
                validate_annotations(&invalid, limits),
                Err(McpError::InvalidContent),
                "{invalid}"
            );
        }
        assert_eq!(
            validate_icons(
                &json!([{"src": "https://x/icon.png", "mimeType": "image/png", "sizes": ["48x48"], "theme": "dark"}]),
                limits
            ),
            Ok(())
        );
        for invalid in [
            json!({}),
            json!([{"src": ""}]),
            json!([{"src": "a", "theme": "blue"}]),
            json!([{"src": "a", "sizes": [48]}]),
            Value::Array(vec![json!({"src": "a"}); 17]),
        ] {
            assert_eq!(
                validate_icons(&invalid, limits),
                Err(McpError::InvalidContent),
                "{invalid}"
            );
        }
    }

    #[test]
    fn prompt_resource_links_follow_the_shared_descriptor_bounds() {
        let limits = Limits::default();
        let link = |fields: Value| {
            let mut content = json!({"type": "resource_link", "uri": "git://repo", "name": "repo"});
            content
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            validate_prompt_content(&content, limits)
        };
        assert_eq!(
            link(json!({
                "title": "Repository",
                "description": "The repository",
                "mimeType": "text/x-git",
                "icons": [{"src": "https://x/icon.png"}],
                "size": 1e3
            })),
            Ok(())
        );
        for invalid in [
            json!({"name": ""}),
            json!({"uri": "x".repeat(64 * 1024 + 1)}),
            json!({"name": "x".repeat(4097)}),
            json!({"title": 1}),
            json!({"mimeType": "x".repeat(4097)}),
            json!({"icons": [{"src": ""}]}),
            json!({"size": 1.5}),
            json!({"size": "3"}),
        ] {
            assert_eq!(
                link(invalid.clone()),
                Err(McpError::InvalidContent),
                "{invalid}"
            );
        }
    }

    #[test]
    fn bounded_json_reports_size_and_depth_separately() {
        assert_eq!(validate_bounded_json(&json!({"k": "v"}), 9, 32), Ok(()));
        assert_eq!(
            validate_bounded_json(&json!({"k": "v"}), 8, 32),
            Err(McpError::MetadataLimitExceeded)
        );
        assert_eq!(
            validate_bounded_json(&nested(3), 1024, 2),
            Err(McpError::JsonDepthLimitExceeded)
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
