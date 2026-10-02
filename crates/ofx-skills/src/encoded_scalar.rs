use ofx_text::write_scalar;

pub(crate) fn encoded_scalar(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    write_scalar(&mut encoded, value);
    encoded
}

pub(crate) fn bounded_encoded_prefix(encoded: &str, max_bytes: usize) -> &str {
    let prefix = &encoded[..encoded.floor_char_boundary(max_bytes)];
    match prefix.rfind('&') {
        Some(entity_start) if !prefix[entity_start..].contains(';') => &prefix[..entity_start],
        _ => prefix,
    }
}

pub(crate) fn write_bounded_encoded_scalar(
    output: &mut String,
    value: &str,
    max_bytes: usize,
) -> usize {
    let encoded = encoded_scalar(value);
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
        assert_eq!(bounded_encoded_prefix(&encoded, 3), "a");
        assert_eq!(bounded_encoded_prefix(&encoded, 5), "a&lt;");
        assert_eq!(bounded_encoded_prefix(&encoded, 7), "a&lt;b");
        assert_eq!(bounded_encoded_prefix(&encoded, 8), encoded);
        let mut output = String::new();
        assert_eq!(write_bounded_encoded_scalar(&mut output, "<&>", 9), 13);
        assert_eq!(output, "&lt;&amp;");
    }
}
