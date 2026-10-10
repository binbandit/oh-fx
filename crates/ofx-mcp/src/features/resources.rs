use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::catalog_freshness::{CacheScope, earliest_expiry, page_expiry};
use crate::error::McpError;
use crate::features::common::{
    self, CacheHints, complete_result, optional_string, parse_cache_hints, parse_envelope,
    required_string, validate_annotations, validate_bounded_json, validate_icons,
};
use crate::json_number::non_negative_u64;

const MAX_TEMPLATE_EXPRESSIONS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) pages: usize,
    pub(crate) resources: usize,
    pub(crate) templates: usize,
    pub(crate) cursor_bytes: usize,
    pub(crate) common: common::Limits,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pages: 64,
            resources: 4096,
            templates: 4096,
            cursor_bytes: 4096,
            common: common::Limits::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resource {
    pub(crate) uri: String,
    pub(crate) name: String,
    pub(crate) title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceTemplate {
    pub(crate) uri_template: String,
    pub(crate) name: String,
    pub(crate) title: Option<String>,
}

pub(crate) trait Listed: Sized {
    const LIST_METHOD: &'static str;
    const ITEMS_FIELD: &'static str;

    fn parse_item(value: &Value, limits: common::Limits) -> Result<Self, McpError>;

    fn identity(&self) -> &str;

    fn name(&self) -> &str;

    fn max_items(limits: Limits) -> usize;

    fn limit_exceeded() -> McpError;

    fn duplicate() -> McpError;
}

impl Listed for Resource {
    const LIST_METHOD: &'static str = "resources/list";
    const ITEMS_FIELD: &'static str = "resources";

    fn parse_item(value: &Value, limits: common::Limits) -> Result<Self, McpError> {
        let object = value.as_object().ok_or(McpError::InvalidResource)?;
        let uri = required_string(object, "uri", limits.uri_bytes)
            .map_err(|_| McpError::InvalidResource)?;
        let name = required_string(object, "name", limits.name_bytes)
            .map_err(|_| McpError::InvalidResource)?;
        let title = parse_descriptor_fields(object, limits)?;
        Ok(Self {
            uri: uri.to_owned(),
            name: name.to_owned(),
            title,
        })
    }

    fn identity(&self) -> &str {
        &self.uri
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn max_items(limits: Limits) -> usize {
        limits.resources
    }

    fn limit_exceeded() -> McpError {
        McpError::ResourceLimitExceeded
    }

    fn duplicate() -> McpError {
        McpError::DuplicateResource
    }
}

impl Listed for ResourceTemplate {
    const LIST_METHOD: &'static str = "resources/templates/list";
    const ITEMS_FIELD: &'static str = "resourceTemplates";

    fn parse_item(value: &Value, limits: common::Limits) -> Result<Self, McpError> {
        let object = value.as_object().ok_or(McpError::InvalidTemplate)?;
        let uri_template = required_string(object, "uriTemplate", limits.uri_bytes)
            .ok()
            .filter(|template| is_valid_uri_template(template))
            .ok_or(McpError::InvalidTemplate)?;
        let name = required_string(object, "name", limits.name_bytes)
            .map_err(|_| McpError::InvalidTemplate)?;
        let title =
            parse_descriptor_fields(object, limits).map_err(|_| McpError::InvalidTemplate)?;
        Ok(Self {
            uri_template: uri_template.to_owned(),
            name: name.to_owned(),
            title,
        })
    }

    fn identity(&self) -> &str {
        &self.uri_template
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn max_items(limits: Limits) -> usize {
        limits.templates
    }

    fn limit_exceeded() -> McpError {
        McpError::TemplateLimitExceeded
    }

    fn duplicate() -> McpError {
        McpError::DuplicateTemplate
    }
}

#[derive(Debug)]
pub(crate) struct Page<T> {
    items: Vec<T>,
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
        limits: Limits,
    ) -> Result<Option<String>, McpError> {
        if self.pages + 1 > limits.pages {
            return Err(McpError::PaginationLimitExceeded);
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
        if self.items.len() + page.items.len() > T::max_items(limits) {
            return Err(T::limit_exceeded());
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

pub(crate) fn parse_page<T: Listed>(response: &str, limits: Limits) -> Result<Page<T>, McpError> {
    let envelope = parse_envelope(response, limits.common)?;
    let result = complete_result(&envelope).map_err(list_result_error)?;
    let items = result
        .get(T::ITEMS_FIELD)
        .and_then(Value::as_array)
        .filter(|items| items.len() <= T::max_items(limits))
        .ok_or(McpError::InvalidListResult)?
        .iter()
        .map(|item| T::parse_item(item, limits.common))
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = match result.get("nextCursor") {
        None => None,
        Some(Value::String(cursor)) if cursor.len() <= limits.cursor_bytes => Some(cursor.clone()),
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

fn parse_descriptor_fields(
    object: &Map<String, Value>,
    limits: common::Limits,
) -> Result<Option<String>, McpError> {
    let title = optional_string(object, "title", limits.title_bytes)?;
    optional_string(object, "description", limits.description_bytes)?;
    optional_string(object, "mimeType", limits.title_bytes)?;
    if let Some(icons) = object.get("icons") {
        validate_icons(icons, limits)?;
        validate_bounded_json(icons, limits.metadata_bytes, limits.json_depth)?;
    }
    if let Some(annotations) = object.get("annotations") {
        validate_annotations(annotations, limits)?;
    }
    if let Some(metadata) = object.get("_meta") {
        if !metadata.is_object() {
            return Err(McpError::InvalidContent);
        }
        validate_bounded_json(metadata, limits.metadata_bytes, limits.json_depth)?;
    }
    if let Some(size) = object.get("size")
        && non_negative_u64(size).is_none()
    {
        return Err(McpError::InvalidContent);
    }
    Ok(title.map(str::to_owned))
}

fn is_valid_uri_template(template: &str) -> bool {
    let bytes = template.as_bytes();
    let mut expressions = 0;
    let mut literal_start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'}' => return false,
            b'{' => {}
            _ => {
                index += 1;
                continue;
            }
        }
        if expressions > 0 && literal_start == index {
            return false;
        }
        if !is_valid_literal(&bytes[literal_start..index])
            || expressions >= MAX_TEMPLATE_EXPRESSIONS
        {
            return false;
        }
        let expression_start = index + 1;
        let Some(length) = bytes[expression_start..]
            .iter()
            .position(|byte| *byte == b'}')
        else {
            return false;
        };
        let expression = &bytes[expression_start..expression_start + length];
        if expression.is_empty() || expression.contains(&b'{') || !is_valid_expression(expression) {
            return false;
        }
        expressions += 1;
        index = expression_start + length + 1;
        literal_start = index;
    }
    is_valid_literal(&bytes[literal_start..])
}

fn is_valid_expression(expression: &[u8]) -> bool {
    let variable = match expression[0] {
        b'+' | b'#' | b'.' | b'/' | b';' | b'?' | b'&' => &expression[1..],
        _ => expression,
    };
    is_valid_variable(variable)
}

fn is_valid_variable(variable: &[u8]) -> bool {
    if variable.first().is_none_or(|byte| *byte == b'.') || variable.last() == Some(&b'.') {
        return false;
    }
    !variable.windows(2).any(|pair| pair == b"..")
        && variable
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
}

fn is_valid_literal(literal: &[u8]) -> bool {
    let mut index = 0;
    while index < literal.len() {
        let byte = literal[index];
        if byte == b'%' {
            if index + 2 >= literal.len()
                || !literal[index + 1].is_ascii_hexdigit()
                || !literal[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
            continue;
        }
        if !is_unreserved(byte) && !is_reserved(byte) {
            return false;
        }
        index += 1;
    }
    true
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn is_reserved(byte: u8) -> bool {
    matches!(
        byte,
        b':' | b'/'
            | b'?'
            | b'#'
            | b'['
            | b']'
            | b'@'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn page<T: Listed>(result: &Value) -> Result<Page<T>, McpError> {
        parse_page(
            &json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string(),
            Limits::default(),
        )
    }

    fn catalog<T: Listed>(pages: &[(Value, u64)]) -> Result<Catalog<T>, McpError> {
        let mut builder = CatalogBuilder::default();
        for (result, received_at_ms) in pages {
            builder.append_page(page::<T>(result)?, *received_at_ms, Limits::default())?;
        }
        builder.finish()
    }

    #[test]
    fn resource_and_template_pagination_publishes_complete_sorted_snapshots() {
        let resources = catalog::<Resource>(&[
            (
                json!({"resources": [{"uri": "git://b", "name": "B"}], "nextCursor": "", "ttlMs": 100}),
                1000,
            ),
            (
                json!({"resources": [{"uri": "custom://a", "name": "A", "title": "Alpha", "annotations": {"audience": ["user"]}, "_meta": {"x": 1}}], "ttlMs": 50}),
                1010,
            ),
        ])
        .unwrap();
        assert_eq!(
            resources.items,
            [
                Resource {
                    uri: "custom://a".to_owned(),
                    name: "A".to_owned(),
                    title: Some("Alpha".to_owned()),
                },
                Resource {
                    uri: "git://b".to_owned(),
                    name: "B".to_owned(),
                    title: None,
                },
            ]
        );
        assert_eq!(resources.expires_at_ms, 1060);

        let templates = catalog::<ResourceTemplate>(&[(
            json!({"resourceTemplates": [{"uriTemplate": "db:///{table}/{id}", "name": "row"}]}),
            2000,
        )])
        .unwrap();
        assert_eq!(templates.items[0].uri_template, "db:///{table}/{id}");
        assert_eq!(templates.expires_at_ms, u64::MAX);
    }

    #[test]
    fn pages_must_agree_and_stay_within_their_bounds() {
        let first = json!({"resources": [{"uri": "a://1", "name": "one"}], "nextCursor": "next"});
        assert_eq!(
            catalog::<Resource>(&[(first.clone(), 0), (first.clone(), 0)]),
            Err(McpError::DuplicateCursor)
        );
        assert_eq!(
            catalog::<Resource>(&[
                (first.clone(), 0),
                (json!({"resources": [{"uri": "a://1", "name": "again"}]}), 0)
            ]),
            Err(McpError::DuplicateResource)
        );
        assert_eq!(
            catalog::<Resource>(&[(
                json!({"resources": [{"uri": "a://1", "name": "one"}, {"uri": "a://1", "name": "two"}]}),
                0
            )]),
            Err(McpError::DuplicateResource)
        );
        assert_eq!(
            catalog::<Resource>(&[
                (first, 0),
                (json!({"resources": [], "cacheScope": "public"}), 0)
            ]),
            Err(McpError::InconsistentCacheScope)
        );
        assert_eq!(
            catalog::<ResourceTemplate>(&[(
                json!({"resourceTemplates": [{"uriTemplate": "a://{x}", "name": "x"}, {"uriTemplate": "a://{x}", "name": "y"}]}),
                0
            )]),
            Err(McpError::DuplicateTemplate)
        );
        let mut builder = CatalogBuilder::<Resource>::default();
        let limits = Limits {
            pages: 1,
            ..Limits::default()
        };
        let next = json!({"resources": [], "nextCursor": "1"});
        builder
            .append_page(page(&next).unwrap(), 0, limits)
            .unwrap();
        assert_eq!(
            builder.append_page(page(&json!({"resources": []})).unwrap(), 0, limits),
            Err(McpError::PaginationLimitExceeded)
        );
        let mut builder = CatalogBuilder::<Resource>::default();
        let limits = Limits {
            resources: 1,
            ..Limits::default()
        };
        let first = json!({"resources": [{"uri": "a://", "name": "a"}], "nextCursor": "more"});
        builder
            .append_page(page(&first).unwrap(), 0, limits)
            .unwrap();
        assert_eq!(
            builder.append_page(
                page(&json!({"resources": [{"uri": "b://", "name": "b"}]})).unwrap(),
                0,
                limits
            ),
            Err(McpError::ResourceLimitExceeded)
        );
    }

    #[test]
    fn list_results_fail_with_upstreams_error_names() {
        let cases = [
            (json!({}), McpError::InvalidListResult),
            (json!({"resources": {}}), McpError::InvalidListResult),
            (
                json!({"resources": [], "nextCursor": 1}),
                McpError::InvalidListResult,
            ),
            (
                json!({"resources": [], "resultType": "input_required"}),
                McpError::UnsupportedResultType,
            ),
            (
                json!({"resources": [], "ttlMs": 0.5}),
                McpError::InvalidResult,
            ),
            (
                json!({"resources": [{"uri": "a://"}]}),
                McpError::InvalidResource,
            ),
            (
                json!({"resources": [{"uri": "", "name": "a"}]}),
                McpError::InvalidResource,
            ),
            (
                json!({"resources": [{"uri": "a://", "name": "a", "title": 1}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"resources": [{"uri": "a://", "name": "a", "size": -1}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"resources": [{"uri": "a://", "name": "a", "_meta": []}]}),
                McpError::InvalidContent,
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(page::<Resource>(&result).err(), Some(expected), "{result}");
        }
        let failure =
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "no"}});
        assert_eq!(
            parse_page::<Resource>(&failure.to_string(), Limits::default()).err(),
            Some(McpError::ProtocolFailure)
        );
        assert_eq!(
            page::<ResourceTemplate>(
                &json!({"resourceTemplates": [{"uriTemplate": "a://{x}", "name": "x", "title": 1}]})
            )
            .err(),
            Some(McpError::InvalidTemplate)
        );
        assert_eq!(
            page::<Resource>(&json!({"resources": [{"uri": "a://", "name": "a", "size": 1.0e3}]}))
                .map(|page| page.items.len()),
            Ok(1)
        );
    }

    #[test]
    fn serialized_icons_stay_within_the_metadata_limit() {
        let icon = json!({"src": format!("data:image/png;base64,{}", "A".repeat(60 * 1024))});
        let icons = json!([icon, icon, icon]);
        assert_eq!(
            page::<Resource>(&json!({"resources": [{"uri": "a://", "name": "a", "icons": icons}]}))
                .err(),
            Some(McpError::MetadataLimitExceeded)
        );
        assert_eq!(
            page::<ResourceTemplate>(&json!({"resourceTemplates": [{"uriTemplate": "a://{x}", "name": "a", "icons": icons}]}))
                .err(),
            Some(McpError::InvalidTemplate)
        );
        assert_eq!(
            page::<Resource>(
                &json!({"resources": [{"uri": "a://", "name": "a", "icons": [icon]}]})
            )
            .map(|page| page.items.len()),
            Ok(1)
        );
    }

    #[test]
    fn resource_template_catalogs_reject_malformed_and_unsupported_syntax() {
        for uri_template in [
            "db:///{table",
            "db:///table}",
            "db:///{table,id}",
            "db:///{table*}",
            "db:///{table:3}",
            "db:///{table}{/id}",
            "db:///%GG/{table}",
            "db:///{}",
            "db:///{..table}",
            "db:///{ta..ble}",
            "db:///{t{a}}",
            "db:///space {x}",
        ] {
            assert_eq!(
                page::<ResourceTemplate>(
                    &json!({"resourceTemplates": [{"uriTemplate": uri_template, "name": "row"}]})
                )
                .err(),
                Some(McpError::InvalidTemplate),
                "{uri_template}"
            );
        }
        for uri_template in [
            "db:///{table}/{id}",
            "db:///{+path}/x{?q}",
            "db:///{#frag}/x{;p}/{&more}/y{.ext}",
            "file:///{a.b}/%2F",
            "static://no/expressions",
        ] {
            assert!(is_valid_uri_template(uri_template), "{uri_template}");
        }
        let many = "x://".to_owned() + &"/{v}".repeat(MAX_TEMPLATE_EXPRESSIONS);
        assert!(is_valid_uri_template(&many));
        assert!(!is_valid_uri_template(&(many + "/{w}")));
    }

    #[test]
    fn resource_catalog_metadata_enforces_the_shared_json_depth_boundary() {
        let limits = Limits {
            common: common::Limits {
                json_depth: 6,
                ..common::Limits::default()
            },
            ..Limits::default()
        };
        let parse = |meta: Value| {
            parse_page::<Resource>(
                &json!({"jsonrpc": "2.0", "id": 1, "result": {"resources": [{"uri": "memory://x", "name": "x", "_meta": meta}]}}).to_string(),
                limits,
            )
            .map(|page| page.items.len())
        };
        assert_eq!(parse(json!({"value": 1})), Ok(1));
        assert_eq!(parse(json!({"nested": {"value": 1}})), Ok(1));
        assert_eq!(
            parse(json!({"nested": {"again": {"value": 1}}})),
            Err(McpError::JsonDepthLimitExceeded)
        );
    }
}
