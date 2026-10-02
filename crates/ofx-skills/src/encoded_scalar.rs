use ofx_config::utf8_prefix_length;
use ofx_text::write_scalar;

pub(crate) fn encoded_scalar(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    write_scalar(&mut encoded, value);
    encoded
}

pub(crate) fn encoded_bytes(value: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(value.len());
    for chunk in value.utf8_chunks() {
        encoded.extend_from_slice(encoded_scalar(chunk.valid()).as_bytes());
        encoded.extend_from_slice(chunk.invalid());
    }
    encoded
}

pub(crate) fn bounded_encoded_prefix(encoded: &[u8], max_bytes: usize) -> &str {
    let valid = &encoded[..utf8_prefix_length(encoded, max_bytes)];
    let prefix = std::str::from_utf8(valid).unwrap_or_default();
    match prefix.rfind('&') {
        Some(entity_start) if !prefix[entity_start..].contains(';') => &prefix[..entity_start],
        _ => prefix,
    }
}

pub(crate) fn write_bounded_encoded_scalar(
    output: &mut String,
    value: &[u8],
    max_bytes: usize,
) -> usize {
    let encoded = encoded_bytes(value);
    output.push_str(bounded_encoded_prefix(&encoded, max_bytes));
    encoded.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_encoded_prefixes_never_split_an_entity_or_a_character() {
        let encoded = encoded_scalar("a<b\u{e9}");
        assert_eq!(encoded, "a&lt;b\u{e9}");
        let encoded = encoded.as_bytes();
        assert_eq!(bounded_encoded_prefix(encoded, 3), "a");
        assert_eq!(bounded_encoded_prefix(encoded, 5), "a&lt;");
        assert_eq!(bounded_encoded_prefix(encoded, 7), "a&lt;b");
        assert_eq!(bounded_encoded_prefix(encoded, 8), "a&lt;b\u{e9}");
        let mut output = String::new();
        assert_eq!(write_bounded_encoded_scalar(&mut output, b"<&>", 9), 13);
        assert_eq!(output, "&lt;&amp;");
    }

    #[test]
    fn encoded_bytes_keep_invalid_utf_8_raw_and_prefixes_stop_before_it() {
        let encoded = encoded_bytes(b"a\n\xe9<b\xff");
        assert_eq!(encoded, b"a&#x0a;\xe9&lt;b\xff");
        assert_eq!(bounded_encoded_prefix(&encoded, usize::MAX), "a&#x0a;");
        let mut output = String::new();
        assert_eq!(
            write_bounded_encoded_scalar(&mut output, b"caf\xe9/x", 4096),
            6
        );
        assert_eq!(output, "caf");
    }
}
