use serde_json::Value;

use crate::protocol_negotiation::ElicitationWire;

mod form;
mod secret;
#[cfg(test)]
mod tests;
mod uri;

pub(crate) use form::{Field, FieldKind, FormSchema};

const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_RESPONSE_BYTES: usize = 128 * 1024;
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ElicitationCapabilities {
    pub(crate) form: bool,
    pub(crate) url: bool,
}

impl ElicitationCapabilities {
    pub(crate) fn supports(self, mode: Mode) -> bool {
        match mode {
            Mode::Form => self.form,
            Mode::Url => self.url,
        }
    }

    pub(crate) fn any(self) -> bool {
        self.form || self.url
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    AcceptForm(Vec<(String, String)>),
    AcceptUrl,
    Decline,
    Cancel,
}

impl Answer {
    pub(crate) fn to_json(&self) -> String {
        match self {
            Self::AcceptForm(content) => {
                let fields = content
                    .iter()
                    .map(|(name, json)| format!("{}:{json}", Value::from(name.as_str())))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{{\"action\":\"accept\",\"content\":{{{fields}}}}}")
            }
            Self::AcceptUrl => "{\"action\":\"accept\"}".to_owned(),
            Self::Decline => "{\"action\":\"decline\"}".to_owned(),
            Self::Cancel => "{\"action\":\"cancel\"}".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RequestKind {
    Form(FormSchema),
    Url(UrlRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UrlRequest {
    pub(crate) url: String,
    pub(crate) host: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostClassification {
    Ordinary,
    Punycode,
    NonAscii,
}

pub(crate) fn classify_host(host: &[u8]) -> HostClassification {
    if host
        .split(|byte| *byte == b'.')
        .any(|label| label.len() >= 4 && label[..4].eq_ignore_ascii_case(b"xn--"))
    {
        return HostClassification::Punycode;
    }
    if host.iter().any(|byte| *byte >= 0x80) {
        return HostClassification::NonAscii;
    }
    HostClassification::Ordinary
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Request {
    pub(crate) message: String,
    pub(crate) kind: RequestKind,
}

impl Request {
    pub(crate) fn mode(&self) -> Mode {
        match self.kind {
            RequestKind::Form(_) => Mode::Form,
            RequestKind::Url(_) => Mode::Url,
        }
    }
}

pub(crate) fn parse_request(wire: ElicitationWire, params: &Value) -> Outcome<Request> {
    if serialized_len(params) > MAX_REQUEST_BYTES {
        return Err(ElicitationError::LimitExceeded);
    }
    let object = params.as_object().ok_or(ElicitationError::InvalidRequest)?;
    let mode = parse_mode(wire, object.get("mode")).ok_or(ElicitationError::UnsupportedMode)?;
    let message = bounded_string(
        object.get("message"),
        MAX_MESSAGE_BYTES,
        ElicitationError::InvalidRequest,
    )?
    .to_owned();
    let kind = match mode {
        Mode::Form => {
            let schema = object
                .get("requestedSchema")
                .ok_or(ElicitationError::InvalidRequest)?;
            RequestKind::Form(form::parse_form_schema(wire, schema)?)
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
            let host = uri::classify_url(url)?;
            if wire == ElicitationWire::LegacyMcp2025_11 {
                bounded_string(
                    object.get("elicitationId"),
                    MAX_NAME_BYTES,
                    ElicitationError::InvalidRequest,
                )?;
            } else if object.contains_key("elicitationId") {
                return Err(ElicitationError::InvalidRequest);
            }
            RequestKind::Url(UrlRequest {
                url: url.to_owned(),
                host,
            })
        }
    };
    Ok(Request { message, kind })
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

pub(crate) fn validate_answer(request: &Request, answer: &Answer) -> Outcome<()> {
    if answer.to_json().len() > MAX_RESPONSE_BYTES {
        return Err(ElicitationError::LimitExceeded);
    }
    match (answer, &request.kind) {
        (Answer::Decline | Answer::Cancel, _) | (Answer::AcceptUrl, RequestKind::Url(_)) => Ok(()),
        (Answer::AcceptForm(content), RequestKind::Form(form)) => form.validate_answers(content),
        (Answer::AcceptForm(_), RequestKind::Url(_))
        | (Answer::AcceptUrl, RequestKind::Form(_)) => Err(ElicitationError::InvalidResponse),
    }
}

pub(crate) fn validate_field_json(field: &Field, value_json: &str) -> Outcome<()> {
    if value_json.len() > MAX_RESPONSE_BYTES {
        return Err(ElicitationError::LimitExceeded);
    }
    if matches!(field.kind, FieldKind::Number | FieldKind::Integer) {
        return field.validate_number_lexeme(value_json.trim_matches([' ', '\t', '\n', '\r']));
    }
    let value: Value =
        serde_json::from_str(value_json).map_err(|_| ElicitationError::InvalidResponse)?;
    field.validate_value(&value)
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
