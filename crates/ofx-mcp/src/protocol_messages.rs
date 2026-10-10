use std::fmt::Write as _;

use ofx_jsonrpc::{Frame, RequestId};
use serde_json::{Map, Value, json};

use crate::error::McpError;
use crate::mcp_contract::validate_json_rpc_response_envelope;
use crate::mcp_json::write_compact;
use crate::protocol_negotiation::{ElicitationWire, ResponsePayload, classify_response_payload};

pub(crate) const CLIENT_NAME: &str = "oh-fx";
pub(crate) const STDIO_INITIALIZED_NOTIFICATION: &str =
    "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ElicitationCapabilities {
    pub(crate) form: bool,
    pub(crate) url: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ServerCapabilities {
    pub tools_list_changed: bool,
    pub resources: Option<ResourceCapabilities>,
    pub prompts: Option<PromptCapabilities>,
    pub completion: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResourceCapabilities {
    pub list_changed: bool,
    pub subscribe: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PromptCapabilities {
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ServerIdentity {
    pub(crate) name: Option<String>,
    pub(crate) version: Option<String>,
}

pub(crate) fn build_legacy_initialize_request(
    request_id: u64,
    protocol_version: &str,
    negotiated_wire: Option<ElicitationWire>,
    elicitation: ElicitationCapabilities,
    client_version: &str,
) -> String {
    let params = json!({
        "protocolVersion": protocol_version,
        "capabilities": client_capabilities(negotiated_wire, elicitation),
        "clientInfo": {"name": CLIENT_NAME, "version": client_version},
    });
    request_frame(request_id, "initialize", &params)
}

fn client_capabilities(
    negotiated_wire: Option<ElicitationWire>,
    elicitation: ElicitationCapabilities,
) -> Value {
    let mut capabilities = Map::new();
    match negotiated_wire {
        Some(ElicitationWire::LegacyMcp2025_06) if elicitation.form => {
            capabilities.insert("elicitation".to_owned(), json!({}));
        }
        Some(ElicitationWire::LegacyMcp2025_11) if elicitation.form => {
            let mut modes = Map::new();
            modes.insert("form".to_owned(), json!({}));
            if elicitation.url {
                modes.insert("url".to_owned(), json!({}));
            }
            capabilities.insert("elicitation".to_owned(), Value::Object(modes));
        }
        Some(ElicitationWire::LegacyMcp2025_11) if elicitation.url => {
            capabilities.insert("elicitation".to_owned(), json!({"url": {}}));
        }
        _ => {}
    }
    Value::Object(capabilities)
}

pub(crate) fn build_tools_list_request(request_id: u64, cursor: Option<&str>) -> String {
    build_list_request(request_id, "tools/list", cursor)
}

pub(crate) fn build_list_request(request_id: u64, method: &str, cursor: Option<&str>) -> String {
    let params = match cursor {
        Some(cursor) => json!({"cursor": cursor}),
        None => json!({}),
    };
    request_frame(request_id, method, &params)
}

pub(crate) fn build_resource_read_request(request_id: u64, uri: &str) -> String {
    request_frame(request_id, "resources/read", &json!({"uri": uri}))
}

pub(crate) fn build_prompt_get_request(request_id: u64, name: &str, arguments: &Value) -> String {
    request_frame(
        request_id,
        "prompts/get",
        &json!({"name": name, "arguments": arguments}),
    )
}

pub(crate) fn build_tool_call_request(
    request_id: u64,
    original_name: &str,
    arguments_json: &str,
    progress_token: Option<u64>,
) -> String {
    let mut out = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"tools/call\",\"params\":{{",
        wire_id(request_id)
    );
    if let Some(token) = progress_token {
        let _ = write!(out, "\"_meta\":{{\"progressToken\":{token}}},");
    }
    let _ = write!(out, "\"name\":{},\"arguments\":", json!(original_name));
    write_compact(&mut out, arguments_json);
    out.push_str("}}");
    out
}

pub(crate) fn build_cancellation_notification(request_id: u64, reason: &str) -> String {
    Frame::Notification {
        method: "notifications/cancelled",
        params: Some(&json!({"requestId": request_id, "reason": reason})),
    }
    .encode()
}

fn wire_id(request_id: u64) -> RequestId {
    i64::try_from(request_id).map_or_else(
        |_| RequestId::String(request_id.to_string()),
        RequestId::Integer,
    )
}

fn request_frame(request_id: u64, method: &str, params: &Value) -> String {
    let id = wire_id(request_id);
    Frame::Request {
        id: &id,
        method,
        params: Some(params),
    }
    .encode()
}

pub(crate) fn parse_server_capabilities(value: &Value) -> Result<ServerCapabilities, McpError> {
    validate_json_rpc_response_envelope(value)?;
    if value.get("error").is_some() {
        return Ok(ServerCapabilities::default());
    }
    let result = value
        .get("result")
        .and_then(Value::as_object)
        .ok_or(McpError::McpInvalidResult)?;
    let Some(capabilities) = result.get("capabilities") else {
        return Ok(ServerCapabilities::default());
    };
    let capabilities = capabilities.as_object().ok_or(McpError::McpInvalidResult)?;
    let mut parsed = ServerCapabilities::default();
    if let Some(tools) = capabilities.get("tools") {
        let tools = tools.as_object().ok_or(McpError::McpInvalidResult)?;
        parsed.tools_list_changed = optional_capability_bool(tools, "listChanged")?;
    }
    if let Some(resources) = capabilities.get("resources") {
        let resources = resources.as_object().ok_or(McpError::McpInvalidResult)?;
        parsed.resources = Some(ResourceCapabilities {
            list_changed: optional_capability_bool(resources, "listChanged")?,
            subscribe: optional_capability_bool(resources, "subscribe")?,
        });
    }
    if let Some(prompts) = capabilities.get("prompts") {
        let prompts = prompts.as_object().ok_or(McpError::McpInvalidResult)?;
        parsed.prompts = Some(PromptCapabilities {
            list_changed: optional_capability_bool(prompts, "listChanged")?,
        });
    }
    if let Some(completions) = capabilities.get("completions") {
        completions.as_object().ok_or(McpError::McpInvalidResult)?;
        parsed.completion = true;
    }
    Ok(parsed)
}

fn optional_capability_bool(object: &Map<String, Value>, name: &str) -> Result<bool, McpError> {
    match object.get(name) {
        None => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(McpError::McpInvalidResult),
    }
}

pub(crate) fn parse_server_identity(value: &Value) -> Result<ServerIdentity, McpError> {
    validate_json_rpc_response_envelope(value)?;
    let result = value
        .get("result")
        .and_then(Value::as_object)
        .ok_or(McpError::McpInvalidResult)?;
    let modern = match result.get("_meta") {
        Some(meta) => meta
            .as_object()
            .ok_or(McpError::McpInvalidResult)?
            .get("io.modelcontextprotocol/serverInfo"),
        None => None,
    };
    let Some(info) = modern.or_else(|| result.get("serverInfo")) else {
        return Ok(ServerIdentity::default());
    };
    let info = info.as_object().ok_or(McpError::McpInvalidResult)?;
    Ok(ServerIdentity {
        name: non_empty_string(info.get("name"))?,
        version: non_empty_string(info.get("version"))?,
    })
}

fn non_empty_string(value: Option<&Value>) -> Result<Option<String>, McpError> {
    match value {
        None => Ok(None),
        Some(Value::String(text)) => Ok((!text.is_empty()).then(|| text.clone())),
        Some(_) => Err(McpError::McpInvalidResult),
    }
}

pub(crate) fn parse_server_instructions(value: &Value) -> Result<Option<String>, McpError> {
    let result = match classify_response_payload(value)? {
        ResponsePayload::Complete(result) => result,
        ResponsePayload::ProtocolError(error) => return Err(McpError::McpProtocolError(error)),
    };
    let Some(Value::String(instructions)) = result.get("instructions") else {
        return Ok(None);
    };
    let safe = sanitize_model_text(instructions);
    let trimmed = safe.trim_matches([' ', '\t', '\r', '\n']);
    Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
}

fn sanitize_model_text(text: &str) -> String {
    if text.contains('\0') {
        format!(
            "binary or non-utf8 tool output omitted ({} bytes)",
            text.len()
        )
    } else {
        text.to_owned()
    }
}

pub(crate) fn parse_json(bytes: &[u8]) -> Option<Value> {
    serde_json::from_str(std::str::from_utf8(bytes).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_requests_advertise_only_elicitation_capabilities() {
        let none = ElicitationCapabilities::default();
        assert_eq!(
            build_legacy_initialize_request(
                3,
                "2025-11-25",
                Some(ElicitationWire::LegacyMcp2025_11),
                none,
                "1.2.3"
            ),
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"oh-fx\",\"version\":\"1.2.3\"}}}"
        );
        let both = ElicitationCapabilities {
            form: true,
            url: true,
        };
        let url_only = ElicitationCapabilities {
            form: false,
            url: true,
        };
        let capabilities = |wire, elicitation| client_capabilities(wire, elicitation).to_string();
        assert_eq!(
            capabilities(Some(ElicitationWire::LegacyMcp2025_11), both),
            "{\"elicitation\":{\"form\":{},\"url\":{}}}"
        );
        assert_eq!(
            capabilities(Some(ElicitationWire::LegacyMcp2025_11), url_only),
            "{\"elicitation\":{\"url\":{}}}"
        );
        assert_eq!(
            capabilities(Some(ElicitationWire::LegacyMcp2025_06), both),
            "{\"elicitation\":{}}"
        );
        assert_eq!(
            capabilities(Some(ElicitationWire::LegacyMcp2025_06), url_only),
            "{}"
        );
        assert_eq!(capabilities(None, both), "{}");
    }

    #[test]
    fn tools_list_request_carries_pagination_cursor_after_modern_metadata() {
        assert_eq!(
            build_tools_list_request(7, Some("next")),
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/list\",\"params\":{\"cursor\":\"next\"}}"
        );
        assert_eq!(
            build_tools_list_request(1, None),
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}"
        );
        assert_eq!(
            build_list_request(2, "resources/templates/list", Some("")),
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"resources/templates/list\",\"params\":{\"cursor\":\"\"}}"
        );
        assert_eq!(
            build_resource_read_request(7, "git+ssh://host/repo?ref=main#README"),
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"resources/read\",\"params\":{\"uri\":\"git+ssh://host/repo?ref=main#README\"}}"
        );
        assert_eq!(
            build_prompt_get_request(8, "review", &json!({"focus": "caf\u{e9}\n", "depth": "2"})),
            "{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"prompts/get\",\"params\":{\"name\":\"review\",\"arguments\":{\"focus\":\"caf\u{e9}\\n\",\"depth\":\"2\"}}}"
        );
    }

    #[test]
    fn tool_call_requests_compact_arguments_and_carry_progress_tokens() {
        let arguments = "{\n  \"text\": \"line one\\nline two\",\n  \"n\": 2\n}";
        assert_eq!(
            build_tool_call_request(4, "echo", arguments, Some(4)),
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"_meta\":{\"progressToken\":4},\"name\":\"echo\",\"arguments\":{\"text\":\"line one\\nline two\",\"n\":2}}}"
        );
        assert!(!build_tool_call_request(5, "echo", arguments, None).contains("_meta"));
    }

    #[test]
    fn tool_call_arguments_keep_the_number_tokens_and_key_order_the_model_wrote() {
        let arguments = "{\"z\": 1e3, \"id\": 12345678901234567890123, \"ratio\": 1.50, \"a\": -0}";
        assert_eq!(
            build_tool_call_request(6, "lookup", arguments, None),
            "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"lookup\",\"arguments\":{\"z\":1e3,\"id\":12345678901234567890123,\"ratio\":1.50,\"a\":-0}}}"
        );
    }

    #[test]
    fn cancellation_notification_preserves_the_original_request_id() {
        assert_eq!(
            build_cancellation_notification(17, "user \"cancel\""),
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":17,\"reason\":\"user \\\"cancel\\\"\"}}"
        );
    }

    #[test]
    fn server_capabilities_identity_and_instructions_parse_from_initialize_results() {
        let response = json!({"jsonrpc":"2.0","id":0,"result":{
            "protocolVersion":"2025-11-25",
            "capabilities":{"tools":{"listChanged":true},"resources":{"subscribe":true},"prompts":{},"completions":{}},
            "serverInfo":{"name":"fixture","version":""},
            "instructions":"  Use the fixture.\n"
        }});
        assert_eq!(
            parse_server_capabilities(&response),
            Ok(ServerCapabilities {
                tools_list_changed: true,
                resources: Some(ResourceCapabilities {
                    list_changed: false,
                    subscribe: true,
                }),
                prompts: Some(PromptCapabilities {
                    list_changed: false
                }),
                completion: true,
            })
        );
        assert_eq!(
            parse_server_identity(&response),
            Ok(ServerIdentity {
                name: Some("fixture".to_owned()),
                version: None,
            })
        );
        assert_eq!(
            parse_server_instructions(&response),
            Ok(Some("Use the fixture.".to_owned()))
        );
        let invalid = json!({"jsonrpc":"2.0","id":0,"result":{"capabilities":{"tools":{"listChanged":"yes"}}}});
        assert_eq!(
            parse_server_capabilities(&invalid),
            Err(McpError::McpInvalidResult)
        );
        let binary = json!({"jsonrpc":"2.0","id":0,"result":{"instructions":"a\u{0}b"}});
        assert_eq!(
            parse_server_instructions(&binary),
            Ok(Some(
                "binary or non-utf8 tool output omitted (3 bytes)".to_owned()
            ))
        );
    }
}
