use crate::mcp_contract::HttpHeader;
use crate::uri::Uri;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    #[error("InvalidEndpoint")]
    InvalidEndpoint,
    #[error("InsecureEndpoint")]
    InsecureEndpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("InvalidHeaderName")]
    InvalidHeaderName,
    #[error("InvalidHeaderValue")]
    InvalidHeaderValue,
    #[error("DuplicateHeader")]
    DuplicateHeader,
    #[error("ReservedHeader")]
    ReservedHeader,
}

const RESERVED_HEADERS: [&str; 12] = [
    "accept",
    "accept-encoding",
    "connection",
    "content-length",
    "content-type",
    "host",
    "last-event-id",
    "mcp-method",
    "mcp-name",
    "mcp-protocol-version",
    "mcp-session-id",
    "transfer-encoding",
];

pub fn validate_endpoint(url: &str) -> Result<(), EndpointError> {
    let uri = Uri::parse(url).ok_or(EndpointError::InvalidEndpoint)?;
    if uri.has_userinfo || uri.has_fragment || uri.host.is_none_or(str::is_empty) {
        return Err(EndpointError::InvalidEndpoint);
    }
    if uri.scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    if !uri.scheme.eq_ignore_ascii_case("http") || uri.port.is_none() {
        return Err(EndpointError::InsecureEndpoint);
    }
    if uri.is_loopback_host() {
        Ok(())
    } else {
        Err(EndpointError::InsecureEndpoint)
    }
}

pub fn validate_static_headers(headers: &[HttpHeader]) -> Result<(), HeaderError> {
    for (index, header) in headers.iter().enumerate() {
        if !is_http_token(&header.name) {
            return Err(HeaderError::InvalidHeaderName);
        }
        validate_header_value(&header.value)?;
        if is_reserved_header(&header.name) {
            return Err(HeaderError::ReservedHeader);
        }
        if headers
            .iter()
            .take(index)
            .any(|previous| previous.name.eq_ignore_ascii_case(&header.name))
        {
            return Err(HeaderError::DuplicateHeader);
        }
    }
    Ok(())
}

pub(crate) fn validate_header_value(value: &str) -> Result<(), HeaderError> {
    let valid = value
        .bytes()
        .all(|byte| (byte >= 0x20 || byte == b'\t') && byte != 0x7f);
    if valid {
        Ok(())
    } else {
        Err(HeaderError::InvalidHeaderValue)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaType {
    Json,
    EventStream,
}

pub(crate) fn parse_media_type(value: &str) -> Option<MediaType> {
    let media_type = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim_matches([' ', '\t']);
    if media_type.eq_ignore_ascii_case("application/json") {
        Some(MediaType::Json)
    } else if media_type.eq_ignore_ascii_case("text/event-stream") {
        Some(MediaType::EventStream)
    } else {
        None
    }
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn is_reserved_header(name: &str) -> bool {
    RESERVED_HEADERS
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
        || starts_with_ignore_case(name, "mcp-param-")
}

fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len()
        && value.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> HttpHeader {
        HttpHeader {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn modern_mcp_endpoint_policy_accepts_https_and_explicit_loopback_http() {
        for url in [
            "https://mcp.example.com/rpc?workspace=one",
            "http://localhost:4321/mcp",
            "http://127.0.0.1:4321/mcp",
            "http://[::1]:4321/mcp",
        ] {
            assert_eq!(validate_endpoint(url), Ok(()), "{url}");
        }
        assert_eq!(
            validate_endpoint("http://example.com/mcp"),
            Err(EndpointError::InsecureEndpoint)
        );
        assert_eq!(
            validate_endpoint("http://localhost/mcp"),
            Err(EndpointError::InsecureEndpoint)
        );
        assert_eq!(
            validate_endpoint("https://user@example.com/mcp"),
            Err(EndpointError::InvalidEndpoint)
        );
        assert_eq!(
            validate_endpoint("https://example.com/mcp#fragment"),
            Err(EndpointError::InvalidEndpoint)
        );
    }

    #[test]
    fn modern_mcp_response_media_types_tolerate_case_and_parameters_only() {
        assert_eq!(
            parse_media_type("Application/JSON; charset=utf-8"),
            Some(MediaType::Json)
        );
        assert_eq!(
            parse_media_type(" text/event-stream ;q=1"),
            Some(MediaType::EventStream)
        );
        assert_eq!(parse_media_type("text/plain"), None);
        assert_eq!(parse_media_type("application/json-seq"), None);
    }

    #[test]
    fn modern_mcp_static_headers_reject_malformed_duplicate_and_protocol_owned_names() {
        assert_eq!(
            validate_static_headers(&[
                header("Authorization", "Bearer redacted"),
                header("X-Workspace", "one"),
            ]),
            Ok(())
        );
        assert_eq!(
            validate_static_headers(&[header("X-Workspace", "one"), header("x-workspace", "two")]),
            Err(HeaderError::DuplicateHeader)
        );
        assert_eq!(
            validate_static_headers(&[header("Bad Header", "value")]),
            Err(HeaderError::InvalidHeaderName)
        );
        assert_eq!(
            validate_static_headers(&[header("X-Test", "one\r\ntwo")]),
            Err(HeaderError::InvalidHeaderValue)
        );
        assert_eq!(
            validate_static_headers(&[header("Mcp-Param-Region", "override")]),
            Err(HeaderError::ReservedHeader)
        );
        assert_eq!(
            validate_static_headers(&[header("MCP-Session-Id", "override")]),
            Err(HeaderError::ReservedHeader)
        );
    }
}
