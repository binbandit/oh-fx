const WHITESPACE: &[u8] = b" \t\n\r";
const MAX_U64_DIGITS: i64 = 20;
const F128_FRACTION_BITS: u32 = 112;
const UNDERFLOW_ORDER: i64 = -4965;
const UNDERFLOW_DIGITS: &str = "3237587559719012555462219479113823276249784669017340504844";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Value<'a> {
    Null,
    Bool(bool),
    Number(&'a str),
    String(String),
    Array(Vec<&'a [u8]>),
    Object,
}

pub(crate) fn skip_whitespace(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(|byte| WHITESPACE.contains(byte)) {
        at += 1;
    }
    at
}

pub(crate) fn value_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut open: Vec<u8> = Vec::new();
    let mut at = start;
    loop {
        at = skip_whitespace(bytes, at);
        match *bytes.get(at)? {
            open_brace @ (b'{' | b'[') => {
                let close = if open_brace == b'{' { b'}' } else { b']' };
                let inner = skip_whitespace(bytes, at + 1);
                if bytes.get(inner) == Some(&close) {
                    at = inner + 1;
                } else {
                    open.push(open_brace);
                    at = if open_brace == b'{' {
                        object_key_end(bytes, inner)?
                    } else {
                        inner
                    };
                    continue;
                }
            }
            b'"' => at = string_end(bytes, at)?,
            b't' => at = literal_end(bytes, at, b"true")?,
            b'f' => at = literal_end(bytes, at, b"false")?,
            b'n' => at = literal_end(bytes, at, b"null")?,
            b'-' | b'0'..=b'9' => at = number_end(bytes, at)?,
            _ => return None,
        }
        loop {
            let Some(&container) = open.last() else {
                return Some(at);
            };
            at = skip_whitespace(bytes, at);
            match (container, *bytes.get(at)?) {
                (_, b',') => {
                    at += 1;
                    if container == b'{' {
                        at = object_key_end(bytes, skip_whitespace(bytes, at))?;
                    }
                    break;
                }
                (b'{', b'}') | (b'[', b']') => {
                    open.pop();
                    at += 1;
                }
                _ => return None,
            }
        }
    }
}

fn object_key_end(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes.get(at) != Some(&b'"') {
        return None;
    }
    let after = skip_whitespace(bytes, string_end(bytes, at)?);
    (bytes.get(after) == Some(&b':')).then_some(after + 1)
}

fn literal_end(bytes: &[u8], at: usize, literal: &[u8]) -> Option<usize> {
    bytes[at..]
        .starts_with(literal)
        .then_some(at + literal.len())
}

fn digits_end(bytes: &[u8], at: usize) -> usize {
    at + bytes[at..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
}

fn number_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut at = start + usize::from(bytes[start] == b'-');
    match bytes.get(at)? {
        b'0' => at += 1,
        b'1'..=b'9' => at = digits_end(bytes, at),
        _ => return None,
    }
    if bytes.get(at) == Some(&b'.') {
        let fraction = digits_end(bytes, at + 1);
        if fraction == at + 1 {
            return None;
        }
        at = fraction;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let exponent = digits_end(bytes, at);
        if exponent == at {
            return None;
        }
        at = exponent;
    }
    Some(at)
}

pub(crate) fn string_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut at = open + 1;
    let mut pending_high = false;
    loop {
        let byte = *bytes.get(at)?;
        if pending_high && byte != b'\\' {
            return None;
        }
        match byte {
            b'"' => break,
            b'\\' => {
                let escaped = *bytes.get(at + 1)?;
                if escaped == b'u' {
                    let unit = hex4(bytes.get(at + 2..at + 6)?)?;
                    match unit {
                        0xd800..=0xdbff if !pending_high => pending_high = true,
                        0xdc00..=0xdfff if pending_high => pending_high = false,
                        _ if pending_high => return None,
                        0xdc00..=0xdfff => return None,
                        _ => {}
                    }
                    at += 6;
                } else {
                    if pending_high
                        || !matches!(
                            escaped,
                            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                        )
                    {
                        return None;
                    }
                    at += 2;
                }
            }
            0..=0x1f => return None,
            _ => at += 1,
        }
    }
    std::str::from_utf8(&bytes[open + 1..at]).ok()?;
    Some(at + 1)
}

fn hex4(digits: &[u8]) -> Option<u32> {
    digits.iter().try_fold(0_u32, |value, &digit| {
        Some((value << 4) | char::from(digit).to_digit(16)?)
    })
}

pub(crate) fn decode_string(token: &[u8]) -> Option<String> {
    let inner = token.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    let mut out = String::with_capacity(inner.len());
    let text = std::str::from_utf8(inner).ok()?;
    let mut chars = text.char_indices();
    while let Some((index, character)) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        let (_, escaped) = chars.next()?;
        out.push(match escaped {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => {
                let unit = hex4(inner.get(index + 2..index + 6)?)?;
                for _ in 0..4 {
                    chars.next();
                }
                if (0xd800..=0xdbff).contains(&unit) {
                    let low_at = index + 6;
                    if inner.get(low_at..low_at + 2) != Some(b"\\u") {
                        return None;
                    }
                    let low = hex4(inner.get(low_at + 2..low_at + 6)?)?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return None;
                    }
                    for _ in 0..6 {
                        chars.next();
                    }
                    char::from_u32(0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00))?
                } else {
                    char::from_u32(unit)?
                }
            }
            _ => return None,
        });
    }
    Some(out)
}

pub(crate) fn parse(raw: &[u8]) -> Option<Value<'_>> {
    let start = skip_whitespace(raw, 0);
    let end = value_end(raw, start)?;
    if skip_whitespace(raw, end) != raw.len() {
        return None;
    }
    let token = &raw[start..end];
    Some(match token[0] {
        b'n' => Value::Null,
        b't' => Value::Bool(true),
        b'f' => Value::Bool(false),
        b'"' => Value::String(decode_string(token)?),
        b'{' => Value::Object,
        b'[' => Value::Array(elements(token)?),
        _ => Value::Number(std::str::from_utf8(token).ok()?),
    })
}

fn elements(array: &[u8]) -> Option<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut at = skip_whitespace(array, 1);
    if array.get(at) == Some(&b']') {
        return Some(out);
    }
    loop {
        let end = value_end(array, at)?;
        out.push(&array[at..end]);
        at = skip_whitespace(array, end);
        match array.get(at)? {
            b',' => at = skip_whitespace(array, at + 1),
            b']' => return Some(out),
            _ => return None,
        }
    }
}

pub(crate) fn slice_to_u64(text: &str) -> Option<u64> {
    if text == "-0" || text.contains(['.', 'e', 'E']) {
        integral_decimal(text)
    } else {
        parse_integer(text)
    }
}

fn parse_integer(text: &str) -> Option<u64> {
    let (negative, digits) = split_sign(text);
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') {
        return None;
    }
    let mut value: u64 = 0;
    for digit in digits.bytes().filter(|&byte| byte != b'_') {
        let digit = char::from(digit).to_digit(10)?;
        value = value.checked_mul(10)?.checked_add(u64::from(digit))?;
    }
    (!negative || value == 0).then_some(value)
}

fn split_sign(text: &str) -> (bool, &str) {
    match text.as_bytes().first() {
        Some(b'+') => (false, &text[1..]),
        Some(b'-') => (true, &text[1..]),
        _ => (false, text),
    }
}

fn integral_decimal(text: &str) -> Option<u64> {
    let (negative, signed) = split_sign(text);
    if !valid_underscores(signed) {
        return None;
    }
    let unsigned: String = signed
        .chars()
        .filter(|&character| character != '_')
        .collect();
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], exponent_of(&unsigned[at + 1..])?),
        None => (unsigned.as_str(), 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits_valid = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty()) || !digits_valid(whole) || !digits_valid(fraction)
    {
        return None;
    }
    let digits: String = whole.chars().chain(fraction.chars()).collect();
    let leading = digits.trim_start_matches('0');
    let significant = leading.trim_end_matches('0');
    if significant.is_empty() {
        return Some(0);
    }
    let trailing = i64::try_from(leading.len() - significant.len()).ok()?;
    let fraction_len = i64::try_from(fraction.len()).ok()?;
    let scale = exponent
        .saturating_sub(fraction_len)
        .saturating_add(trailing);
    let order = i64::try_from(significant.len()).ok()?.saturating_add(scale);
    if order > MAX_U64_DIGITS {
        return None;
    }
    if order < UNDERFLOW_ORDER || (order == UNDERFLOW_ORDER && significant <= UNDERFLOW_DIGITS) {
        return Some(0);
    }
    if negative || order < 0 {
        return None;
    }
    if scale >= 0 {
        let zeros = usize::try_from(scale).ok()?;
        return format!("{significant}{}", "0".repeat(zeros)).parse().ok();
    }
    let (whole, fraction) = significant.split_at(usize::try_from(order).ok()?);
    let floor: u128 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    if floor > 0 && fraction <= half_spacing(floor, false)?.as_str() {
        return u64::try_from(floor).ok();
    }
    let ceiling = floor + 1;
    let gap = complement(fraction);
    if gap <= half_spacing(ceiling, true)? {
        return u64::try_from(ceiling).ok();
    }
    None
}

fn half_spacing(integer: u128, below: bool) -> Option<String> {
    let narrower = below && integer.is_power_of_two();
    let bits = (F128_FRACTION_BITS + 1 + u32::from(narrower)).checked_sub(integer.ilog2())?;
    let places = usize::try_from(bits).ok()?;
    let mut digits = vec![1_u8];
    for _ in 0..places {
        let mut carry = 0;
        for digit in &mut digits {
            let product = *digit * 5 + carry;
            *digit = product % 10;
            carry = product / 10;
        }
        if carry > 0 {
            digits.push(carry);
        }
    }
    digits.resize(places, 0);
    Some(
        digits
            .iter()
            .rev()
            .map(|digit| char::from(b'0' + digit))
            .collect(),
    )
}

fn complement(fraction: &str) -> String {
    let last = fraction.len() - 1;
    fraction
        .bytes()
        .enumerate()
        .map(|(at, digit)| {
            let whole = if at == last { 10 } else { 9 };
            char::from(b'0' + whole - (digit - b'0'))
        })
        .collect()
}

fn valid_underscores(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().all(|(at, &byte)| {
        byte != b'_'
            || (at > 0
                && bytes[at - 1].is_ascii_digit()
                && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
    })
}

fn exponent_of(text: &str) -> Option<i64> {
    let (negative, digits) = split_sign(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let magnitude = digits.bytes().fold(0_i64, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'))
    });
    Some(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
mod tests;
