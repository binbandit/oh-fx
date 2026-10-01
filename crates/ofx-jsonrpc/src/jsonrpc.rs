use std::fmt;

use serde_json::{Map, Value};

pub struct ErrorCode;

impl ErrorCode {
    pub const INVALID_REQUEST: i64 = -32_600;
    pub const METHOD_NOT_FOUND: i64 = -32_601;
    pub const INVALID_PARAMS: i64 = -32_602;
    pub const INTERNAL_ERROR: i64 = -32_603;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Integer(i64),
    String(String),
    Null,
}

impl RequestId {
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(number) => number.as_i64().map(Self::Integer),
            Value::String(text) => Some(Self::String(text.clone())),
            Value::Null => Some(Self::Null),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Self::Integer(number) => Value::from(*number),
            Self::String(text) => Value::String(text.clone()),
            Self::Null => Value::Null,
        }
    }
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.to_value())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn to_value(&self) -> Value {
        let mut object = Map::new();
        object.insert("code".to_owned(), Value::from(self.code));
        object.insert("message".to_owned(), Value::String(self.message.clone()));
        if let Some(data) = &self.data {
            object.insert("data".to_owned(), data.clone());
        }
        Value::Object(object)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Frame<'a> {
    ErrorResponse {
        id: &'a RequestId,
        error: &'a RpcError,
    },
    Notification {
        method: &'a str,
        params: Option<&'a Value>,
    },
    Request {
        id: &'a RequestId,
        method: &'a str,
        params: Option<&'a Value>,
    },
}

impl Frame<'_> {
    pub fn encode(&self) -> String {
        let mut out = String::from("{\"jsonrpc\":\"2.0\"");
        match self {
            Self::ErrorResponse { id, error } => {
                push_member(&mut out, "id", &id.to_value());
                push_member(&mut out, "error", &error.to_value());
            }
            Self::Notification { method, params } => {
                push_member(&mut out, "method", &Value::from(*method));
                if let Some(params) = params {
                    push_member(&mut out, "params", params);
                }
            }
            Self::Request { id, method, params } => {
                push_member(&mut out, "id", &id.to_value());
                push_member(&mut out, "method", &Value::from(*method));
                if let Some(params) = params {
                    push_member(&mut out, "params", params);
                }
            }
        }
        out.push('}');
        out
    }
}

fn push_member(out: &mut String, name: &str, value: &Value) {
    out.push_str(",\"");
    out.push_str(name);
    out.push_str("\":");
    out.push_str(&value.to_string());
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn frames_encode_protocol_objects() {
        let params = json!({"ok": true});
        assert_eq!(
            Frame::ErrorResponse {
                id: &RequestId::String("req-1".to_owned()),
                error: &RpcError::new(ErrorCode::INVALID_PARAMS, "bad \"params\""),
            }
            .encode(),
            "{\"jsonrpc\":\"2.0\",\"id\":\"req-1\",\"error\":{\"code\":-32602,\"message\":\"bad \\\"params\\\"\"}}"
        );
        let with_data = RpcError {
            data: Some(json!({"reason": "quoted\"value"})),
            ..RpcError::new(ErrorCode::INTERNAL_ERROR, "Server request failed")
        };
        assert_eq!(
            Frame::ErrorResponse {
                id: &RequestId::Integer(7),
                error: &with_data,
            }
            .encode(),
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"error\":{\"code\":-32603,\"message\":\"Server request failed\",\"data\":{\"reason\":\"quoted\\\"value\"}}}"
        );
        assert_eq!(
            Frame::Notification {
                method: "notifications/progress",
                params: Some(&params),
            }
            .encode(),
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"ok\":true}}"
        );
        assert_eq!(
            Frame::Request {
                id: &RequestId::Integer(3),
                method: "tools/list",
                params: Some(&params),
            }
            .encode(),
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\",\"params\":{\"ok\":true}}"
        );
    }

    #[test]
    fn notifications_without_params_omit_the_member() {
        assert_eq!(
            Frame::Notification {
                method: "notifications/initialized",
                params: None,
            }
            .encode(),
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}"
        );
    }

    #[test]
    fn request_ids_display_as_json_and_reject_non_scalar_values() {
        assert_eq!(RequestId::Integer(99).to_string(), "99");
        assert_eq!(RequestId::Null.to_string(), "null");
        assert_eq!(
            RequestId::String("a\"b".to_owned()).to_string(),
            "\"a\\\"b\""
        );
        assert_eq!(RequestId::from_value(&json!(1.5)), None);
        assert_eq!(RequestId::from_value(&json!([1])), None);
        assert_eq!(RequestId::from_value(&json!(null)), Some(RequestId::Null));
    }
}
