pub(crate) fn parse_unsigned(text: &[u8], radix: u32) -> Option<u64> {
    let (negative, digits) = match text.split_first() {
        Some((b'+', rest)) => (false, rest),
        Some((b'-', rest)) => (true, rest),
        _ => (false, text),
    };
    if digits.first().is_none_or(|byte| *byte == b'_') || digits.last() == Some(&b'_') {
        return None;
    }
    let mut value: u64 = 0;
    for byte in digits.iter().filter(|byte| **byte != b'_') {
        let digit = char::from(*byte).to_digit(radix)?;
        value = value
            .checked_mul(u64::from(radix))?
            .checked_add(u64::from(digit))?;
    }
    (!negative || value == 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_parsing_accepts_signs_and_inner_underscores_as_upstream_does() {
        assert_eq!(parse_unsigned(b"+80", 10), Some(80));
        assert_eq!(parse_unsigned(b"8_0", 10), Some(80));
        assert_eq!(parse_unsigned(b"-0", 10), Some(0));
        assert_eq!(parse_unsigned(b"-00", 16), Some(0));
        assert_eq!(parse_unsigned(b"fF", 16), Some(255));
        for text in [
            &b""[..],
            b"+",
            b"-",
            b"-1",
            b"_1",
            b"1_",
            b"+_1",
            b"1 ",
            b"0x1",
            b"99999999999999999999",
        ] {
            assert_eq!(parse_unsigned(text, 10), None, "{text:?}");
        }
    }
}
