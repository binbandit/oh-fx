use ofx_contract::{ToolArgValue, ToolArgs, ToolOutput};
use serde_json::{Map, Value};

use crate::tool_args::parse_arguments;

const TOOL_NAME: &str = "web_search";
const FIELDS: [&str; 3] = ["query", "allowed_domains", "blocked_domains"];
const DOMAIN_FIELDS: [&str; 2] = ["allowed_domains", "blocked_domains"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DomainFilters {
    allowed: bool,
    blocked: bool,
}

pub(crate) fn decode(arguments: &str) -> Result<DomainFilters, ToolOutput> {
    let fields = parse_arguments(TOOL_NAME, arguments)?;
    if let Some(unknown) = fields.names().find(|name| !FIELDS.contains(name)) {
        return Err(ToolOutput::failure(format!(
            "web_search field \"{unknown}\" is not supported"
        )));
    }
    let query = match fields.get("query") {
        None => {
            return Err(ToolOutput::failure(
                "web_search field \"query\" is required",
            ));
        }
        Some(ToolArgValue::String(query)) => query,
        Some(_) => {
            return Err(ToolOutput::failure(
                "web_search field \"query\" must be a string",
            ));
        }
    };
    if query.chars().count() < 2 {
        return Err(ToolOutput::failure(
            "web_search field \"query\" must contain at least two characters",
        ));
    }
    let document = domain_document(arguments, &fields)?;
    Ok(DomainFilters {
        allowed: has_domains(&fields, document.as_ref(), "allowed_domains")?,
        blocked: has_domains(&fields, document.as_ref(), "blocked_domains")?,
    })
}

pub(crate) fn validate(filters: DomainFilters) -> Result<(), ToolOutput> {
    if filters.allowed && filters.blocked {
        return Err(ToolOutput::failure(
            "web_search accepts only one non-empty domain filter",
        ));
    }
    Ok(())
}

fn domain_document(
    arguments: &str,
    fields: &ToolArgs,
) -> Result<Option<Map<String, Value>>, ToolOutput> {
    if !DOMAIN_FIELDS
        .iter()
        .any(|field| fields.get(field) == Some(&ToolArgValue::Other))
    {
        return Ok(None);
    }
    serde_json::from_str(arguments)
        .map(Some)
        .map_err(|_| ToolOutput::failure("web_search arguments must be valid JSON"))
}

fn has_domains(
    fields: &ToolArgs,
    document: Option<&Map<String, Value>>,
    field: &str,
) -> Result<bool, ToolOutput> {
    let items = match fields.get(field) {
        None => return Ok(false),
        Some(ToolArgValue::Other) => document.and_then(|document| document.get(field)),
        Some(_) => None,
    };
    let Some(Value::Array(items)) = items else {
        return Err(ToolOutput::failure(format!(
            "web_search field \"{field}\" must be an array of strings"
        )));
    };
    if let Some(index) = items.iter().position(|item| !item.is_string()) {
        return Err(ToolOutput::failure(format!(
            "web_search field \"{field}\" item {index} must be a string"
        )));
    }
    Ok(!items.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_failure(arguments: &str) -> String {
        decode(arguments).unwrap_err().content
    }

    #[test]
    fn rejects_invalid_argument_shapes() {
        for (arguments, expected) in [
            ("{", "web_search arguments must be valid JSON"),
            ("[]", "web_search arguments must be an object"),
            (
                "{\"query\":\"ok\",\"extra\":true}",
                "web_search field \"extra\" is not supported",
            ),
            ("{}", "web_search field \"query\" is required"),
            (
                "{\"query\":1}",
                "web_search field \"query\" must be a string",
            ),
            (
                "{\"query\":\"a\"}",
                "web_search field \"query\" must contain at least two characters",
            ),
            (
                "{\"query\":\"x\"}",
                "web_search field \"query\" must contain at least two characters",
            ),
            (
                "{\"query\":\"zig\",\"allowed_domains\":\"example.com\"}",
                "web_search field \"allowed_domains\" must be an array of strings",
            ),
            (
                "{\"query\":\"zig\",\"blocked_domains\":[\"example.com\",1]}",
                "web_search field \"blocked_domains\" item 1 must be a string",
            ),
            (
                "{\"query\":\"current news\",\"allowed_domains\":[1]}",
                "web_search field \"allowed_domains\" item 0 must be a string",
            ),
        ] {
            assert_eq!(decode_failure(arguments), expected, "{arguments}");
        }
    }

    #[test]
    fn rejects_repeated_fields_as_invalid_json() {
        assert_eq!(
            decode_failure(r#"{"query":"x","query":"current news"}"#),
            "web_search arguments must be valid JSON"
        );
        assert_eq!(
            decode_failure(r#"{"query":"news","allowed_domains":[],"allowed_domains":["a.com"]}"#),
            "web_search arguments must be valid JSON"
        );
        for (arguments, expected) in [
            (
                r#"{"query":"news","extra":1e400}"#,
                "web_search field \"extra\" is not supported",
            ),
            (
                r#"{"query":"news","allowed_domains":null}"#,
                "web_search field \"allowed_domains\" must be an array of strings",
            ),
            (
                r#"{"query":"news","blocked_domains":{"a":"b"}}"#,
                "web_search field \"blocked_domains\" must be an array of strings",
            ),
            (
                r#"{"query":"news","blocked_domains":[["a.com"]]}"#,
                "web_search field \"blocked_domains\" item 0 must be a string",
            ),
        ] {
            assert_eq!(decode_failure(arguments), expected, "{arguments}");
        }
    }

    #[test]
    fn counts_query_characters_not_bytes() {
        assert!(decode("{\"query\":\"\u{e9}\"}").is_err());
        assert!(decode("{\"query\":\"\u{e9}\u{e9}\"}").is_ok());
    }

    #[test]
    fn treats_empty_domain_arrays_as_absent_filters_without_syntax_checks() {
        assert_eq!(
            decode(
                "{\"query\":\"zig allocators\",\"allowed_domains\":[\"ziglang.org\",\"example.com\"],\"blocked_domains\":[]}"
            ),
            Ok(DomainFilters {
                allowed: true,
                blocked: false,
            })
        );
        assert_eq!(
            decode(
                "{\"query\":\"current news\",\"allowed_domains\":[\"\", \"https://example.com/path\"]}"
            ),
            Ok(DomainFilters {
                allowed: true,
                blocked: false,
            })
        );
    }

    #[test]
    fn allows_only_one_non_empty_domain_filter() {
        let both = decode("{\"query\":\"zig allocators\",\"allowed_domains\":[\"ziglang.org\"],\"blocked_domains\":[\"example.com\"]}").unwrap();
        assert_eq!(
            validate(both),
            Err(ToolOutput::failure(
                "web_search accepts only one non-empty domain filter"
            ))
        );
        let one = decode("{\"query\":\"zig allocators\",\"allowed_domains\":[],\"blocked_domains\":[\"example.com\"]}").unwrap();
        assert_eq!(validate(one), Ok(()));
    }
}
