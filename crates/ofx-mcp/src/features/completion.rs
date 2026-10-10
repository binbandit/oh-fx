use serde_json::{Map, Value, json};

use crate::error::McpError;
use crate::features::common::{self, complete_result, parse_envelope};
use crate::json_number::non_negative_u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) values: usize,
    pub(crate) total_value_bytes: usize,
    pub(crate) value_bytes: usize,
    pub(crate) name_bytes: usize,
    pub(crate) ref_bytes: usize,
    pub(crate) context_arguments: usize,
    pub(crate) context_bytes: usize,
    pub(crate) common: common::Limits,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            values: 100,
            total_value_bytes: 64 * 1024,
            value_bytes: 4096,
            name_bytes: 256,
            ref_bytes: 64 * 1024,
            context_arguments: 128,
            context_bytes: 128 * 1024,
            common: common::Limits::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionReference<'a> {
    Prompt(&'a str),
    ResourceTemplate(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionArgument<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub values: Vec<String>,
    pub total: Option<u64>,
    pub has_more: Option<bool>,
}

pub(crate) fn parse_result(response: &str, limits: Limits) -> Result<CompletionResult, McpError> {
    let envelope = parse_envelope(response, limits.common)?;
    let result = complete_result(&envelope).map_err(|error| match error {
        McpError::ProtocolFailure => error,
        _ => McpError::InvalidResult,
    })?;
    let completion = result
        .get("completion")
        .and_then(Value::as_object)
        .ok_or(McpError::InvalidResult)?;
    let items = completion
        .get("values")
        .ok_or(McpError::InvalidResult)?
        .as_array()
        .filter(|items| items.len() <= limits.values)
        .ok_or(McpError::CompletionLimitExceeded)?;
    let mut values = Vec::with_capacity(items.len());
    let mut total_bytes: usize = 0;
    for item in items {
        let value = item
            .as_str()
            .filter(|value| value.len() <= limits.value_bytes)
            .ok_or(McpError::CompletionByteLimitExceeded)?;
        total_bytes = total_bytes.saturating_add(value.len());
        if total_bytes > limits.total_value_bytes {
            return Err(McpError::CompletionByteLimitExceeded);
        }
        values.push(value.to_owned());
    }
    let total = completion
        .get("total")
        .map(|total| non_negative_u64(total).ok_or(McpError::InvalidResult))
        .transpose()?;
    let has_more = match completion.get("hasMore") {
        None => None,
        Some(Value::Bool(has_more)) => Some(*has_more),
        Some(_) => return Err(McpError::InvalidResult),
    };
    Ok(CompletionResult {
        values,
        total,
        has_more,
    })
}

pub(crate) fn request_params(
    reference: CompletionReference<'_>,
    argument: CompletionArgument<'_>,
    context: &[CompletionArgument<'_>],
    limits: Limits,
) -> Result<Value, McpError> {
    let reference = match reference {
        CompletionReference::Prompt(name) => {
            validate_reference(name, limits)?;
            json!({"type": "ref/prompt", "name": name})
        }
        CompletionReference::ResourceTemplate(uri) => {
            validate_reference(uri, limits)?;
            json!({"type": "ref/resource", "uri": uri})
        }
    };
    validate_argument(argument, limits)?;
    if context.len() > limits.context_arguments {
        return Err(McpError::InvalidContext);
    }
    let mut context_bytes: usize = 0;
    let mut arguments = Map::with_capacity(context.len());
    for item in context {
        validate_argument(*item, limits)?;
        context_bytes = context_bytes
            .saturating_add(item.name.len())
            .saturating_add(item.value.len());
        if context_bytes > limits.context_bytes
            || arguments
                .insert(item.name.to_owned(), json!(item.value))
                .is_some()
        {
            return Err(McpError::InvalidContext);
        }
    }
    let mut params = Map::with_capacity(3);
    params.insert("ref".to_owned(), reference);
    params.insert(
        "argument".to_owned(),
        json!({"name": argument.name, "value": argument.value}),
    );
    if !arguments.is_empty() {
        params.insert("context".to_owned(), json!({"arguments": arguments}));
    }
    Ok(Value::Object(params))
}

fn validate_reference(value: &str, limits: Limits) -> Result<(), McpError> {
    if value.is_empty() || value.len() > limits.ref_bytes {
        return Err(McpError::InvalidReference);
    }
    Ok(())
}

fn validate_argument(argument: CompletionArgument<'_>, limits: Limits) -> Result<(), McpError> {
    if argument.name.is_empty()
        || argument.name.len() > limits.name_bytes
        || argument.value.len() > limits.value_bytes
    {
        return Err(McpError::InvalidArgument);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(result: &Value, limits: Limits) -> Result<CompletionResult, McpError> {
        parse_result(
            &json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string(),
            limits,
        )
    }

    fn argument<'a>(name: &'a str, value: &'a str) -> CompletionArgument<'a> {
        CompletionArgument { name, value }
    }

    #[test]
    fn completion_result_enforces_count_bytes_total_and_has_more() {
        assert_eq!(
            parse(
                &json!({"completion": {"values": ["alpha", "beta"], "total": 3, "hasMore": true}}),
                Limits::default()
            ),
            Ok(CompletionResult {
                values: vec!["alpha".to_owned(), "beta".to_owned()],
                total: Some(3),
                has_more: Some(true),
            })
        );
        assert_eq!(
            parse(
                &json!({"completion": {"values": ["a", "b"]}}),
                Limits {
                    values: 1,
                    ..Limits::default()
                }
            ),
            Err(McpError::CompletionLimitExceeded)
        );
        assert_eq!(
            parse(
                &json!({"completion": {"values": ["alpha", "beta"]}}),
                Limits {
                    total_value_bytes: 4,
                    ..Limits::default()
                }
            ),
            Err(McpError::CompletionByteLimitExceeded)
        );
        assert_eq!(
            parse(&json!({"completion": {"values": []}}), Limits::default()),
            Ok(CompletionResult {
                values: Vec::new(),
                total: None,
                has_more: None,
            })
        );
    }

    #[test]
    fn completion_results_fail_with_upstreams_error_names() {
        let cases = [
            (json!({}), McpError::InvalidResult),
            (json!({"completion": []}), McpError::InvalidResult),
            (json!({"completion": {}}), McpError::InvalidResult),
            (
                json!({"completion": {"values": []}, "resultType": "input_required"}),
                McpError::InvalidResult,
            ),
            (
                json!({"completion": {"values": {}}}),
                McpError::CompletionLimitExceeded,
            ),
            (
                json!({"completion": {"values": vec!["a"; 101]}}),
                McpError::CompletionLimitExceeded,
            ),
            (
                json!({"completion": {"values": [1]}}),
                McpError::CompletionByteLimitExceeded,
            ),
            (
                json!({"completion": {"values": ["x".repeat(4097)]}}),
                McpError::CompletionByteLimitExceeded,
            ),
            (
                json!({"completion": {"values": [], "total": -1}}),
                McpError::InvalidResult,
            ),
            (
                json!({"completion": {"values": [], "total": "3"}}),
                McpError::InvalidResult,
            ),
            (
                json!({"completion": {"values": [], "hasMore": "yes"}}),
                McpError::InvalidResult,
            ),
        ];
        for (result, expected) in cases {
            assert_eq!(
                parse(&result, Limits::default()).err(),
                Some(expected),
                "{result}"
            );
        }
        assert_eq!(
            parse(
                &json!({"completion": {"values": [], "total": 1e3}}),
                Limits::default()
            )
            .map(|result| result.total),
            Ok(Some(1000))
        );
        assert_eq!(
            parse_result(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"no"}}"#,
                Limits::default()
            ),
            Err(McpError::ProtocolFailure)
        );
        assert_eq!(
            parse_result("{", Limits::default()),
            Err(McpError::InvalidEnvelope)
        );
    }

    #[test]
    fn completion_requests_encode_prompt_and_resource_template_references() {
        let limits = Limits::default();
        assert_eq!(
            request_params(
                CompletionReference::Prompt("review"),
                argument("focus", "sec"),
                &[argument("language", "zig")],
                limits
            ),
            Ok(json!({
                "ref": {"type": "ref/prompt", "name": "review"},
                "argument": {"name": "focus", "value": "sec"},
                "context": {"arguments": {"language": "zig"}}
            }))
        );
        assert_eq!(
            request_params(
                CompletionReference::ResourceTemplate("db:///{table}/{id}"),
                argument("table", "us"),
                &[],
                limits
            )
            .map(|params| params.to_string()),
            Ok(
                r#"{"ref":{"type":"ref/resource","uri":"db:///{table}/{id}"},"argument":{"name":"table","value":"us"}}"#
                    .to_owned()
            )
        );
        assert_eq!(
            request_params(
                CompletionReference::Prompt("review"),
                argument("focus", "sec"),
                &[argument("language", "zig")],
                Limits {
                    context_bytes: 2,
                    ..Limits::default()
                }
            ),
            Err(McpError::InvalidContext)
        );
    }

    #[test]
    fn completion_requests_check_the_reference_argument_and_context() {
        let limits = Limits::default();
        let request = |reference, argument, context: &[CompletionArgument<'_>]| {
            request_params(reference, argument, context, limits).err()
        };
        let prompt = CompletionReference::Prompt("review");
        let long_reference = "x".repeat(64 * 1024 + 1);
        let long_name = "x".repeat(257);
        let long_value = "x".repeat(4097);
        assert_eq!(
            request(CompletionReference::Prompt(""), argument("a", ""), &[]),
            Some(McpError::InvalidReference)
        );
        assert_eq!(
            request(
                CompletionReference::ResourceTemplate(&long_reference),
                argument("a", ""),
                &[]
            ),
            Some(McpError::InvalidReference)
        );
        assert_eq!(request(prompt, argument("a", ""), &[]), None);
        for invalid in [
            argument("", "x"),
            argument(&long_name, "x"),
            argument("a", &long_value),
        ] {
            assert_eq!(
                request(prompt, invalid, &[]),
                Some(McpError::InvalidArgument)
            );
            assert_eq!(
                request(prompt, argument("a", ""), &[invalid]),
                Some(McpError::InvalidArgument)
            );
        }
        assert_eq!(
            request(
                prompt,
                argument("a", ""),
                &[argument("b", "1"), argument("b", "2")]
            ),
            Some(McpError::InvalidContext)
        );
        let many = vec![argument("b", "1"); 129];
        assert_eq!(
            request(prompt, argument("a", ""), &many),
            Some(McpError::InvalidContext)
        );
    }
}
