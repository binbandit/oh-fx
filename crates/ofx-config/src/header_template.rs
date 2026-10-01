use crate::configured_provider::{
    ConfiguredProviderError, MAX_ENV_BYTES, ProviderAuth, is_environment_name,
};
use crate::connection::MAX_CREDENTIAL_BYTES;

const MAX_HEADER_NAME_BYTES: usize = 256;
const MAX_TEMPLATE_BYTES: usize = 8 * 1024;
const TRANSPORT_HEADERS: [&str; 7] = [
    "accept",
    "connection",
    "content-length",
    "content-type",
    "host",
    "transfer-encoding",
    "user-agent",
];
const TOKEN_PUNCTUATION: &[u8] = b"!#$%&'*+-.^_`|~";
const SENSITIVE_NAME_PARTS: [&str; 11] = [
    "key",
    "apikey",
    "token",
    "secret",
    "auth",
    "authorization",
    "password",
    "passwd",
    "credential",
    "credentials",
    "cookie",
];
const MIN_UNNAMED_SECRET_BYTES: usize = 8;
const WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Variable {
        name: String,
        default: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeaderTemplate {
    name: String,
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InterpolationError {
    MissingVariable(String),
    InvalidValue,
}

impl HeaderTemplate {
    pub(crate) fn parse(
        name: &str,
        template: &str,
        auth: &ProviderAuth,
    ) -> Result<Self, ConfiguredProviderError> {
        if !is_token(name) {
            return Err(ConfiguredProviderError::InvalidHeaderName);
        }
        let reserved = TRANSPORT_HEADERS
            .iter()
            .any(|header| header.eq_ignore_ascii_case(name))
            || (name.eq_ignore_ascii_case("authorization") && *auth != ProviderAuth::None);
        if reserved {
            return Err(ConfiguredProviderError::ReservedHeader);
        }
        if template.len() > MAX_TEMPLATE_BYTES {
            return Err(ConfiguredProviderError::LimitExceeded);
        }
        let segments =
            parse_segments(template).ok_or(ConfiguredProviderError::InvalidHeaderValue)?;
        Ok(Self {
            name: name.to_owned(),
            segments,
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn interpolate(
        &self,
        lookup: &dyn Fn(&str) -> Option<String>,
        secrets: &mut Vec<String>,
    ) -> Result<String, InterpolationError> {
        let sensitive = is_sensitive_header(&self.name);
        let mut value = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => value.push_str(text),
                Segment::Variable { name, default } => {
                    let found =
                        lookup(name).filter(|found| !found.trim_matches(WHITESPACE).is_empty());
                    match (found, default) {
                        (Some(found), _) => {
                            value.push_str(&found);
                            if sensitive || found.len() >= MIN_UNNAMED_SECRET_BYTES {
                                secrets.push(found);
                            }
                        }
                        (None, Some(default)) => value.push_str(default),
                        (None, None) => {
                            return Err(InterpolationError::MissingVariable(name.clone()));
                        }
                    }
                }
            }
        }
        if value.len() > MAX_CREDENTIAL_BYTES || !value.bytes().all(is_value_byte) {
            return Err(InterpolationError::InvalidValue);
        }
        if sensitive && !value.is_empty() {
            secrets.push(value.clone());
        }
        Ok(value)
    }
}

fn parse_segments(template: &str) -> Option<Vec<Segment>> {
    let mut segments = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        push_literal(&mut segments, &rest[..start])?;
        let body_and_rest = &rest[start + 2..];
        let end = body_and_rest.find('}')?;
        segments.push(parse_variable(&body_and_rest[..end])?);
        rest = &body_and_rest[end + 1..];
    }
    push_literal(&mut segments, rest)?;
    Some(segments)
}

fn push_literal(segments: &mut Vec<Segment>, text: &str) -> Option<()> {
    if !text.bytes().all(is_value_byte) {
        return None;
    }
    if !text.is_empty() {
        segments.push(Segment::Literal(text.to_owned()));
    }
    Some(())
}

fn parse_variable(body: &str) -> Option<Segment> {
    let (name, default) = match body.split_once(":-") {
        Some((name, default)) => (name, Some(default)),
        None => (body, None),
    };
    if name.len() > MAX_ENV_BYTES || !is_environment_name(name) {
        return None;
    }
    if default.is_some_and(|default| default.contains("${") || !default.bytes().all(is_value_byte))
    {
        return None;
    }
    Some(Segment::Variable {
        name: name.to_owned(),
        default: default.map(str::to_owned),
    })
}

fn is_sensitive_header(name: &str) -> bool {
    name.split(['-', '_', '.']).any(|part| {
        SENSITIVE_NAME_PARTS
            .iter()
            .any(|sensitive| part.eq_ignore_ascii_case(sensitive))
    })
}

fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_HEADER_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || TOKEN_PUNCTUATION.contains(&byte))
}

fn is_value_byte(byte: u8) -> bool {
    byte == b'\t' || (0x20..0x7f).contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(text: &str) -> HeaderTemplate {
        HeaderTemplate::parse("x-team", text, &ProviderAuth::None).unwrap()
    }

    fn named(name: &str, text: &str) -> HeaderTemplate {
        HeaderTemplate::parse(name, text, &ProviderAuth::None).unwrap()
    }

    fn environment(name: &str) -> Option<String> {
        match name {
            "PORTKEY_API_KEY" => Some("pk-secret".to_owned()),
            "PROVIDER" => Some("openai".to_owned()),
            "CONFIG_ID" => Some("pc-config-123".to_owned()),
            "EMPTY" => Some(String::new()),
            "BLANK" => Some(" \t\n".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn interpolates_variables_defaults_and_literals() {
        let mut secrets = Vec::new();
        assert_eq!(
            template("${PORTKEY_API_KEY}").interpolate(&environment, &mut secrets),
            Ok("pk-secret".to_owned())
        );
        assert_eq!(
            template("Bearer ${PORTKEY_API_KEY} (${MISSING:-none})")
                .interpolate(&environment, &mut secrets),
            Ok("Bearer pk-secret (none)".to_owned())
        );
        assert_eq!(
            template("${EMPTY:-fallback}").interpolate(&environment, &mut secrets),
            Ok("fallback".to_owned())
        );
        assert_eq!(
            template("${MISSING:-}").interpolate(&environment, &mut secrets),
            Ok(String::new())
        );
        assert_eq!(
            template("price $5").interpolate(&environment, &mut secrets),
            Ok("price $5".to_owned())
        );
        assert_eq!(
            template("${BLANK:-fallback}").interpolate(&environment, &mut secrets),
            Ok("fallback".to_owned())
        );
        assert_eq!(secrets, ["pk-secret", "pk-secret"]);
    }

    #[test]
    fn only_plausible_secrets_are_collected_for_masking() {
        let mut secrets = Vec::new();
        assert_eq!(
            named("x-portkey-provider", "${PROVIDER}").interpolate(&environment, &mut secrets),
            Ok("openai".to_owned())
        );
        assert_eq!(
            named("x-portkey-config", "${CONFIG_ID}").interpolate(&environment, &mut secrets),
            Ok("pc-config-123".to_owned())
        );
        assert_eq!(
            named("x-team", "platform").interpolate(&environment, &mut secrets),
            Ok("platform".to_owned())
        );
        assert_eq!(secrets, ["pc-config-123"]);
        secrets.clear();
        for name in [
            "Authorization",
            "Proxy-Authorization",
            "Cookie",
            "x-portkey-api-key",
            "X-Client-Secret",
            "apikey",
        ] {
            named(name, "${PROVIDER}")
                .interpolate(&environment, &mut secrets)
                .unwrap();
        }
        assert_eq!(secrets.len(), 12);
        assert!(secrets.iter().all(|secret| secret == "openai"));
        secrets.clear();
        named("x-api-key", "literal-key")
            .interpolate(&environment, &mut secrets)
            .unwrap();
        assert_eq!(secrets, ["literal-key"]);
    }

    #[test]
    fn missing_variables_name_the_variable() {
        let mut secrets = Vec::new();
        assert_eq!(
            template("${MISSING}").interpolate(&environment, &mut secrets),
            Err(InterpolationError::MissingVariable("MISSING".to_owned()))
        );
        assert_eq!(
            template("${EMPTY}").interpolate(&environment, &mut secrets),
            Err(InterpolationError::MissingVariable("EMPTY".to_owned()))
        );
        assert_eq!(
            template("${BLANK}").interpolate(&environment, &mut secrets),
            Err(InterpolationError::MissingVariable("BLANK".to_owned()))
        );
    }

    #[test]
    fn rejects_values_that_cannot_travel_in_a_header() {
        let lookup = |_: &str| Some("line\nbreak".to_owned());
        assert_eq!(
            template("${ANY}").interpolate(&lookup, &mut Vec::new()),
            Err(InterpolationError::InvalidValue)
        );
        let oversized = |_: &str| Some("k".repeat(MAX_CREDENTIAL_BYTES + 1));
        assert_eq!(
            template("${ANY}").interpolate(&oversized, &mut Vec::new()),
            Err(InterpolationError::InvalidValue)
        );
        for text in [
            "${",
            "${}",
            "${A",
            "${A B}",
            "${A:-x\ny}",
            "tab\u{7f}",
            "${A:-${B}}",
            "${A:-prefix${B}suffix}",
        ] {
            assert_eq!(
                HeaderTemplate::parse("x-key", text, &ProviderAuth::None),
                Err(ConfiguredProviderError::InvalidHeaderValue),
                "{text:?}"
            );
        }
    }

    #[test]
    fn reserves_transport_headers_and_bearer_authorization() {
        let bearer = ProviderAuth::Bearer {
            env: "KEY".to_owned(),
        };
        assert_eq!(
            HeaderTemplate::parse("Authorization", "Bearer x", &bearer),
            Err(ConfiguredProviderError::ReservedHeader)
        );
        assert!(
            HeaderTemplate::parse("Authorization", "Bearer ${KEY}", &ProviderAuth::None).is_ok()
        );
        for name in ["Accept", "user-agent", "Host", "CONTENT-LENGTH"] {
            assert_eq!(
                HeaderTemplate::parse(name, "x", &ProviderAuth::None),
                Err(ConfiguredProviderError::ReservedHeader)
            );
        }
        for name in ["", "bad name", "x:y", "é"] {
            assert_eq!(
                HeaderTemplate::parse(name, "x", &ProviderAuth::None),
                Err(ConfiguredProviderError::InvalidHeaderName)
            );
        }
    }
}
