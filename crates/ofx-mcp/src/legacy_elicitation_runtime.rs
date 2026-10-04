use std::sync::atomic::{AtomicUsize, Ordering};

use ofx_jsonrpc::{ErrorCode, Frame, RequestId, RpcError};
use serde_json::Value;

use crate::error::McpError;
use crate::protocol_messages::parse_json;

const MAX_SERVER_REQUESTS: usize = 32;
const MAX_SERVER_REQUEST_FRAME_BYTES: usize = 128 * 1024;
const MAX_STRING_ID_BYTES: usize = 256;

#[derive(Debug, Default)]
pub(crate) struct ElicitationContext {
    requests_seen: AtomicUsize,
}

impl ElicitationContext {
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
        if !object.get("params").is_some_and(Value::is_object) {
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

    #[test]
    fn unsupported_client_features_answer_method_not_found() {
        let context = ElicitationContext::default();
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
        let context = ElicitationContext::default();
        let elicit = br#"{"jsonrpc":"2.0","id":"e","method":"elicitation/create","params":{"message":"hi"}}"#;
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
        let bad_id = br#"{"jsonrpc":"2.0","id":{},"method":"elicitation/create"}"#;
        assert_eq!(
            context.respond(bad_id).unwrap(),
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32600,\"message\":\"Invalid request id\"}}"
        );
        for _ in 0..30 {
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
}
