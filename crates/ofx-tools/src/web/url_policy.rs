use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const MAX_URL_BYTES: usize = 2000;
const HTTPS_PORT: u16 = 443;
const HTTP_PORT: u16 = 80;
const MAPPED_IPV4_PREFIX: &str = "::ffff:";
const BLOCKED_IPV4_RANGES: [([u8; 4], u32); 15] = [
    ([0, 0, 0, 0], 8),
    ([10, 0, 0, 0], 8),
    ([100, 64, 0, 0], 10),
    ([127, 0, 0, 0], 8),
    ([169, 254, 0, 0], 16),
    ([172, 16, 0, 0], 12),
    ([192, 0, 0, 0], 24),
    ([192, 0, 2, 0], 24),
    ([192, 88, 99, 0], 24),
    ([192, 168, 0, 0], 16),
    ([198, 18, 0, 0], 15),
    ([198, 51, 100, 0], 24),
    ([203, 0, 113, 0], 24),
    ([224, 0, 0, 0], 4),
    ([240, 0, 0, 0], 4),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyError {
    UrlTooLong,
    EmptyUrl,
    UnsupportedScheme,
    MissingHost,
    CredentialedUrl,
    SingleLabelHost,
    MalformedHost,
    MalformedLocation,
    MalformedPercentEncoding,
    PercentEncodedHost,
    UnicodeHost,
    ControlByte,
    RequestTargetWhitespace,
    InvalidPort,
    InvalidIpv4,
    ScopeIdRejected,
    NonPublicAddress,
    PortChanged,
}

impl PolicyError {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::UrlTooLong => "UrlTooLong",
            Self::EmptyUrl => "EmptyUrl",
            Self::UnsupportedScheme => "UnsupportedScheme",
            Self::MissingHost => "MissingHost",
            Self::CredentialedUrl => "CredentialedUrl",
            Self::SingleLabelHost => "SingleLabelHost",
            Self::MalformedHost => "MalformedHost",
            Self::MalformedLocation => "MalformedLocation",
            Self::MalformedPercentEncoding => "MalformedPercentEncoding",
            Self::PercentEncodedHost => "PercentEncodedHost",
            Self::UnicodeHost => "UnicodeHost",
            Self::ControlByte => "ControlByte",
            Self::RequestTargetWhitespace => "RequestTargetWhitespace",
            Self::InvalidPort => "InvalidPort",
            Self::InvalidIpv4 => "InvalidIpv4",
            Self::ScopeIdRejected => "ScopeIdRejected",
            Self::NonPublicAddress => "NonPublicAddress",
            Self::PortChanged => "PortChanged",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedUrl {
    pub(crate) canonical_host: String,
    pub(crate) port: u16,
    explicit_port: Option<u16>,
    path_query: String,
    pub(crate) retrieval_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Redirect {
    Follow(ValidatedUrl),
    CrossHost(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ipv4Parse {
    Address([u8; 4]),
    NotIpv4,
}

pub(crate) fn normalize(raw_url: &str) -> Result<ValidatedUrl, PolicyError> {
    if raw_url.is_empty() {
        return Err(PolicyError::EmptyUrl);
    }
    if raw_url.len() > MAX_URL_BYTES {
        return Err(PolicyError::UrlTooLong);
    }
    reject_control_bytes(raw_url)?;
    validate_percent_encoding(raw_url.as_bytes())?;
    let scheme_end = raw_url.find("://").ok_or(PolicyError::UnsupportedScheme)?;
    let raw_scheme = &raw_url[..scheme_end];
    let plain_http = if raw_scheme.eq_ignore_ascii_case("http") {
        true
    } else if raw_scheme.eq_ignore_ascii_case("https") {
        false
    } else {
        return Err(PolicyError::UnsupportedScheme);
    };
    let authority_start = scheme_end + "://".len();
    let authority_end = authority_end(raw_url, authority_start);
    if authority_end == authority_start {
        return Err(PolicyError::MissingHost);
    }
    let authority = &raw_url[authority_start..authority_end];
    if authority.contains('@') {
        return Err(PolicyError::CredentialedUrl);
    }
    let (raw_host, raw_port, ipv6_literal) = parse_authority(authority)?;
    let canonical_host = canonicalize_host(raw_host, ipv6_literal)?;
    let port = match raw_port {
        Some(HTTP_PORT) if plain_http => HTTPS_PORT,
        Some(port) => port,
        None => HTTPS_PORT,
    };
    let explicit_port = raw_port.filter(|_| port != HTTPS_PORT).map(|_| port);
    let path_query = normalize_path_query(strip_fragment(&raw_url[authority_end..]))?;
    let retrieval_url = format_retrieval_url(&canonical_host, explicit_port, &path_query);
    Ok(ValidatedUrl {
        canonical_host,
        port,
        explicit_port,
        path_query,
        retrieval_url,
    })
}

pub(crate) fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address.octets()),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

pub(crate) fn redirect_target(
    current: &ValidatedUrl,
    location: &str,
) -> Result<Redirect, PolicyError> {
    if location.is_empty() {
        return Err(PolicyError::MalformedLocation);
    }
    reject_control_bytes(location)?;
    let absolute = absolute_redirect_url(current, location)?;
    let target = normalize(&absolute).map_err(|error| match error {
        PolicyError::UnsupportedScheme
        | PolicyError::CredentialedUrl
        | PolicyError::ScopeIdRejected
        | PolicyError::ControlByte
        | PolicyError::RequestTargetWhitespace
        | PolicyError::UrlTooLong
        | PolicyError::NonPublicAddress => error,
        _ => PolicyError::MalformedLocation,
    })?;
    if target.port != current.port {
        return Err(PolicyError::PortChanged);
    }
    if same_host_or_optional_www(&current.canonical_host, &target.canonical_host) {
        return Ok(Redirect::Follow(target));
    }
    Ok(Redirect::CrossHost(target.retrieval_url))
}

fn authority_end(url: &str, start: usize) -> usize {
    url.as_bytes()[start..]
        .iter()
        .position(|byte| matches!(byte, b'/' | b'?' | b'#'))
        .map_or(url.len(), |offset| start + offset)
}

fn parse_authority(authority: &str) -> Result<(&str, Option<u16>, bool), PolicyError> {
    if authority.is_empty() {
        return Err(PolicyError::MissingHost);
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed.find(']').ok_or(PolicyError::MalformedHost)?;
        if end == 0 {
            return Err(PolicyError::MissingHost);
        }
        let host = &bracketed[..end];
        if host.contains('%') {
            return Err(PolicyError::ScopeIdRejected);
        }
        let rest = &bracketed[end + 1..];
        if rest.is_empty() {
            return Ok((host, None, true));
        }
        let port = rest.strip_prefix(':').ok_or(PolicyError::MalformedHost)?;
        return Ok((host, Some(parse_port(port)?), true));
    }
    let colon = authority.rfind(':').unwrap_or(authority.len());
    if colon == 0 {
        return Err(PolicyError::MissingHost);
    }
    let port = if colon == authority.len() {
        None
    } else {
        Some(parse_port(&authority[colon + 1..])?)
    };
    Ok((&authority[..colon], port, false))
}

fn parse_port(raw: &str) -> Result<u16, PolicyError> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PolicyError::InvalidPort);
    }
    raw.parse().map_err(|_| PolicyError::InvalidPort)
}

fn canonicalize_host(raw_host: &str, ipv6_literal: bool) -> Result<String, PolicyError> {
    if raw_host.is_empty() {
        return Err(PolicyError::MissingHost);
    }
    if raw_host.contains('%') {
        return Err(PolicyError::PercentEncodedHost);
    }
    for byte in raw_host.bytes() {
        if byte < 0x20 || byte == 0x7f {
            return Err(PolicyError::ControlByte);
        }
        if byte >= 0x80 {
            return Err(PolicyError::UnicodeHost);
        }
        if matches!(byte, b' ' | b'\\' | b'/') {
            return Err(PolicyError::MalformedHost);
        }
    }
    if ipv6_literal {
        let address = parse_ipv6_literal(raw_host).ok_or(PolicyError::MalformedHost)?;
        if !is_public_address(IpAddr::V6(address)) {
            return Err(PolicyError::NonPublicAddress);
        }
        return Ok(raw_host.to_ascii_lowercase());
    }
    let host = strip_one_root_dot(raw_host);
    if let Ipv4Parse::Address(octets) = parse_strict_ipv4(host)? {
        if !is_public_address(IpAddr::V4(Ipv4Addr::from(octets))) {
            return Err(PolicyError::NonPublicAddress);
        }
        return Ok(host.to_ascii_lowercase());
    }
    if looks_like_ambiguous_ipv4(host) {
        return Err(PolicyError::InvalidIpv4);
    }
    if !host.contains('.') {
        return Err(PolicyError::SingleLabelHost);
    }
    validate_hostname_grammar(host)?;
    let lower = host.to_ascii_lowercase();
    if is_blocked_hostname(&lower) {
        return Err(PolicyError::NonPublicAddress);
    }
    Ok(lower)
}

fn parse_ipv6_literal(text: &str) -> Option<Ipv6Addr> {
    match text.get(..MAPPED_IPV4_PREFIX.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(MAPPED_IPV4_PREFIX) => text
            [MAPPED_IPV4_PREFIX.len()..]
            .parse::<Ipv4Addr>()
            .ok()
            .map(|address| address.to_ipv6_mapped()),
        _ if text.contains('.') => None,
        _ => text.parse().ok(),
    }
}

fn strip_one_root_dot(host: &str) -> &str {
    match host.strip_suffix('.') {
        Some(stripped) if !stripped.is_empty() => stripped,
        _ => host,
    }
}

fn validate_hostname_grammar(host: &str) -> Result<(), PolicyError> {
    let mut label_len = 0_usize;
    let mut previous = 0_u8;
    for byte in host.bytes() {
        if byte == b'.' {
            if label_len == 0 || label_len > 63 || !previous.is_ascii_alphanumeric() {
                return Err(PolicyError::MalformedHost);
            }
            label_len = 0;
            previous = byte;
            continue;
        }
        if !byte.is_ascii_alphanumeric() && byte != b'-' {
            return Err(PolicyError::MalformedHost);
        }
        if label_len == 0 && byte == b'-' {
            return Err(PolicyError::MalformedHost);
        }
        label_len += 1;
        previous = byte;
    }
    if label_len == 0 || label_len > 63 || !previous.is_ascii_alphanumeric() {
        return Err(PolicyError::MalformedHost);
    }
    Ok(())
}

fn is_blocked_hostname(host: &str) -> bool {
    matches!(
        host,
        "localhost" | "localhost.localdomain" | "metadata.google.internal" | "metadata.goog"
    ) || [".localhost", ".local", ".localdomain", ".internal"]
        .iter()
        .any(|suffix| host.len() > suffix.len() && host.ends_with(suffix))
}

fn parse_strict_ipv4(host: &str) -> Result<Ipv4Parse, PolicyError> {
    let mut octets = [0_u8; 4];
    let mut count = 0;
    for part in host.split('.') {
        if count == 4 {
            return Err(PolicyError::InvalidIpv4);
        }
        if part.is_empty() {
            return Ok(Ipv4Parse::NotIpv4);
        }
        if part.len() > 1 && part.starts_with('0') {
            return Err(PolicyError::InvalidIpv4);
        }
        if !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Ok(Ipv4Parse::NotIpv4);
        }
        octets[count] = part.parse().map_err(|_| PolicyError::InvalidIpv4)?;
        count += 1;
    }
    match count {
        4 => Ok(Ipv4Parse::Address(octets)),
        2.. => Err(PolicyError::InvalidIpv4),
        _ => Ok(Ipv4Parse::NotIpv4),
    }
}

fn looks_like_ambiguous_ipv4(host: &str) -> bool {
    if host.contains("0x") || host.contains("0X") {
        return true;
    }
    let mut labels = 0;
    let mut numeric_labels = 0;
    for label in host.split('.') {
        labels += 1;
        if label.is_empty() {
            return true;
        }
        if label.bytes().all(|byte| byte.is_ascii_digit()) {
            numeric_labels += 1;
        }
    }
    (numeric_labels == labels && labels > 1) || (labels == 4 && numeric_labels > 0)
}

fn reject_control_bytes(value: &str) -> Result<(), PolicyError> {
    if value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(PolicyError::ControlByte);
    }
    Ok(())
}

fn validate_percent_encoding(value: &[u8]) -> Result<(), PolicyError> {
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%' {
            if index + 2 >= value.len()
                || !value[index + 1].is_ascii_hexdigit()
                || !value[index + 2].is_ascii_hexdigit()
            {
                return Err(PolicyError::MalformedPercentEncoding);
            }
            index += 2;
        }
        index += 1;
    }
    Ok(())
}

fn strip_fragment(path_query_fragment: &str) -> &str {
    path_query_fragment
        .find('#')
        .map_or(path_query_fragment, |end| &path_query_fragment[..end])
}

fn normalize_path_query(path_query: &str) -> Result<String, PolicyError> {
    if path_query
        .bytes()
        .any(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
    {
        return Err(PolicyError::RequestTargetWhitespace);
    }
    if path_query.is_empty() {
        return Ok("/".to_owned());
    }
    if path_query.starts_with('?') {
        return Ok(format!("/{path_query}"));
    }
    Ok(path_query.to_owned())
}

fn format_retrieval_url(host: &str, explicit_port: Option<u16>, path_query: &str) -> String {
    let host = host_for_url(host);
    match explicit_port {
        Some(port) => format!("https://{host}:{port}{path_query}"),
        None => format!("https://{host}{path_query}"),
    }
}

fn host_for_url(canonical_host: &str) -> String {
    if canonical_host.contains(':') {
        format!("[{canonical_host}]")
    } else {
        canonical_host.to_owned()
    }
}

fn absolute_redirect_url(current: &ValidatedUrl, location: &str) -> Result<String, PolicyError> {
    let location = strip_fragment(location);
    if location.is_empty() {
        return Ok(current.retrieval_url.clone());
    }
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_owned());
    }
    if location.starts_with("//") {
        return Ok(format!("https:{location}"));
    }
    if has_unsupported_absolute_scheme(location) {
        return Err(PolicyError::UnsupportedScheme);
    }
    let path = if location.starts_with('/') {
        location.to_owned()
    } else {
        merge_relative_path(&current.path_query, location)
    };
    Ok(format_retrieval_url(
        &current.canonical_host,
        current.explicit_port,
        &path,
    ))
}

fn has_unsupported_absolute_scheme(value: &str) -> bool {
    let Some(colon) = value.find(':') else {
        return false;
    };
    let slash = value.find('/').unwrap_or(value.len());
    let question = value.find('?').unwrap_or(value.len());
    colon < slash && colon < question
}

fn merge_relative_path(current_path_query: &str, relative: &str) -> String {
    let current_path = current_path_query
        .find('?')
        .map_or(current_path_query, |end| &current_path_query[..end]);
    let base_end = current_path.rfind('/').map_or(0, |slash| slash + 1);
    let (relative_path, relative_query) =
        relative.split_at(relative.find('?').unwrap_or(relative.len()));
    let joined = format!("{}{relative_path}", &current_path[..base_end]);
    format!("{}{relative_query}", remove_dot_segments(&joined))
}

fn remove_dot_segments(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    let mut out = format!("/{}", segments.join("/"));
    if path.ends_with('/') && !segments.is_empty() {
        out.push('/');
    }
    out
}

fn same_host_or_optional_www(current: &str, target: &str) -> bool {
    current == target || strip_one_www(current) == strip_one_www(target)
}

fn strip_one_www(host: &str) -> &str {
    match host.strip_prefix("www.") {
        Some(rest) if !rest.is_empty() => rest,
        _ => host,
    }
}

fn is_public_ipv4(octets: [u8; 4]) -> bool {
    let value = u32::from_be_bytes(octets);
    !BLOCKED_IPV4_RANGES.iter().any(|(prefix, bits)| {
        let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
        value & mask == u32::from_be_bytes(*prefix) & mask
    })
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let bytes = address.octets();
    let global_unicast = bytes[0] & 0xe0 == 0x20;
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped.octets()) && global_unicast;
    }
    if !global_unicast {
        return false;
    }
    let teredo = bytes[0] == 0x20 && bytes[1] == 0x01 && bytes[2] & 0xfe == 0;
    let documentation = bytes[..4] == [0x20, 0x01, 0x0d, 0xb8];
    let six_to_four = bytes[0] == 0x20 && bytes[1] == 0x02;
    let reserved = bytes[0] == 0x3f && bytes[1] & 0xf0 == 0xf0;
    !(teredo || documentation || six_to_four || reserved)
}

#[cfg(test)]
mod tests;
