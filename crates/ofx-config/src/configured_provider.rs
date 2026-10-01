use std::net::Ipv6Addr;

use serde_json::{Map, Value};

use crate::header_template::HeaderTemplate;
use crate::model_capabilities::Capabilities;

const MAX_PROVIDERS: usize = 32;
const MAX_MODELS: usize = 256;
const MAX_ID_BYTES: usize = 64;
pub const MAX_MODEL_BYTES: usize = 1024;
const MAX_URL_BYTES: usize = 2048;
pub(crate) const MAX_ENV_BYTES: usize = 128;
const MAX_HEADERS: usize = 64;
const MAX_PATH_BYTES: usize = 4096;
const RESERVED_PROVIDER_IDS: [&str; 3] = ["gateway", "codex", "grok"];
const DEFINITION_FIELDS: [&str; 11] = [
    "protocol",
    "base_url",
    "auth",
    "tool_choice_mode",
    "max_tokens_parameter",
    "reviewer_model",
    "model_metadata",
    "headers",
    "tls",
    "proxy",
    "models",
];
const PATH_PUNCTUATION: &[u8] = b"/-._~!$&'()*+,;=:@";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfiguredProviderError {
    #[error("InvalidJson")]
    InvalidJson,
    #[error("DuplicateField")]
    DuplicateField,
    #[error("LimitExceeded")]
    LimitExceeded,
    #[error("InvalidObject")]
    InvalidObject,
    #[error("UnknownField")]
    UnknownField,
    #[error("MissingField")]
    MissingField,
    #[error("InvalidProviderId")]
    InvalidProviderId,
    #[error("ReservedProviderId")]
    ReservedProviderId,
    #[error("InvalidProtocol")]
    InvalidProtocol,
    #[error("InvalidBaseUrl")]
    InvalidBaseUrl,
    #[error("InsecureBaseUrl")]
    InsecureBaseUrl,
    #[error("InvalidAuth")]
    InvalidAuth,
    #[error("InvalidEnvironmentName")]
    InvalidEnvironmentName,
    #[error("InvalidToolChoiceMode")]
    InvalidToolChoiceMode,
    #[error("InvalidMaxTokensParameter")]
    InvalidMaxTokensParameter,
    #[error("InvalidModelId")]
    InvalidModelId,
    #[error("InvalidModelMetadata")]
    InvalidModelMetadata,
    #[error("InvalidHeaderName")]
    InvalidHeaderName,
    #[error("InvalidHeaderValue")]
    InvalidHeaderValue,
    #[error("ReservedHeader")]
    ReservedHeader,
    #[error("InvalidTls")]
    InvalidTls,
    #[error("InvalidProxy")]
    InvalidProxy,
}

type ParseResult<T> = Result<T, ConfiguredProviderError>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolChoiceMode {
    #[default]
    Omit,
    Send,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MaxTokensParameter {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
}

impl MaxTokensParameter {
    pub const fn field(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderAuth {
    None,
    Bearer { env: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelMetadata {
    pub(crate) id: String,
    pub(crate) context_window: Option<u32>,
    pub(crate) max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDefinition {
    pub(crate) id: String,
    pub(crate) base_url: String,
    pub(crate) auth: ProviderAuth,
    pub(crate) tool_choice_mode: ToolChoiceMode,
    pub(crate) max_tokens_parameter: MaxTokensParameter,
    pub(crate) model_metadata: Vec<ModelMetadata>,
    pub(crate) headers: Vec<HeaderTemplate>,
    pub(crate) ca_file: Option<String>,
    pub(crate) proxy: Option<String>,
    pub(crate) models: Vec<String>,
}

impl ProviderDefinition {
    pub(crate) fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    pub fn capabilities(&self, model: &str) -> Capabilities {
        self.model_metadata
            .iter()
            .find(|metadata| metadata.id == model)
            .map_or_else(Capabilities::default, |metadata| Capabilities {
                context_window: metadata.context_window,
                max_output_tokens: metadata.max_output_tokens,
            })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProviderRegistry {
    definitions: Vec<ProviderDefinition>,
}

#[cfg(test)]
const MAX_JSON_BYTES: usize = 1024 * 1024;

#[cfg(test)]
impl ProviderRegistry {
    pub(crate) fn parse_json(json: &[u8]) -> Result<Self, ConfiguredProviderError> {
        use crate::strict_json::{self, StrictJsonError};
        if json.len() > MAX_JSON_BYTES {
            return Err(ConfiguredProviderError::LimitExceeded);
        }
        let value = strict_json::parse(json).map_err(|error| match error {
            StrictJsonError::DuplicateField => ConfiguredProviderError::DuplicateField,
            StrictJsonError::Syntax => ConfiguredProviderError::InvalidJson,
        })?;
        Self::parse(&value)
    }
}

impl ProviderRegistry {
    pub(crate) fn parse(providers: &Value) -> ParseResult<Self> {
        let entries = providers
            .as_object()
            .ok_or(ConfiguredProviderError::InvalidObject)?;
        if entries.len() > MAX_PROVIDERS {
            return Err(ConfiguredProviderError::LimitExceeded);
        }
        let definitions = entries
            .iter()
            .map(|(id, value)| parse_definition(id, value))
            .collect::<ParseResult<_>>()?;
        Ok(Self { definitions })
    }

    pub(crate) fn get(&self, id: &str) -> Option<&ProviderDefinition> {
        self.definitions
            .iter()
            .find(|definition| definition.id == id)
    }
}

fn parse_definition(id: &str, value: &Value) -> ParseResult<ProviderDefinition> {
    validate_id(id)?;
    let fields = checked_fields(value, &DEFINITION_FIELDS)?;
    match required(fields, "protocol")? {
        Value::String(protocol) if protocol == "openai-chat-completions" => {}
        _ => return Err(ConfiguredProviderError::InvalidProtocol),
    }
    let Value::String(url) = required(fields, "base_url")? else {
        return Err(ConfiguredProviderError::InvalidBaseUrl);
    };
    let base_url = validate_url(url)?.to_owned();
    let auth = parse_auth(required(fields, "auth")?)?;
    let tool_choice_mode = match fields.get("tool_choice_mode") {
        None => ToolChoiceMode::Omit,
        Some(Value::String(mode)) if mode == "omit" => ToolChoiceMode::Omit,
        Some(Value::String(mode)) if mode == "send" => ToolChoiceMode::Send,
        Some(_) => return Err(ConfiguredProviderError::InvalidToolChoiceMode),
    };
    let max_tokens_parameter = match fields.get("max_tokens_parameter") {
        None => MaxTokensParameter::MaxTokens,
        Some(Value::String(field)) if field == "max_tokens" => MaxTokensParameter::MaxTokens,
        Some(Value::String(field)) if field == "max_completion_tokens" => {
            MaxTokensParameter::MaxCompletionTokens
        }
        Some(_) => return Err(ConfiguredProviderError::InvalidMaxTokensParameter),
    };
    match fields.get("reviewer_model") {
        Some(Value::String(model)) => validate_model_id(model)?,
        Some(_) => return Err(ConfiguredProviderError::InvalidModelId),
        None => {}
    }
    let model_metadata = fields
        .get("model_metadata")
        .map(parse_metadata)
        .transpose()?
        .unwrap_or_default();
    let headers = fields
        .get("headers")
        .map(|headers| parse_headers(headers, &auth))
        .transpose()?
        .unwrap_or_default();
    let ca_file = fields.get("tls").map(parse_tls).transpose()?.flatten();
    let proxy = fields.get("proxy").map(parse_proxy).transpose()?;
    let models = fields
        .get("models")
        .map(parse_models)
        .transpose()?
        .unwrap_or_default();
    Ok(ProviderDefinition {
        id: id.to_owned(),
        base_url,
        auth,
        tool_choice_mode,
        max_tokens_parameter,
        model_metadata,
        headers,
        ca_file,
        proxy,
        models,
    })
}

fn parse_auth(value: &Value) -> ParseResult<ProviderAuth> {
    let fields = checked_fields(value, &["type", "env"])?;
    let Value::String(kind) = required(fields, "type")? else {
        return Err(ConfiguredProviderError::InvalidAuth);
    };
    if kind == "none" {
        if fields.contains_key("env") {
            return Err(ConfiguredProviderError::InvalidAuth);
        }
        return Ok(ProviderAuth::None);
    }
    if kind != "bearer" {
        return Err(ConfiguredProviderError::InvalidAuth);
    }
    let Value::String(env) = required(fields, "env")? else {
        return Err(ConfiguredProviderError::InvalidEnvironmentName);
    };
    validate_environment_name(env)?;
    Ok(ProviderAuth::Bearer { env: env.clone() })
}

pub(crate) fn validate_environment_name(name: &str) -> ParseResult<()> {
    if name.len() > MAX_ENV_BYTES {
        return Err(ConfiguredProviderError::LimitExceeded);
    }
    if is_environment_name(name) {
        Ok(())
    } else {
        Err(ConfiguredProviderError::InvalidEnvironmentName)
    }
}

pub(crate) fn is_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn parse_metadata(value: &Value) -> ParseResult<Vec<ModelMetadata>> {
    let entries = value
        .as_object()
        .ok_or(ConfiguredProviderError::InvalidObject)?;
    if entries.len() > MAX_MODELS {
        return Err(ConfiguredProviderError::LimitExceeded);
    }
    entries
        .iter()
        .map(|(id, metadata)| {
            validate_model_id(id)?;
            let fields = checked_fields(
                metadata,
                &[
                    "context_window",
                    "max_output_tokens",
                    "supports_tool_use",
                    "supports_vision",
                ],
            )?;
            let context_window = positive_limit(fields.get("context_window"))?;
            let max_output_tokens = positive_limit(fields.get("max_output_tokens"))?;
            if let (Some(context), Some(output)) = (context_window, max_output_tokens)
                && output >= context
            {
                return Err(ConfiguredProviderError::InvalidModelMetadata);
            }
            optional_bool(fields.get("supports_tool_use"))?;
            optional_bool(fields.get("supports_vision"))?;
            Ok(ModelMetadata {
                id: id.clone(),
                context_window,
                max_output_tokens,
            })
        })
        .collect()
}

fn positive_limit(value: Option<&Value>) -> ParseResult<Option<u32>> {
    value
        .map(|present| {
            present
                .as_i64()
                .filter(|number| *number > 0)
                .and_then(|number| u32::try_from(number).ok())
                .ok_or(ConfiguredProviderError::InvalidModelMetadata)
        })
        .transpose()
}

fn optional_bool(value: Option<&Value>) -> ParseResult<Option<bool>> {
    value
        .map(|present| {
            present
                .as_bool()
                .ok_or(ConfiguredProviderError::InvalidModelMetadata)
        })
        .transpose()
}

fn parse_headers(value: &Value, auth: &ProviderAuth) -> ParseResult<Vec<HeaderTemplate>> {
    let entries = value
        .as_object()
        .ok_or(ConfiguredProviderError::InvalidObject)?;
    if entries.len() > MAX_HEADERS {
        return Err(ConfiguredProviderError::LimitExceeded);
    }
    let mut headers: Vec<HeaderTemplate> = Vec::with_capacity(entries.len());
    for (name, template) in entries {
        let Value::String(template) = template else {
            return Err(ConfiguredProviderError::InvalidHeaderValue);
        };
        let header = HeaderTemplate::parse(name, template, auth)?;
        if headers
            .iter()
            .any(|prior| prior.name().eq_ignore_ascii_case(name))
        {
            return Err(ConfiguredProviderError::DuplicateField);
        }
        headers.push(header);
    }
    Ok(headers)
}

fn parse_tls(value: &Value) -> ParseResult<Option<String>> {
    let fields = value
        .as_object()
        .ok_or(ConfiguredProviderError::InvalidObject)?;
    if fields.keys().any(|key| key != "ca_file") {
        return Err(ConfiguredProviderError::UnknownField);
    }
    fields
        .get("ca_file")
        .map(|path| match path {
            Value::String(path)
                if !path.is_empty()
                    && path.len() <= MAX_PATH_BYTES
                    && !path.contains('\0')
                    && (path.starts_with('/') || path.starts_with("~/")) =>
            {
                Ok(path.clone())
            }
            _ => Err(ConfiguredProviderError::InvalidTls),
        })
        .transpose()
}

fn parse_proxy(value: &Value) -> ParseResult<String> {
    let Value::String(proxy) = value else {
        return Err(ConfiguredProviderError::InvalidProxy);
    };
    let lowercase = proxy.to_ascii_lowercase();
    let has_scheme = lowercase.starts_with("http://") || lowercase.starts_with("https://");
    let printable = proxy.bytes().all(|byte| (0x21..0x7f).contains(&byte));
    if proxy.len() > MAX_URL_BYTES || !has_scheme || !printable {
        return Err(ConfiguredProviderError::InvalidProxy);
    }
    Ok(proxy.clone())
}

fn parse_models(value: &Value) -> ParseResult<Vec<String>> {
    let Value::Array(items) = value else {
        return Err(ConfiguredProviderError::InvalidModelId);
    };
    if items.len() > MAX_MODELS {
        return Err(ConfiguredProviderError::LimitExceeded);
    }
    let mut models: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let Value::String(model) = item else {
            return Err(ConfiguredProviderError::InvalidModelId);
        };
        validate_model_id(model)?;
        if models.contains(model) {
            return Err(ConfiguredProviderError::DuplicateField);
        }
        models.push(model.clone());
    }
    Ok(models)
}

fn checked_fields<'a>(value: &'a Value, allowed: &[&str]) -> ParseResult<&'a Map<String, Value>> {
    let fields = value
        .as_object()
        .ok_or(ConfiguredProviderError::InvalidObject)?;
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ConfiguredProviderError::UnknownField);
    }
    Ok(fields)
}

fn required<'a>(fields: &'a Map<String, Value>, name: &str) -> ParseResult<&'a Value> {
    fields
        .get(name)
        .ok_or(ConfiguredProviderError::MissingField)
}

pub(crate) fn validate_id(id: &str) -> ParseResult<()> {
    if id.len() > MAX_ID_BYTES {
        return Err(ConfiguredProviderError::LimitExceeded);
    }
    let mut bytes = id.bytes();
    let well_formed = bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if !well_formed {
        return Err(ConfiguredProviderError::InvalidProviderId);
    }
    if RESERVED_PROVIDER_IDS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(id))
    {
        return Err(ConfiguredProviderError::ReservedProviderId);
    }
    Ok(())
}

pub fn is_valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MODEL_BYTES
        && id.trim_matches([' ', '\t', '\r', '\n']).len() == id.len()
        && !id.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
}

pub(crate) fn validate_model_id(id: &str) -> ParseResult<()> {
    if id.len() > MAX_MODEL_BYTES {
        return Err(ConfiguredProviderError::LimitExceeded);
    }
    if is_valid_model_id(id) {
        Ok(())
    } else {
        Err(ConfiguredProviderError::InvalidModelId)
    }
}

fn validate_url(url: &str) -> ParseResult<&str> {
    if url.len() > MAX_URL_BYTES {
        return Err(ConfiguredProviderError::LimitExceeded);
    }
    let invalid = ConfiguredProviderError::InvalidBaseUrl;
    if url
        .bytes()
        .any(|byte| byte <= 0x20 || byte >= 0x7f || matches!(byte, b'\\' | b'?' | b'#'))
    {
        return Err(invalid);
    }
    let (scheme, rest) = url.split_once(':').ok_or(invalid)?;
    if !is_scheme(scheme) {
        return Err(invalid);
    }
    let after_slashes = rest.strip_prefix("//").ok_or(invalid)?;
    let authority_end = after_slashes.find('/').unwrap_or(after_slashes.len());
    let (authority, path) = after_slashes.split_at(authority_end);
    if authority.contains('@') {
        return Err(invalid);
    }
    let (host, port) = split_host_port(authority)?;
    validate_port(port)?;
    validate_host(host)?;
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return Err(ConfiguredProviderError::InsecureBaseUrl);
    }
    validate_path(path)?;
    Ok(url.strip_suffix('/').unwrap_or(url))
}

fn is_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

fn split_host_port(authority: &str) -> ParseResult<(&str, Option<&str>)> {
    let invalid = ConfiguredProviderError::InvalidBaseUrl;
    let (host, remainder) = if authority.starts_with('[') {
        let close = authority.find(']').ok_or(invalid)?;
        authority.split_at(close + 1)
    } else {
        let colon = authority.find(':').unwrap_or(authority.len());
        authority.split_at(colon)
    };
    if host.is_empty() {
        return Err(invalid);
    }
    if remainder.is_empty() {
        return Ok((host, None));
    }
    let port = remainder.strip_prefix(':').ok_or(invalid)?;
    Ok((host, Some(port)))
}

fn validate_port(port: Option<&str>) -> ParseResult<()> {
    let Some(port) = port else {
        return Ok(());
    };
    let digits_only =
        !port.is_empty() && port.len() <= 5 && port.bytes().all(|byte| byte.is_ascii_digit());
    match port.parse::<u16>() {
        Ok(number) if digits_only && number != 0 => Ok(()),
        _ => Err(ConfiguredProviderError::InvalidBaseUrl),
    }
}

fn validate_host(host: &str) -> ParseResult<()> {
    let invalid = ConfiguredProviderError::InvalidBaseUrl;
    if let Some(bracketed) = host.strip_prefix('[') {
        let literal = bracketed.strip_suffix(']').ok_or(invalid)?;
        return literal.parse::<Ipv6Addr>().map(|_| ()).map_err(|_| invalid);
    }
    if host.len() > 253 {
        return Err(invalid);
    }
    let labels_valid = host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    });
    if labels_valid { Ok(()) } else { Err(invalid) }
}

fn validate_path(path: &str) -> ParseResult<()> {
    let invalid = ConfiguredProviderError::InvalidBaseUrl;
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let escape = bytes.get(index + 1..index + 3).ok_or(invalid)?;
            if !escape.iter().all(u8::is_ascii_hexdigit) {
                return Err(invalid);
            }
            let decoded = u8::from_str_radix(std::str::from_utf8(escape).map_err(|_| invalid)?, 16)
                .map_err(|_| invalid)?;
            if decoded < 0x20 || decoded == 0x7f {
                return Err(invalid);
            }
            index += 3;
        } else if byte.is_ascii_alphanumeric() || PATH_PUNCTUATION.contains(&byte) {
            index += 1;
        } else {
            return Err(invalid);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_JSON: &str = r#"{"local":{"protocol":"openai-chat-completions","base_url":"http://localhost:11434/v1/","auth":{"type":"none"}},
"router":{"protocol":"openai-chat-completions","base_url":"https://openrouter.ai/api/v1","auth":{"type":"bearer","env":"OPENROUTER_API_KEY"},"tool_choice_mode":"send","reviewer_model":"openai/review","model_metadata":{"openai/gpt-4.1":{"context_window":8192,"max_output_tokens":1024,"supports_tool_use":true,"supports_vision":false},"unknown":{}}}}"#;

    const TEST_REQUIRED_FIELDS: &str = r#""protocol":"openai-chat-completions","base_url":"https://example.com/v1","auth":{"type":"none"}"#;

    fn parse(json: &str) -> Result<ProviderRegistry, ConfiguredProviderError> {
        ProviderRegistry::parse_json(json.as_bytes())
    }

    fn with_required(extra: &str) -> String {
        format!(r#"{{"local":{{{TEST_REQUIRED_FIELDS},{extra}}}}}"#)
    }

    #[test]
    fn configured_provider_owns_definitions_and_preserves_unknown_metadata() {
        let registry = parse(TEST_JSON).unwrap();
        assert_eq!(registry.definitions.len(), 2);
        let local = registry.get("local").unwrap();
        assert_eq!(local.auth, ProviderAuth::None);
        assert_eq!(local.tool_choice_mode, ToolChoiceMode::Omit);
        assert_eq!(local.max_tokens_parameter, MaxTokensParameter::MaxTokens);
        assert_eq!(
            local.chat_url(),
            "http://localhost:11434/v1/chat/completions"
        );
        let router = registry.get("router").unwrap();
        assert_eq!(
            router.auth,
            ProviderAuth::Bearer {
                env: "OPENROUTER_API_KEY".to_owned()
            }
        );
        assert_eq!(router.tool_choice_mode, ToolChoiceMode::Send);
        assert_eq!(
            router.capabilities("openai/gpt-4.1"),
            Capabilities {
                context_window: Some(8192),
                max_output_tokens: Some(1024),
            }
        );
        assert_eq!(router.capabilities("unknown"), Capabilities::default());
        assert_eq!(router.capabilities("missing"), Capabilities::default());
        assert!(registry.get("Router").is_none());
    }

    #[test]
    fn configured_provider_url_policy_and_prefix_normalization() {
        let valid = [
            ("https://example.com", "https://example.com"),
            ("https://example.com/", "https://example.com"),
            ("https://example.com/api/v1/", "https://example.com/api/v1"),
            (
                "https://example.com/a//b/%2f/",
                "https://example.com/a//b/%2f",
            ),
            ("http://localhost", "http://localhost"),
            ("http://127.0.0.1:80/v1", "http://127.0.0.1:80/v1"),
            ("http://[::1]:65535/v1", "http://[::1]:65535/v1"),
            ("HTTPS://Example.com/API/V1", "HTTPS://Example.com/API/V1"),
            ("https://[2001:db8::1]/v1", "https://[2001:db8::1]/v1"),
            ("http://example.com", "http://example.com"),
            (
                "http://portkey.internal:8787/v1/",
                "http://portkey.internal:8787/v1",
            ),
            ("http://10.0.0.7/v1", "http://10.0.0.7/v1"),
            (
                "http://portkey_gateway.svc:8787/v1",
                "http://portkey_gateway.svc:8787/v1",
            ),
        ];
        for (input, expected) in valid {
            assert_eq!(validate_url(input), Ok(expected), "{input}");
        }
        let invalid = [
            "",
            "https:/example.com",
            "https://",
            "https://user:secret@example.com",
            "https://@example.com",
            "https://example.com?",
            "https://example.com#",
            "https://example.com/\r\nx",
            "https://example.com/a b",
            "https://example.com\\evil",
            "https://example.com/%0a",
            "https://example.com/%7f",
            "https://example.com/%",
            "https://example.com/%zz",
            "https://example.com:",
            "https://example.com:+80",
            "https://example.com:-1",
            "https://example.com:8_0",
            "https://example.com:65536",
            "https://example.com:0",
            "https://example.com:abc",
            "https://[::1]extra",
            "https://[::1]extra:80",
            "https://[not-ip]:80",
            "https://::1",
            "https://%6cocalhost",
            "https://bad..host",
            "https://-bad.host",
            "https://bad.host-",
        ];
        for url in invalid {
            assert_eq!(
                validate_url(url),
                Err(ConfiguredProviderError::InvalidBaseUrl),
                "{url}"
            );
        }
        assert_eq!(
            validate_url("ftp://example.com"),
            Err(ConfiguredProviderError::InsecureBaseUrl)
        );
    }

    #[test]
    fn configured_provider_duplicate_keys_are_rejected_before_value_loses_evidence() {
        for json in [
            r#"{"local":{},"local":{}}"#,
            r#"{"local":{"protocol":1,"protocol":2}}"#,
            r#"{"local":{"auth":{"type":"none","type":"bearer"}}}"#,
            r#"{"local":{"model_metadata":{"model":{},"model":{}}}}"#,
            r#"{"local":{"model_metadata":{"model":{"supports_vision":true,"supports_vision":false}}}}"#,
            r#"{"local":{},"local":{}}"#,
        ] {
            assert_eq!(
                parse(json),
                Err(ConfiguredProviderError::DuplicateField),
                "{json}"
            );
        }
    }

    #[test]
    fn configured_provider_invalid_schemas_fail_explicitly() {
        use ConfiguredProviderError as E;
        let documents = [
            ("{", E::InvalidJson),
            ("null", E::InvalidObject),
            ("[]", E::InvalidObject),
            (r#"{"":{}}"#, E::InvalidProviderId),
            (r#"{"1local":{}}"#, E::InvalidProviderId),
            (r#"{"local.host":{}}"#, E::InvalidProviderId),
            (r#"{"local/host":{}}"#, E::InvalidProviderId),
            (r#"{" local":{}}"#, E::InvalidProviderId),
            (r#"{"GATEWAY":{}}"#, E::ReservedProviderId),
            (r#"{"codex":{}}"#, E::ReservedProviderId),
            (r#"{"Grok":{}}"#, E::ReservedProviderId),
            (r#"{"local":null}"#, E::InvalidObject),
            (r#"{"local":{}}"#, E::MissingField),
            (r#"{"local":{"protocol":"responses"}}"#, E::InvalidProtocol),
            (r#"{"local":{"protocol":false}}"#, E::InvalidProtocol),
            (
                r#"{"local":{"protocol":"openai-chat-completions","base_url":null}}"#,
                E::InvalidBaseUrl,
            ),
        ];
        for (json, error) in documents {
            assert_eq!(parse(json), Err(error), "{json}");
        }
        let fields = [
            (r#""secret":"not-allowed""#, E::UnknownField),
            (r#""tool_choice_mode":"auto""#, E::InvalidToolChoiceMode),
            (r#""tool_choice_mode":null"#, E::InvalidToolChoiceMode),
            (
                r#""max_tokens_parameter":"max_output_tokens""#,
                E::InvalidMaxTokensParameter,
            ),
            (
                r#""max_tokens_parameter":null"#,
                E::InvalidMaxTokensParameter,
            ),
            (r#""reviewer_model":null"#, E::InvalidModelId),
            (r#""reviewer_model":"""#, E::InvalidModelId),
            (r#""reviewer_model":"bad\nmodel""#, E::InvalidModelId),
            (r#""model_metadata":null"#, E::InvalidObject),
            (r#""model_metadata":{"":{}}"#, E::InvalidModelId),
            (r#""model_metadata":{"m":[]}"#, E::InvalidObject),
        ];
        for (extra, error) in fields {
            assert_eq!(parse(&with_required(extra)), Err(error), "{extra}");
        }
        let metadata = [
            (r#"{"context_window":0}"#, E::InvalidModelMetadata),
            (r#"{"context_window":-1}"#, E::InvalidModelMetadata),
            (r#"{"context_window":1.5}"#, E::InvalidModelMetadata),
            (r#"{"context_window":4294967296}"#, E::InvalidModelMetadata),
            (r#"{"max_output_tokens":null}"#, E::InvalidModelMetadata),
            (r#"{"max_output_tokens":"10"}"#, E::InvalidModelMetadata),
            (
                r#"{"context_window":1,"max_output_tokens":1}"#,
                E::InvalidModelMetadata,
            ),
            (
                r#"{"context_window":2,"max_output_tokens":3}"#,
                E::InvalidModelMetadata,
            ),
            (r#"{"supports_tool_use":1}"#, E::InvalidModelMetadata),
            (r#"{"supports_vision":null}"#, E::InvalidModelMetadata),
            (r#"{"supports_search":true}"#, E::UnknownField),
        ];
        for (model, error) in metadata {
            let extra = format!(r#""model_metadata":{{"m":{model}}}"#);
            assert_eq!(parse(&with_required(&extra)), Err(error), "{model}");
        }
    }

    #[test]
    fn configured_provider_auth_admits_only_explicit_none_or_a_portable_environment_slot() {
        use ConfiguredProviderError as E;
        let cases = [
            ("null", E::InvalidObject),
            ("{}", E::MissingField),
            (r#"{"type":false}"#, E::InvalidAuth),
            (r#"{"type":"basic"}"#, E::InvalidAuth),
            (r#"{"type":"none","env":"KEY"}"#, E::InvalidAuth),
            (r#"{"type":"none","env":null}"#, E::InvalidAuth),
            (r#"{"type":"bearer"}"#, E::MissingField),
            (r#"{"type":"bearer","env":null}"#, E::InvalidEnvironmentName),
            (r#"{"type":"bearer","env":""}"#, E::InvalidEnvironmentName),
            (
                r#"{"type":"bearer","env":"1KEY"}"#,
                E::InvalidEnvironmentName,
            ),
            (
                r#"{"type":"bearer","env":"${KEY}"}"#,
                E::InvalidEnvironmentName,
            ),
            (
                r#"{"type":"bearer","env":"KEY=secret"}"#,
                E::InvalidEnvironmentName,
            ),
            (
                r#"{"type":"bearer","env":"KEY\n"}"#,
                E::InvalidEnvironmentName,
            ),
            (
                r#"{"type":"bearer","env":"KEY","token":"literal"}"#,
                E::UnknownField,
            ),
            (r#"{"type":"bearer","command":"get-key"}"#, E::UnknownField),
        ];
        for (auth, error) in cases {
            let json = format!(
                r#"{{"local":{{"protocol":"openai-chat-completions","base_url":"https://example.com","auth":{auth}}}}}"#
            );
            assert_eq!(parse(&json), Err(error), "{auth}");
        }
        let auth: Value = serde_json::from_str(r#"{"type":"bearer","env":"_key_2"}"#).unwrap();
        assert_eq!(
            parse_auth(&auth),
            Ok(ProviderAuth::Bearer {
                env: "_key_2".to_owned()
            })
        );
    }

    #[test]
    fn configured_provider_scalar_bounds_and_minimum_input_budget() {
        assert_eq!(validate_id(&"a".repeat(MAX_ID_BYTES)), Ok(()));
        assert_eq!(validate_id("Local_2-test"), Ok(()));
        assert_eq!(
            validate_id(&"a".repeat(MAX_ID_BYTES + 1)),
            Err(ConfiguredProviderError::LimitExceeded)
        );
        assert_eq!(validate_model_id(&"m".repeat(MAX_MODEL_BYTES)), Ok(()));
        assert_eq!(validate_model_id("vendor/model:tag"), Ok(()));
        assert_eq!(validate_model_id("model with internal spaces"), Ok(()));
        assert_eq!(
            validate_model_id(" leading"),
            Err(ConfiguredProviderError::InvalidModelId)
        );
        assert_eq!(
            validate_model_id("trailing "),
            Err(ConfiguredProviderError::InvalidModelId)
        );
        assert_eq!(
            validate_model_id(&"m".repeat(MAX_MODEL_BYTES + 1)),
            Err(ConfiguredProviderError::LimitExceeded)
        );
        let prefix = "https://example.com/";
        let longest = format!("{prefix}{}", "a".repeat(MAX_URL_BYTES - prefix.len()));
        assert!(validate_url(&longest).is_ok());
        assert_eq!(
            validate_url(&format!("{longest}a")),
            Err(ConfiguredProviderError::LimitExceeded)
        );
        for length in [MAX_ENV_BYTES, MAX_ENV_BYTES + 1] {
            let json = format!(
                r#"{{"local":{{"protocol":"openai-chat-completions","base_url":"https://example.com","auth":{{"type":"bearer","env":"{}"}}}}}}"#,
                "E".repeat(length)
            );
            if length == MAX_ENV_BYTES {
                assert!(parse(&json).is_ok());
            } else {
                assert_eq!(parse(&json), Err(ConfiguredProviderError::LimitExceeded));
            }
        }
        let json = with_required(
            r#""model_metadata":{"small":{"context_window":2,"max_output_tokens":1},"large":{"context_window":4294967295},"output-only":{"max_output_tokens":1}}"#,
        );
        let registry = parse(&json).unwrap();
        let local = registry.get("local").unwrap();
        assert_eq!(local.capabilities("large").context_window, Some(u32::MAX));
        assert!(local.capabilities("output-only").context_window.is_none());
    }

    #[test]
    fn configured_provider_registry_model_count_and_raw_input_bounds() {
        let definition = format!("{{{TEST_REQUIRED_FIELDS}}}");
        let providers = |count: usize| {
            let entries: Vec<String> = (0..count)
                .map(|index| format!(r#""local{index}":{definition}"#))
                .collect();
            format!("{{{}}}", entries.join(","))
        };
        assert!(parse(&providers(MAX_PROVIDERS)).is_ok());
        assert_eq!(
            parse(&providers(MAX_PROVIDERS + 1)),
            Err(ConfiguredProviderError::LimitExceeded)
        );
        let metadata = |count: usize| {
            let entries: Vec<String> = (0..count)
                .map(|index| format!(r#""model/{index}":{{}}"#))
                .collect();
            with_required(&format!(r#""model_metadata":{{{}}}"#, entries.join(",")))
        };
        assert!(parse(&metadata(MAX_MODELS)).is_ok());
        assert_eq!(
            parse(&metadata(MAX_MODELS + 1)),
            Err(ConfiguredProviderError::LimitExceeded)
        );
        let mut raw = vec![b' '; MAX_JSON_BYTES + 1];
        raw[0] = b'{';
        raw[1] = b'}';
        assert!(ProviderRegistry::parse_json(&raw[..MAX_JSON_BYTES]).is_ok());
        assert_eq!(
            ProviderRegistry::parse_json(&raw),
            Err(ConfiguredProviderError::LimitExceeded)
        );
        assert_eq!(parse("{}"), Ok(ProviderRegistry::default()));
    }

    #[test]
    fn portkey_connections_accept_headers_tls_proxy_and_models() {
        let json = r#"{"portkey":{"protocol":"openai-chat-completions","base_url":"https://portkey.internal.example.com/v1","auth":{"type":"none"},"headers":{"x-portkey-api-key":"${PORTKEY_API_KEY}","x-portkey-config":"pc-abc123"},"tls":{"ca_file":"/etc/ssl/corp.pem"},"proxy":"http://proxy.corp:3128","models":["@openai/gpt-4o","@anthropic/claude"]}}"#;
        let registry = parse(json).unwrap();
        let portkey = registry.get("portkey").unwrap();
        let names: Vec<&str> = portkey.headers.iter().map(HeaderTemplate::name).collect();
        assert_eq!(names, ["x-portkey-api-key", "x-portkey-config"]);
        assert_eq!(portkey.ca_file.as_deref(), Some("/etc/ssl/corp.pem"));
        assert_eq!(portkey.proxy.as_deref(), Some("http://proxy.corp:3128"));
        assert_eq!(portkey.models, ["@openai/gpt-4o", "@anthropic/claude"]);
    }

    #[test]
    fn deliberate_additions_validate_their_shapes() {
        use ConfiguredProviderError as E;
        let cases = [
            (r#""headers":[]"#, E::InvalidObject),
            (r#""headers":{"x-key":1}"#, E::InvalidHeaderValue),
            (r#""headers":{"bad header":"v"}"#, E::InvalidHeaderName),
            (r#""headers":{"x-key":"a\nb"}"#, E::InvalidHeaderValue),
            (
                r#""headers":{"x-key":"${UNTERMINATED"}"#,
                E::InvalidHeaderValue,
            ),
            (r#""headers":{"x-key":"${1BAD}"}"#, E::InvalidHeaderValue),
            (
                r#""headers":{"Content-Type":"text/plain"}"#,
                E::ReservedHeader,
            ),
            (r#""headers":{"x-key":"a","X-Key":"b"}"#, E::DuplicateField),
            (r#""tls":{"ca_file":"relative.pem"}"#, E::InvalidTls),
            (r#""tls":{"ca_file":1}"#, E::InvalidTls),
            (r#""tls":{"verify":false}"#, E::UnknownField),
            (r#""tls":null"#, E::InvalidObject),
            (r#""proxy":"socks5://proxy:1080""#, E::InvalidProxy),
            (r#""proxy":7"#, E::InvalidProxy),
            (r#""models":"one""#, E::InvalidModelId),
            (r#""models":[" padded"]"#, E::InvalidModelId),
            (r#""models":["a","a"]"#, E::DuplicateField),
        ];
        for (extra, error) in cases {
            assert_eq!(parse(&with_required(extra)), Err(error), "{extra}");
        }
        assert!(parse(&with_required(r#""tls":{"ca_file":"~/certs/corp.pem"}"#)).is_ok());
        assert!(parse(&with_required(r#""tls":{}"#)).is_ok());
        let completion_tokens = parse(&with_required(
            r#""max_tokens_parameter":"max_completion_tokens""#,
        ))
        .unwrap();
        assert_eq!(
            completion_tokens.get("local").unwrap().max_tokens_parameter,
            MaxTokensParameter::MaxCompletionTokens
        );
        assert_eq!(
            MaxTokensParameter::MaxCompletionTokens.field(),
            "max_completion_tokens"
        );
    }
}
