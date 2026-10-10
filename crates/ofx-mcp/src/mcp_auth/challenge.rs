use reqwest::header::{HeaderMap, WWW_AUTHENTICATE};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Challenge {
    pub(crate) resource_metadata: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) insufficient_scope: bool,
}

pub(crate) fn collect_authenticate_header(headers: &HeaderMap) -> Option<String> {
    let values: Vec<String> = headers
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .map(|value| {
            String::from_utf8_lossy(value.as_bytes())
                .trim_matches([' ', '\t', '\r', '\n'])
                .to_owned()
        })
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

pub(crate) fn parse_challenge(value: &str) -> Option<Challenge> {
    let bytes = value.as_bytes();
    let mut challenge = Challenge::default();
    let Some(mut cursor) = bearer_parameters(bytes) else {
        return Some(challenge);
    };
    while cursor < bytes.len() {
        while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t' | b',') {
            cursor += 1;
        }
        let key_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || matches!(bytes[cursor], b'_' | b'-'))
        {
            cursor += 1;
        }
        if cursor == key_start {
            break;
        }
        let key = &value[key_start..cursor];
        while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'=' {
            break;
        }
        cursor += 1;
        while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
            cursor += 1;
        }
        let parsed = challenge_value(bytes, &mut cursor)?;
        if key.eq_ignore_ascii_case("resource_metadata") {
            challenge.resource_metadata = Some(parsed);
        } else if key.eq_ignore_ascii_case("scope") {
            challenge.scope = Some(parsed);
        } else if key.eq_ignore_ascii_case("error") && parsed == "insufficient_scope" {
            challenge.insufficient_scope = true;
        }
    }
    Some(challenge)
}

fn bearer_parameters(bytes: &[u8]) -> Option<usize> {
    const SCHEME: &[u8] = b"bearer";
    if bytes.len() < SCHEME.len() {
        return None;
    }
    (0..=bytes.len() - SCHEME.len()).find_map(|index| {
        let after = index + SCHEME.len();
        let matched = bytes[index..after].eq_ignore_ascii_case(SCHEME)
            && (index == 0 || matches!(bytes[index - 1], b',' | b' ' | b'\t'))
            && (after == bytes.len() || matches!(bytes[after], b' ' | b'\t'));
        matched.then_some(after)
    })
}

fn challenge_value(bytes: &[u8], cursor: &mut usize) -> Option<String> {
    if *cursor >= bytes.len() {
        return None;
    }
    if bytes[*cursor] != b'"' {
        let start = *cursor;
        while *cursor < bytes.len() && !matches!(bytes[*cursor], b',' | b' ') {
            *cursor += 1;
        }
        return Some(String::from_utf8_lossy(&bytes[start..*cursor]).into_owned());
    }
    *cursor += 1;
    let mut out = Vec::new();
    while *cursor < bytes.len() {
        let byte = bytes[*cursor];
        *cursor += 1;
        match byte {
            b'"' => return Some(String::from_utf8_lossy(&out).into_owned()),
            b'\\' => {
                out.push(*bytes.get(*cursor)?);
                *cursor += 1;
            }
            other => out.push(other),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    #[test]
    fn the_parser_keeps_only_non_secret_authorization_directions() {
        let challenge = parse_challenge(
            r#"BEARER realm="mcp", error="insufficient_scope", scope="tools.read tools.call", resource_metadata="https://api.example/.well-known/oauth-protected-resource/mcp""#,
        )
        .unwrap();
        assert!(challenge.insufficient_scope);
        assert_eq!(challenge.scope.as_deref(), Some("tools.read tools.call"));
        assert_eq!(
            challenge.resource_metadata.as_deref(),
            Some("https://api.example/.well-known/oauth-protected-resource/mcp")
        );
    }

    #[test]
    fn header_collection_preserves_multiple_challenges() {
        let mut headers = HeaderMap::new();
        headers.append(
            WWW_AUTHENTICATE,
            HeaderValue::from_static(" Basic realm=\"legacy\" "),
        );
        headers.append(
            WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer scope=\"tools.call\""),
        );
        let joined = collect_authenticate_header(&headers).unwrap();
        assert_eq!(joined, r#"Basic realm="legacy", Bearer scope="tools.call""#);
        assert_eq!(
            parse_challenge(&joined).unwrap().scope.as_deref(),
            Some("tools.call")
        );
        assert_eq!(collect_authenticate_header(&HeaderMap::new()), None);
    }

    #[test]
    fn untrusted_challenges_parse_or_fail_as_upstream_does() {
        for malformed in [
            r#"Bearer scope="open"#,
            r#"Bearer scope="trailing\"#,
            "Bearer scope=",
        ] {
            assert_eq!(parse_challenge(malformed), None, "{malformed}");
        }
        for (value, scope) in [
            ("Bearer,scope=x", None),
            ("Basic realm=\"x bearer y\"", None),
            ("Bearer scope=a\tb", Some("a\tb")),
            ("Bearer scope=\"a\\\"b\"", Some("a\"b")),
            ("Bearer scope=x, Basic realm=y", Some("x")),
            ("Bearer", None),
            ("", None),
        ] {
            assert_eq!(
                parse_challenge(value).unwrap().scope.as_deref(),
                scope,
                "{value}"
            );
        }
        let quoted = parse_challenge(r#"Basic realm="a bearer b" scope=x"#).unwrap();
        assert_eq!(quoted.scope, None);
    }
}
