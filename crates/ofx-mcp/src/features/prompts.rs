use serde_json::Value;

use crate::error::McpError;
use crate::features::common::{
    self, Listed, Paging, optional_string, required_string, validate_bounded_json, validate_icons,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) pages: usize,
    pub(crate) prompts: usize,
    pub(crate) arguments: usize,
    pub(crate) cursor_bytes: usize,
    pub(crate) common: common::Limits,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pages: 64,
            prompts: 4096,
            arguments: 128,
            cursor_bytes: 4096,
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
}
