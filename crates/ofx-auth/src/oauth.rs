use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ofx_contract::parse_strict_json_value;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::secret::Secret;

const SECRET_ENTROPY_BYTES: usize = 32;
const MILLISECONDS_PER_SECOND: i64 = 1000;
const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum OAuthError {
    #[error("InvalidOAuthResponse")]
    InvalidOAuthResponse,
    #[error("RandomSourceUnavailable")]
    RandomSourceUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum QueryError {
    #[error("MissingQueryParameter")]
    MissingQueryParameter,
    #[error("InvalidPercentEncoding")]
    InvalidPercentEncoding,
    #[error("EmptyQueryValue")]
    EmptyQueryValue,
}

#[derive(Debug)]
pub(crate) struct BrowserTokenSet {
    pub(crate) access_token: Secret,
    pub(crate) refresh_token: Secret,
    pub(crate) expires_in: Option<i64>,
}

pub(crate) fn expiry_timestamp_ms(now_ms: i64, expires_in_seconds: i64) -> Result<i64, OAuthError> {
    if expires_in_seconds <= 0 {
        return Err(OAuthError::InvalidOAuthResponse);
    }
    expires_in_seconds
        .checked_mul(MILLISECONDS_PER_SECOND)
        .and_then(|duration| now_ms.checked_add(duration))
        .ok_or(OAuthError::InvalidOAuthResponse)
}

pub(crate) fn random_url_safe_secret() -> Result<Secret, OAuthError> {
    let mut entropy = Zeroizing::new([0_u8; SECRET_ENTROPY_BYTES]);
    getrandom::fill(entropy.as_mut_slice()).map_err(|_| OAuthError::RandomSourceUnavailable)?;
    Ok(Secret::new(URL_SAFE_NO_PAD.encode(entropy.as_slice())))
}

pub(crate) fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[derive(Debug, Default)]
pub struct FormBody {
    text: Zeroizing<String>,
}

impl FormBody {
    pub fn append(&mut self, key: &str, value: &str) {
        if !self.text.is_empty() {
            self.text.push('&');
        }
        percent_encode(&mut self.text, key);
        self.text.push('=');
        percent_encode(&mut self.text, value);
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

pub fn percent_encode(out: &mut String, value: &str) {
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(UPPER_HEX[usize::from(byte >> 4)]));
            out.push(char::from(UPPER_HEX[usize::from(byte & 0x0f)]));
        }
    }
}

pub(crate) fn query_value(query: &str, key: &str) -> Result<Zeroizing<String>, QueryError> {
    let value = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
        .ok_or(QueryError::MissingQueryParameter)?;
    percent_decode(value)
}

pub(crate) fn query_value_non_empty(
    query: &str,
    key: &str,
) -> Result<Zeroizing<String>, QueryError> {
    let value = query_value(query, key)?;
    if value.is_empty() {
        return Err(QueryError::EmptyQueryValue);
    }
    Ok(value)
}

pub(crate) fn percent_decode(value: &str) -> Result<Zeroizing<String>, QueryError> {
    let bytes = value.as_bytes();
    let mut out = Zeroizing::new(Vec::with_capacity(bytes.len()));
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(QueryError::InvalidPercentEncoding);
                }
                let high = hex_digit(bytes[index + 1])?;
                let low = hex_digit(bytes[index + 2])?;
                out.push(high * 16 + low);
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    let text = std::str::from_utf8(&out).map_err(|_| QueryError::InvalidPercentEncoding)?;
    Ok(Zeroizing::new(text.to_owned()))
}

fn hex_digit(byte: u8) -> Result<u8, QueryError> {
    char::from(byte)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
        .ok_or(QueryError::InvalidPercentEncoding)
}

pub(crate) fn parse_browser_token_set(bytes: &[u8]) -> Result<BrowserTokenSet, OAuthError> {
    let object = parse_object(bytes)?;
    let access_token = required_string(&object, "access_token")?;
    let refresh_token = required_string(&object, "refresh_token")?;
    let expires_in = optional_positive_integer(&object, "expires_in")?;
    Ok(BrowserTokenSet {
        access_token,
        refresh_token,
        expires_in,
    })
}

pub(crate) fn parse_object(bytes: &[u8]) -> Result<Map<String, Value>, OAuthError> {
    match parse_strict_json_value(bytes) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err(OAuthError::InvalidOAuthResponse),
    }
}

pub(crate) fn required_string(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Secret, OAuthError> {
    match object.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(Secret::new(value.clone())),
        _ => Err(OAuthError::InvalidOAuthResponse),
    }
}

pub(crate) fn optional_positive_integer(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<i64>, OAuthError> {
    object
        .get(key)
        .map(|value| {
            value
                .as_i64()
                .filter(|integer| *integer > 0)
                .ok_or(OAuthError::InvalidOAuthResponse)
        })
        .transpose()
}

pub(crate) fn is_loopback_http_url(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        && value
            .split_once("://")
            .and_then(|(_, tail)| tail.split(['/', '?', '#']).next())
            .is_some_and(|authority| {
                !authority.contains('@')
                    && authority.rsplit_once(':').is_some_and(|(_, port)| {
                        !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
                    })
            })
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

pub fn loopback_override(variable: &str) -> Option<String> {
    std::env::var(variable)
        .ok()
        .filter(|value| is_loopback_http_url(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_form_codec_preserves_encoding_and_first_value_query_semantics() {
        let mut form = FormBody::default();
        form.append("first key", "one+two");
        form.append("empty", "");
        assert_eq!(form.as_str(), "first%20key=one%2Btwo&empty=");

        let first =
            query_value_non_empty("ignored&value=first+result&value=second", "value").unwrap();
        assert_eq!(first.as_str(), "first result");
        assert_eq!(query_value("value=", "value").unwrap().as_str(), "");
        assert_eq!(
            query_value_non_empty("value=", "value"),
            Err(QueryError::EmptyQueryValue)
        );
        assert_eq!(
            query_value("other=value", "value"),
            Err(QueryError::MissingQueryParameter)
        );
        assert_eq!(
            query_value("value=%zz", "value"),
            Err(QueryError::InvalidPercentEncoding)
        );
        assert_eq!(
            query_value("value=%4", "value"),
            Err(QueryError::InvalidPercentEncoding)
        );
        assert_eq!(query_value("value=%41", "value").unwrap().as_str(), "A");
    }

    #[test]
    fn browser_token_parsing_and_transfer_clean_up_allocation_failures() {
        let token = parse_browser_token_set(
            br#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
        )
        .unwrap();
        assert_eq!(token.access_token.expose(), "access");
        assert_eq!(token.refresh_token.expose(), "refresh");
        assert_eq!(token.expires_in, Some(3600));
        for invalid in [
            &br#"{"access_token":"access","refresh_token":"","expires_in":3600}"#[..],
            br#"{"access_token":"access","expires_in":3600}"#,
            br#"{"access_token":"a","access_token":"b","refresh_token":"r","expires_in":1}"#,
            b"[]",
        ] {
            assert!(parse_browser_token_set(invalid).is_err());
        }
    }

    #[test]
    fn browser_tokens_may_omit_expires_in_but_never_send_a_malformed_one() {
        let token =
            parse_browser_token_set(br#"{"access_token":"access","refresh_token":"refresh"}"#)
                .unwrap();
        assert_eq!(token.expires_in, None);
        for expires_in in [
            "0",
            "-60",
            "36.5",
            "3600.0",
            "\"3600\"",
            "null",
            "true",
            "18446744073709551615",
        ] {
            let body = format!(
                r#"{{"access_token":"access","refresh_token":"refresh","expires_in":{expires_in}}}"#
            );
            assert_eq!(
                parse_browser_token_set(body.as_bytes()).err(),
                Some(OAuthError::InvalidOAuthResponse),
                "{expires_in}"
            );
        }
    }

    #[test]
    fn oauth_expiry_timestamps_reject_invalid_durations() {
        assert_eq!(expiry_timestamp_ms(1_000, 10), Ok(11_000));
        for (now, seconds) in [(1_000, 0), (1_000, -1), (1_000, i64::MAX), (i64::MAX, 1)] {
            assert_eq!(
                expiry_timestamp_ms(now, seconds),
                Err(OAuthError::InvalidOAuthResponse)
            );
        }
    }

    #[test]
    fn pkce_uses_s256_over_a_fresh_url_safe_verifier() {
        let verifier = random_url_safe_secret().unwrap();
        assert_eq!(verifier.expose().len(), 43);
        assert!(
            verifier
                .expose()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
        assert_ne!(verifier, random_url_safe_secret().unwrap());
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
