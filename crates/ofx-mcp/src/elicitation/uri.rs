use super::{ElicitationError, MAX_STRING_BYTES, Outcome};
use crate::fmt_int::parse_unsigned;

const MAX_HOST_BYTES: usize = 255;

pub(super) struct Uri<'a> {
    scheme: &'a str,
    has_user: bool,
    host: Option<&'a str>,
}

pub(super) fn parse(text: &str) -> Option<Uri<'_>> {
    let (scheme, rest) = text.split_once(':')?;
    if !valid_scheme(scheme) {
        return None;
    }
    let mut uri = Uri {
        scheme,
        has_user: false,
        host: None,
    };
    let Some(after) = rest.strip_prefix("//") else {
        return Some(uri);
    };
    let authority = &after[..after.find(['/', '?', '#']).unwrap_or(after.len())];
    if authority.is_empty() {
        return after.starts_with('/').then_some(uri);
    }
    let mut start = 0;
    if let Some(at) = authority.find('@') {
        start = at + 1;
        uri.has_user = true;
    }
    if start >= authority.len() {
        return Some(uri);
    }
    if authority[start..].starts_with(']') {
        return None;
    }
    let mut end = authority.len();
    let last_colon = authority.rfind(':');
    if authority[start..].starts_with('[') {
        end = authority.rfind(']')? + 1;
        if let Some(colon) = last_colon.filter(|colon| *colon >= end) {
            port(&authority[colon + 1..])?;
        }
    } else if let Some(colon) = last_colon.filter(|colon| *colon >= start) {
        end = colon;
        port(&authority[colon + 1..])?;
    }
    if start >= end {
        return None;
    }
    uri.host = Some(&authority[start..end]);
    Some(uri)
}

fn port(text: &str) -> Option<u16> {
    parse_unsigned(text.as_bytes(), 10).and_then(|port| u16::try_from(port).ok())
}

fn valid_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

pub(super) fn classify_url(url: &str) -> Outcome<Vec<u8>> {
    if url.is_empty()
        || url.len() > MAX_STRING_BYTES
        || url != url.trim_matches([' ', '\t', '\r', '\n'])
    {
        return Err(ElicitationError::InvalidUrl);
    }
    let uri = parse(url).ok_or(ElicitationError::InvalidUrl)?;
    let host = uri
        .host
        .filter(|_| !uri.has_user)
        .ok_or(ElicitationError::InvalidUrl)?;
    let host = raw_host(host).ok_or(ElicitationError::InvalidUrl)?;
    if host.is_empty() {
        return Err(ElicitationError::InvalidUrl);
    }
    let loopback = uri.scheme.eq_ignore_ascii_case("http")
        && (host.eq_ignore_ascii_case(b"localhost") || host == b"127.0.0.1" || host == b"[::1]");
    if !uri.scheme.eq_ignore_ascii_case("https") && !loopback {
        return Err(ElicitationError::InsecureUrl);
    }
    Ok(host)
}

fn raw_host(encoded: &str) -> Option<Vec<u8>> {
    let bytes = encoded.as_bytes();
    if !bytes.contains(&b'%') {
        return Some(bytes.to_vec());
    }
    let mut raw = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let decoded = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::try_from(parse_unsigned(hex, 16)?).ok());
        if let Some(byte) = decoded {
            raw.push(byte);
            index += 3;
        } else {
            raw.push(bytes[index]);
            index += 1;
        }
    }
    (raw.len() <= MAX_HOST_BYTES).then_some(raw)
}
