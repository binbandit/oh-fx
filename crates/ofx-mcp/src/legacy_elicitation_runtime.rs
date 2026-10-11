use std::sync::atomic::{AtomicUsize, Ordering};

use ofx_jsonrpc::{ErrorCode, Frame, RequestId, RpcError};
use serde_json::Value;

use crate::elicitation::parse_request;
use crate::error::McpError;
use crate::protocol_messages::parse_json;
use crate::protocol_negotiation::ElicitationWire;

const MAX_SERVER_REQUESTS: usize = 32;
const MAX_SERVER_REQUEST_FRAME_BYTES: usize = 128 * 1024;
const MAX_STRING_ID_BYTES: usize = 256;

#[derive(Debug)]
pub(crate) struct ElicitationContext {
    wire: ElicitationWire,
    requests_seen: AtomicUsize,
}

impl ElicitationContext {
    pub(crate) fn new(wire: ElicitationWire) -> Self {
        Self {
            wire,
            requests_seen: AtomicUsize::new(0),
        }
    }

    pub(crate) fn respond(&self, frame: &[u8]) -> Result<String, McpError> {
        if frame.len() > MAX_SERVER_REQUEST_FRAME_BYTES {
            return Err(McpError::McpResponseFrameTooLarge);
        }
        let request = parse_json(frame).ok_or(McpError::McpInvalidJson)?;
        let object = request.as_object().ok_or(McpError::McpInvalidJson)?;
        let id = object.get("id").ok_or(McpError::McpInvalidJson)?;
        if !valid_server_request_id(id) {
            return Ok(error_response(
                &Value::Null,
                ErrorCode::INVALID_REQUEST,
                "Invalid request id",
            ));
        }
        if self.requests_seen.fetch_add(1, Ordering::Relaxed) >= MAX_SERVER_REQUESTS {
            return Ok(error_response(
                id,
                ErrorCode::INTERNAL_ERROR,
                "Elicitation request limit exceeded",
            ));
        }
        if object.get("method").and_then(Value::as_str) != Some("elicitation/create") {
            return Ok(error_response(
                id,
                ErrorCode::METHOD_NOT_FOUND,
                "Method not found",
            ));
        }
        if object
            .get("params")
            .is_none_or(|params| parse_request(self.wire, params).is_err())
        {
            return Ok(error_response(
                id,
                ErrorCode::INVALID_PARAMS,
                "Invalid params",
            ));
        }
        Ok(error_response(
            id,
            ErrorCode::INVALID_PARAMS,
            "Unsupported elicitation mode",
        ))
    }
}

fn valid_server_request_id(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.is_i64(),
        Value::String(text) => text.len() <= MAX_STRING_ID_BYTES,
        _ => false,
    }
}

pub(crate) fn method_not_found_response(frame: &[u8]) -> String {
    let id = parse_json(frame)
        .and_then(|request| request.get("id").cloned())
        .unwrap_or(Value::Null);
    error_response(&id, ErrorCode::METHOD_NOT_FOUND, "Method not found")
}

pub(crate) fn server_request_failed_response(frame: &[u8]) -> String {
    let id = parse_json(frame)
        .and_then(|request| request.get("id").cloned())
        .unwrap_or(Value::Null);
    error_response(&id, ErrorCode::INTERNAL_ERROR, "Server request failed")
}

fn error_response(id: &Value, code: i64, message: &str) -> String {
    let id = RequestId::from_value(id).unwrap_or(RequestId::Null);
    Frame::ErrorResponse {
        id: &id,
        error: &RpcError::new(code, message),
    }
    .encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ElicitationContext {
        ElicitationContext::new(ElicitationWire::LegacyMcp2025_11)
    }

    #[test]
    fn unsupported_client_features_answer_method_not_found() {
        let context = context();
        for method in ["roots/list", "sampling/createMessage", "ping"] {
            let frame =
                format!("{{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"{method}\",\"params\":{{}}}}");
            assert_eq!(
                context.respond(frame.as_bytes()).unwrap(),
                "{\"jsonrpc\":\"2.0\",\"id\":9,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}"
            );
            assert_eq!(
                method_not_found_response(frame.as_bytes()),
                "{\"jsonrpc\":\"2.0\",\"id\":9,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}"
            );
        }
    }

    #[test]
    fn elicitation_without_an_interface_is_refused_with_bounded_requests() {
        let context = context();
        let elicit = br#"{"jsonrpc":"2.0","id":"e","method":"elicitation/create","params":{"message":"hi","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}}}}}"#;
        assert_eq!(
            context.respond(elicit).unwrap(),
            "{\"jsonrpc\":\"2.0\",\"id\":\"e\",\"error\":{\"code\":-32602,\"message\":\"Unsupported elicitation mode\"}}"
        );
        let missing_params = br#"{"jsonrpc":"2.0","id":1,"method":"elicitation/create"}"#;
        assert!(
            context
                .respond(missing_params)
                .unwrap()
                .contains("Invalid params")
        );
        let url = br#"{"jsonrpc":"2.0","id":2,"method":"elicitation/create","params":{"mode":"url","message":"Open","url":"https://example.test/","elicitationId":"e"}}"#;
        assert!(
            context
                .respond(url)
                .unwrap()
                .contains("Unsupported elicitation mode")
        );
        let bad_id = br#"{"jsonrpc":"2.0","id":{},"method":"elicitation/create"}"#;
        assert_eq!(
            context.respond(bad_id).unwrap(),
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32600,\"message\":\"Invalid request id\"}}"
        );
        for _ in 0..29 {
            context.respond(elicit).unwrap();
        }
        assert!(
            context
                .respond(elicit)
                .unwrap()
                .contains("Elicitation request limit exceeded")
        );
        assert_eq!(
            server_request_failed_response(b"not json"),
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"Server request failed\"}}"
        );
    }

    #[test]
    fn malformed_elicitation_requests_are_refused_as_invalid_params() {
        let context = context();
        for params in [
            r#"{"message":"hi"}"#,
            r#"{"message":"hi","requestedSchema":{"type":"object","properties":{"api_key":{"type":"string"}}}}"#,
            r#"{"message":"hi","requestedSchema":{"type":"object","properties":{"value":{"type":"string","pattern":"(?=x)"}}}}"#,
            r#"{"mode":"url","message":"Open","url":"http://example.test/","elicitationId":"e"}"#,
            r#"{"mode":"url","message":"Open","url":"https://example.test/"}"#,
            "[]",
        ] {
            let frame = format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"elicitation/create","params":{params}}}"#
            );
            assert_eq!(
                context.respond(frame.as_bytes()).unwrap(),
                "{\"jsonrpc\":\"2.0\",\"id\":3,\"error\":{\"code\":-32602,\"message\":\"Invalid params\"}}",
                "{params}"
            );
        }
        let legacy = ElicitationContext::new(ElicitationWire::LegacyMcp2025_06);
        let explicit = br#"{"jsonrpc":"2.0","id":4,"method":"elicitation/create","params":{"mode":"form","message":"hi","requestedSchema":{"type":"object","properties":{}}}}"#;
        assert!(legacy.respond(explicit).unwrap().contains("Invalid params"));
    }
}
