use serde_json::Value;

use crate::protocol_negotiation::ElicitationWire;

mod form;
mod secret;
#[cfg(test)]
mod tests;
mod uri;

const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_STRING_BYTES: usize = 64 * 1024;
const MAX_NAME_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElicitationError {
    InvalidRequest,
    InvalidSchema,
    UnsupportedSchema,
    SecretField,
    InvalidUrl,
    InsecureUrl,
    InvalidResponse,
    LimitExceeded,
    UnsupportedMode,
}

type Outcome<T> = Result<T, ElicitationError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Form,
    Url,
}

pub(crate) fn parse_request(wire: ElicitationWire, params: &Value) -> Outcome<Mode> {
    if serialized_len(params) > MAX_REQUEST_BYTES {
        return Err(ElicitationError::LimitExceeded);
    }
    let object = params.as_object().ok_or(ElicitationError::InvalidRequest)?;
    let mode = parse_mode(wire, object.get("mode")).ok_or(ElicitationError::UnsupportedMode)?;
    bounded_string(
        object.get("message"),
        MAX_MESSAGE_BYTES,
        ElicitationError::InvalidRequest,
    )?;
    match mode {
        Mode::Form => {
            let schema = object
                .get("requestedSchema")
                .ok_or(ElicitationError::InvalidRequest)?;
            form::parse_form_schema(wire, schema)?;
        }
        Mode::Url => {
            if object.contains_key("requestedSchema") {
                return Err(ElicitationError::InvalidRequest);
            }
            let url = bounded_string(
                object.get("url"),
                MAX_STRING_BYTES,
                ElicitationError::InvalidRequest,
            )?;
            uri::classify_url(url)?;
            if wire == ElicitationWire::LegacyMcp2025_11 {
                bounded_string(
                    object.get("elicitationId"),
                    MAX_NAME_BYTES,
                    ElicitationError::InvalidRequest,
                )?;
            } else if object.contains_key("elicitationId") {
                return Err(ElicitationError::InvalidRequest);
            }
        }
    }
    Ok(mode)
}

fn parse_mode(wire: ElicitationWire, mode: Option<&Value>) -> Option<Mode> {
    let Some(mode) = mode else {
        return Some(Mode::Form);
    };
    if wire == ElicitationWire::LegacyMcp2025_06 {
        return None;
    }
    match mode.as_str()? {
        "form" => Some(Mode::Form),
        "url" => Some(Mode::Url),
        _ => None,
    }
}

fn bounded_string(
    value: Option<&Value>,
    max_bytes: usize,
    invalid: ElicitationError,
) -> Outcome<&str> {
    match value {
        Some(Value::String(text)) if text.len() <= max_bytes => Ok(text),
        _ => Err(invalid),
    }
}

fn serialized_len(value: &Value) -> usize {
    serde_json::to_string(value).map_or(usize::MAX, |text| text.len())
}
