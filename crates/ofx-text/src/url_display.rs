use std::borrow::Cow;

use crate::display_width::{prefix_by_width, visible_width};

const REDACTED: &str = "[redacted]";
const CLIP_MARKER: &str = "...";
const SENSITIVE_QUERY_KEY_PARTS: &str = "password passwd token api_key apikey secret authorization cookie signature credential access_key private_key";

pub fn redact_url_for_display(raw_url: &str) -> String {
    let mut out = String::with_capacity(raw_url.len());
    let rest = match raw_url.split_once("://") {
        Some((scheme, after_scheme)) => {
            let authority_end = after_scheme
                .find(['/', '?', '#'])
                .unwrap_or(after_scheme.len());
            let (authority, rest) = after_scheme
                .split_at_checked(authority_end)
                .unwrap_or((after_scheme, ""));
            out.push_str(scheme);
            out.push_str("://");
            match authority.split_once('@') {
                Some((_, host)) => {
                    out.push_str(REDACTED);
                    out.push('@');
                    out.push_str(host);
                }
                None => out.push_str(authority),
            }
            rest
        }
        None => raw_url,
    };
    let Some((before_query, query_and_fragment)) = rest.split_once('?') else {
        out.push_str(rest);
        return out;
    };
    out.push_str(before_query);
    out.push('?');
    let (query, fragment) = query_and_fragment
        .split_once('#')
        .map_or((query_and_fragment, None), |(query, fragment)| {
            (query, Some(fragment))
        });
    for (index, field) in query.split('&').enumerate() {
        if index > 0 {
            out.push('&');
        }
        match field.split_once('=') {
            Some((key, value)) => {
                out.push_str(key);
                out.push('=');
                out.push_str(if sensitive_url_query_key(key.as_bytes()) {
                    REDACTED
                } else {
                    value
                });
            }
            None => out.push_str(field),
        }
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

pub fn clipped_label(text: &str, max_width: usize) -> Cow<'_, str> {
    if max_width == 0 {
        return Cow::Borrowed("");
    }
    if visible_width(text) <= max_width {
        return Cow::Borrowed(text);
    }
    let keep = prefix_by_width(text, max_width.saturating_sub(CLIP_MARKER.len()));
    if keep.is_empty() {
        return Cow::Borrowed(prefix_by_width(text, max_width));
    }
    Cow::Owned(format!("{keep}{CLIP_MARKER}"))
}

fn sensitive_url_query_key(key: &[u8]) -> bool {
    percent_decoded_eq_ignore_case(key, b"sig")
        || SENSITIVE_QUERY_KEY_PARTS
            .split(' ')
            .any(|needle| percent_decoded_contains_ignore_case(key, needle.as_bytes()))
}

fn percent_decoded_eq_ignore_case(text: &[u8], expected: &[u8]) -> bool {
    let mut index = 0;
    for expected_byte in expected {
        match next_percent_decoded_byte(text, &mut index) {
            Some(byte) if byte.eq_ignore_ascii_case(expected_byte) => {}
            _ => return false,
        }
    }
    index == text.len()
}

fn percent_decoded_contains_ignore_case(text: &[u8], needle: &[u8]) -> bool {
    (0..text.len()).any(|start| {
        let mut index = start;
        needle.iter().all(|needle_byte| {
            next_percent_decoded_byte(text, &mut index)
                .is_some_and(|byte| byte.eq_ignore_ascii_case(needle_byte))
        })
    })
}

fn next_percent_decoded_byte(text: &[u8], index: &mut usize) -> Option<u8> {
    let byte = *text.get(*index)?;
    if byte == b'%' && *index + 2 < text.len() {
        let high = char::from(text[*index + 1]).to_digit(16);
        let low = char::from(text[*index + 2]).to_digit(16);
        if let (Some(high), Some(low)) = (high, low) {
            *index += 3;
            return u8::try_from(high * 16 + low).ok();
        }
    }
    *index += 1;
    Some(byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_for_display_masks_credential_like_query_values() {
        assert_eq!(
            redact_url_for_display(
                "https://user:pass@example.com/docs?safe=ok&token=abc123&X-Amz-%43redential=credential-value&X-Amz-Signature=signature-value"
            ),
            "https://[redacted]@example.com/docs?safe=ok&token=[redacted]&X-Amz-%43redential=[redacted]&X-Amz-Signature=[redacted]"
        );
    }

    #[test]
    fn redact_url_for_display_preserves_benign_keys_containing_sig() {
        assert_eq!(
            redact_url_for_display("https://example.com/docs?design=blue&sig=secret"),
            "https://example.com/docs?design=blue&sig=[redacted]"
        );
    }

    #[test]
    fn redact_url_for_display_keeps_fragments_bare_keys_and_scheme_less_text() {
        assert_eq!(
            redact_url_for_display("https://example.com/a?flag&apikey=1#token=2"),
            "https://example.com/a?flag&apikey=[redacted]#token=2"
        );
        assert_eq!(
            redact_url_for_display("example.com/a?password=x"),
            "example.com/a?password=[redacted]"
        );
        assert_eq!(
            redact_url_for_display("https://example.com/plain"),
            "https://example.com/plain"
        );
        assert_eq!(
            redact_url_for_display("https://example.com?Sig=1&%73ig=2&sign=3"),
            "https://example.com?Sig=[redacted]&%73ig=[redacted]&sign=3"
        );
    }

    #[test]
    fn clipped_label_clips_by_display_width_with_an_ellipsis() {
        assert_eq!(clipped_label("ab\u{1f600}cd", 5), "ab...");
        assert_eq!(clipped_label("short", 5), "short");
        assert_eq!(clipped_label("abcdef", 3), "abc");
        assert_eq!(clipped_label("abcdef", 0), "");
    }
}
