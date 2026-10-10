use std::borrow::Cow;
use std::iter;

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

pub fn fixed_decimal(value: f64, precision: usize) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return value.to_string();
    }
    let shortest = value.to_string();
    let (sign, digits) = match shortest.strip_prefix('-') {
        Some(digits) => ("-", digits),
        None => ("", shortest.as_str()),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let mut kept: Vec<u8> = whole
        .bytes()
        .chain(fraction.bytes().chain(iter::repeat(b'0')).take(precision))
        .collect();
    if fraction
        .as_bytes()
        .get(precision)
        .is_some_and(|digit| *digit >= b'5')
    {
        round_up(&mut kept);
    }
    let point = kept.len() - precision;
    let mut text = String::with_capacity(sign.len() + kept.len() + 1);
    text.push_str(sign);
    text.extend(kept[..point].iter().map(|digit| char::from(*digit)));
    if precision > 0 {
        text.push('.');
        text.extend(kept[point..].iter().map(|digit| char::from(*digit)));
    }
    text
}

fn round_up(digits: &mut Vec<u8>) {
    for digit in digits.iter_mut().rev() {
        if *digit == b'9' {
            *digit = b'0';
        } else {
            *digit += 1;
            return;
        }
    }
    digits.insert(0, b'1');
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
    fn fixed_decimal_rounds_the_shortest_digits_half_up() {
        let cases: [(f64, usize, &str); 18] = [
            (0.25, 4, "0.2500"),
            (0.0, 4, "0.0000"),
            (-0.0, 4, "-0.0000"),
            (12.0, 4, "12.0000"),
            (0.031_25, 4, "0.0313"),
            (0.031_249, 4, "0.0312"),
            (0.000_05, 4, "0.0001"),
            (0.000_049_999, 4, "0.0000"),
            (0.999_95, 4, "1.0000"),
            (9.999_96, 4, "10.0000"),
            (1e-7, 4, "0.0000"),
            (1e21, 2, "1000000000000000000000.00"),
            (2.5, 0, "3"),
            (2.4, 0, "2"),
            (1.005, 2, "1.01"),
            (99.95, 1, "100.0"),
            (f64::INFINITY, 4, "inf"),
            (f64::NAN, 4, "nan"),
        ];
        for (value, precision, expected) in cases {
            assert_eq!(
                fixed_decimal(value, precision),
                expected,
                "{value} {precision}"
            );
        }
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
