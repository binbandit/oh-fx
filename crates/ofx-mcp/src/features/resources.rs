pub(crate) mod uri_template;

use std::error::Error;
use std::io;

use ofx_jsonrpc::RpcError;
use serde_json::{Map, Value};

use crate::error::McpError;
use crate::features::common::{
    self, CacheHints, Listed, Paging, ResourceContent, optional_string, parse_cache_hints,
    parse_envelope, parse_protocol_error, parse_resource_content, required_string,
    validate_annotations, validate_bounded_json, validate_icons,
};
use crate::json_number::non_negative_u64;
use uri_template::is_valid_uri_template;

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

impl Limits {
    fn paging(self, items: usize) -> Paging {
        Paging {
            pages: self.pages,
            items,
            cursor_bytes: self.cursor_bytes,
            common: self.common,
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

impl Listed for Resource {
    const LIST_METHOD: &'static str = "resources/list";
    const ITEMS_FIELD: &'static str = "resources";

    type Limits = Limits;

    fn paging(limits: Limits) -> Paging {
        limits.paging(limits.resources)
    }

    fn parse_item(value: &Value, limits: Limits) -> Result<Self, McpError> {
        let limits = limits.common;
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

    type Limits = Limits;

    fn paging(limits: Limits) -> Paging {
        limits.paging(limits.templates)
    }

    fn parse_item(value: &Value, limits: Limits) -> Result<Self, McpError> {
        let limits = limits.common;
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

    fn limit_exceeded() -> McpError {
        McpError::TemplateLimitExceeded
    }

    fn duplicate() -> McpError {
        McpError::DuplicateTemplate
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadResult {
    pub(crate) contents: Vec<ResourceContent>,
    pub(crate) cache: CacheHints,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReadOutcome {
    Complete(ReadResult),
    ProtocolFailure(RpcError),
}

pub(crate) fn parse_read_outcome(response: &str, limits: Limits) -> Result<ReadOutcome, McpError> {
    let limits = limits.common;
    let envelope = parse_envelope(response, limits)?;
    if let Some(error) = envelope.get("error") {
        return parse_protocol_error(error, limits).map(ReadOutcome::ProtocolFailure);
    }
    let result = envelope
        .get("result")
        .and_then(Value::as_object)
        .ok_or(McpError::InvalidReadResult)?;
    match result.get("resultType") {
        None => {}
        Some(Value::String(kind)) if kind == "complete" => {}
        Some(Value::String(_)) => return Err(McpError::UnsupportedResultType),
        Some(_) => return Err(McpError::InvalidReadResult),
    }
    let items = result
        .get("contents")
        .ok_or(McpError::InvalidReadResult)?
        .as_array()
        .filter(|items| items.len() <= limits.content_items)
        .ok_or(McpError::ContentLimitExceeded)?;
    let mut contents = Vec::with_capacity(items.len());
    let mut total_bytes: usize = 0;
    for item in items {
        let content = parse_resource_content(item, limits)?;
        total_bytes = total_bytes.saturating_add(content.content_bytes());
        if total_bytes > limits.total_content_bytes {
            return Err(McpError::ContentLimitExceeded);
        }
        contents.push(content);
    }
    Ok(ReadOutcome::Complete(ReadResult {
        contents,
        cache: parse_cache_hints(result)?,
    }))
}

pub(crate) fn stale_fallback_eligible(error: &McpError) -> bool {
    match error {
        McpError::McpConnectionClosed => true,
        McpError::Io(error) => transient_transport_failure(error.as_ref()),
        McpError::Http(error) => {
            !error.is_body() && !error.is_decode() && transient_transport_failure(error.as_ref())
        }
        _ => false,
    }
}

fn transient_transport_failure(error: &(dyn Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
            )
        }) {
            return true;
        }
        current = error.source();
    }
    false
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::features::common::{Catalog, CatalogBuilder, Page, ResourceData, parse_page};

    fn page<T: Listed<Limits = Limits>>(result: &Value) -> Result<Page<T>, McpError> {
        parse_page(
            &json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string(),
            Limits::default(),
        )
    }

    fn catalog<T: Listed<Limits = Limits>>(pages: &[(Value, u64)]) -> Result<Catalog<T>, McpError> {
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
        let one = Limits {
            resources: 1,
            ..Limits::default()
        };
        let mut builder = CatalogBuilder::<Resource>::default();
        let first = json!({"resources": [{"uri": "a://", "name": "a"}], "nextCursor": "next"});
        builder.append_page(page(&first).unwrap(), 0, one).unwrap();
        assert_eq!(
            builder.append_page(page(&first).unwrap(), 0, one),
            Err(McpError::DuplicateCursor)
        );
        let mut builder = CatalogBuilder::<Resource>::default();
        builder.append_page(page(&first).unwrap(), 0, one).unwrap();
        assert_eq!(
            builder.append_page(
                page(&json!({"resources": [{"uri": "b://", "name": "b"}], "cacheScope": "public"}))
                    .unwrap(),
                0,
                one
            ),
            Err(McpError::InconsistentCacheScope)
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
        assert_eq!(
            page::<ResourceTemplate>(
                &json!({"resourceTemplates": [{"uriTemplate": "db:///{table}/{+path}/x{?q}", "name": "row"}]})
            )
            .map(|page| page.items.len()),
            Ok(1)
        );
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

    fn read(result: &Value, limits: Limits) -> Result<ReadOutcome, McpError> {
        parse_read_outcome(
            &json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string(),
            limits,
        )
    }

    fn contents(outcome: Result<ReadOutcome, McpError>) -> Vec<ResourceContent> {
        match outcome {
            Ok(ReadOutcome::Complete(result)) => result.contents,
            other => panic!("not a complete read: {other:?}"),
        }
    }

    #[test]
    fn resource_read_preserves_multiple_text_and_blob_contents() {
        let outcome = read(
            &json!({"contents": [
                {"uri": "git://one", "mimeType": "text/plain", "text": "hello"},
                {"uri": "memory://two", "blob": "aGVsbG8=", "annotations": {"priority": 0.5}, "_meta": {"v": 1}}
            ], "ttlMs": 100}),
            Limits::default(),
        )
        .unwrap();
        let ReadOutcome::Complete(result) = outcome else {
            panic!("not a complete read");
        };
        assert_eq!(
            result.contents,
            [
                ResourceContent {
                    uri: "git://one".to_owned(),
                    mime_type: Some("text/plain".to_owned()),
                    annotations_json: None,
                    metadata_json: None,
                    data: ResourceData::Text("hello".to_owned()),
                },
                ResourceContent {
                    uri: "memory://two".to_owned(),
                    mime_type: None,
                    annotations_json: Some(r#"{"priority":0.5}"#.to_owned()),
                    metadata_json: Some(r#"{"v":1}"#.to_owned()),
                    data: ResourceData::Blob("aGVsbG8=".to_owned()),
                },
            ]
        );
        assert_eq!(result.cache.ttl_ms, Some(100));
        let tight = Limits {
            common: common::Limits {
                total_content_bytes: 4,
                ..common::Limits::default()
            },
            ..Limits::default()
        };
        assert_eq!(
            read(
                &json!({"contents": [{"uri": "git://one", "text": "hello"}]}),
                tight
            ),
            Err(McpError::ContentLimitExceeded)
        );
        assert_eq!(
            contents(read(
                &json!({"contents": [{"uri": "git://a", "text": ""}]}),
                Limits::default()
            ))[0]
                .data,
            ResourceData::Text(String::new())
        );
    }

    #[test]
    fn resource_content_enforces_the_shared_json_depth_boundary() {
        let limits = Limits {
            common: common::Limits {
                json_depth: 6,
                ..common::Limits::default()
            },
            ..Limits::default()
        };
        let parse = |meta: Value| {
            read(
                &json!({"contents": [{"uri": "memory://x", "text": "ok", "_meta": meta}]}),
                limits,
            )
            .map(|outcome| matches!(outcome, ReadOutcome::Complete(_)))
        };
        assert_eq!(parse(json!({"value": 1})), Ok(true));
        assert_eq!(parse(json!({"nested": {"value": 1}})), Ok(true));
        assert_eq!(
            parse(json!({"nested": {"again": {"value": 1}}})),
            Err(McpError::JsonDepthLimitExceeded)
        );
    }

    #[test]
    fn resource_reads_fail_with_upstreams_error_names() {
        let limits = Limits::default();
        let cases = [
            (json!(1), McpError::InvalidReadResult),
            (json!({}), McpError::InvalidReadResult),
            (
                json!({"contents": [], "resultType": 1}),
                McpError::InvalidReadResult,
            ),
            (
                json!({"contents": [], "resultType": "input_required"}),
                McpError::UnsupportedResultType,
            ),
            (json!({"contents": {}}), McpError::ContentLimitExceeded),
            (
                json!({"contents": vec![json!({"uri": "a://", "text": ""}); 257]}),
                McpError::ContentLimitExceeded,
            ),
            (json!({"contents": [1]}), McpError::InvalidContent),
            (
                json!({"contents": [{"uri": "", "text": "x"}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"contents": [{"uri": "a://"}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"contents": [{"uri": "a://", "text": "x", "blob": "AA=="}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"contents": [{"uri": "a://", "text": "x", "blob": "AA==", "_meta": {"big": "x".repeat(128 * 1024)}}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"contents": [{"uri": "a://", "text": 1}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"contents": [{"uri": "a://", "blob": "AB=="}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"contents": [{"uri": "a://", "text": "x", "mimeType": 1}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"contents": [{"uri": "a://", "text": "x", "annotations": {"priority": 2}}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"contents": [{"uri": "a://", "text": "x", "_meta": []}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"contents": [{"uri": "a://", "text": "x", "_meta": {"big": "x".repeat(128 * 1024)}}]}),
                McpError::MetadataLimitExceeded,
            ),
            (
                json!({"contents": [{"uri": "a://", "text": "x".repeat(1024 * 1024 + 1)}]}),
                McpError::InvalidContent,
            ),
            (
                json!({"contents": [], "ttlMs": "1"}),
                McpError::InvalidResult,
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(read(&result, limits), Err(expected), "{result}");
        }
    }

    #[test]
    fn resource_read_protocol_errors_keep_their_code_message_and_bounded_data() {
        let limits = Limits::default();
        let failure = |error: Value| {
            parse_read_outcome(
                &json!({"jsonrpc": "2.0", "id": 1, "error": error}).to_string(),
                limits,
            )
        };
        assert_eq!(
            failure(
                json!({"code": -32002, "message": "Resource not found", "data": {"uri": "a://"}})
            ),
            Ok(ReadOutcome::ProtocolFailure(RpcError {
                code: -32002,
                message: "Resource not found".to_owned(),
                data: Some(json!({"uri": "a://"})),
            }))
        );
        for invalid in [
            json!([]),
            json!({"message": "no"}),
            json!({"code": 1.5, "message": "no"}),
            json!({"code": "1", "message": "no"}),
            json!({"code": 1}),
            json!({"code": 1, "message": "x".repeat(64 * 1024 + 1)}),
        ] {
            assert_eq!(
                failure(invalid.clone()),
                Err(McpError::InvalidEnvelope),
                "{invalid}"
            );
        }
        assert_eq!(
            failure(json!({"code": 1, "message": "no", "data": "x".repeat(128 * 1024)})),
            Err(McpError::MetadataLimitExceeded)
        );
    }

    #[test]
    fn resource_stale_fallback_accepts_only_transient_transport_failures() {
        for kind in [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::TimedOut,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::UnexpectedEof,
        ] {
            assert!(
                stale_fallback_eligible(&McpError::from(io::Error::from(kind))),
                "{kind:?}"
            );
        }
        assert!(stale_fallback_eligible(&McpError::McpConnectionClosed));
        for error in [
            McpError::McpRequestTimedOut,
            McpError::Cancelled,
            McpError::McpRestartLimitReached,
            McpError::McpResourceNotFound,
            McpError::McpFeatureCatalogChanged,
            McpError::ProtocolFailure,
            McpError::from(io::Error::from(io::ErrorKind::ConnectionRefused)),
            McpError::from(io::Error::from(io::ErrorKind::NotFound)),
        ] {
            assert!(!stale_fallback_eligible(&error), "{error}");
        }
    }

    async fn resetting_server(head: &'static [u8]) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            socket.write_all(head).await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            socket.set_zero_linger().unwrap();
        });
        address
    }

    #[tokio::test]
    async fn http_failures_fall_back_only_when_the_connection_breaks_outside_the_body() {
        let client =
            ofx_http::build_connection_client(&ofx_http::ConnectionOptions::default()).unwrap();
        let address = resetting_server(b"").await;
        let before_head = client
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap_err();
        assert!(stale_fallback_eligible(&McpError::from(before_head)));
        let address =
            resetting_server(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\npartial").await;
        let body = client
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap_err();
        assert!(transient_transport_failure(&body));
        assert!(!stale_fallback_eligible(&McpError::from(body)));
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let refused = client
            .get(format!("http://{closed}/"))
            .send()
            .await
            .unwrap_err();
        assert!(!stale_fallback_eligible(&McpError::from(refused)));
    }
}
