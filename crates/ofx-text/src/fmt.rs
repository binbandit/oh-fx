use std::borrow::Cow;

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const PLAIN_WORD_PUNCTUATION: &[u8] = b"/._-+,:@%";

pub fn lowercase_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| {
            [
                HEX_DIGITS[usize::from(byte >> 4)],
                HEX_DIGITS[usize::from(byte & 0x0f)],
            ]
        })
        .map(char::from)
        .collect()
}

pub fn shell_word(text: &str) -> Cow<'_, str> {
    let plain = !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || PLAIN_WORD_PUNCTUATION.contains(&byte));
    if plain {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("'{}'", text.replace('\'', r"'\''")))
    }
}

pub fn parse_unsigned<T: TryFrom<u64>>(text: &str) -> Option<T> {
    if text.is_empty() || text.starts_with('_') || text.ends_with('_') {
        return None;
    }
    let value = text
        .bytes()
        .filter(|byte| *byte != b'_')
        .try_fold(0_u64, |value, byte| {
            let digit = char::from(byte).to_digit(10)?;
            value.checked_mul(10)?.checked_add(u64::from(digit))
        })?;
    T::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_hex_writes_two_digits_per_byte() {
        assert_eq!(lowercase_hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(lowercase_hex(&[]), "");
    }

    #[test]
    fn shell_word_quotes_everything_but_plain_path_characters() {
        assert_eq!(
            shell_word("/ws/v1.2_a-b+c,d:e@f%g"),
            "/ws/v1.2_a-b+c,d:e@f%g"
        );
        assert_eq!(shell_word("/ws/x, profile=clean"), "'/ws/x, profile=clean'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word("caf\u{e9}"), "'caf\u{e9}'");
        assert_eq!(shell_word(""), "''");
    }

    #[test]
    fn parse_unsigned_accepts_digit_separators_but_no_sign() {
        assert_eq!(parse_unsigned::<u64>("050124"), Some(50124));
        assert_eq!(parse_unsigned::<u64>("65_535"), Some(65535));
        assert_eq!(parse_unsigned::<u64>("_10"), None);
        assert_eq!(parse_unsigned::<u64>("10_"), None);
        assert_eq!(parse_unsigned::<u64>("+0"), None);
        assert_eq!(parse_unsigned::<u64>("-0"), None);
        assert_eq!(parse_unsigned::<u64>(" 10"), None);
        assert_eq!(parse_unsigned::<u64>(""), None);
        assert_eq!(parse_unsigned::<u64>("18446744073709551616"), None);
    }

    #[test]
    fn parse_unsigned_rejects_values_outside_the_target_type() {
        assert_eq!(parse_unsigned::<u8>("255"), Some(255));
        assert_eq!(parse_unsigned::<u8>("256"), None);
        assert_eq!(parse_unsigned::<usize>("100"), Some(100));
    }
}
