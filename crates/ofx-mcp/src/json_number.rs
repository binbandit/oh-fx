use std::cmp::Ordering;

use serde_json::Value;

const MAX_LEXEME_BYTES: usize = 4096;
const MAX_EXPONENT_ABS: i64 = 1_000_000;
const MAX_EXPANDED_DIGITS: usize = 8192;
const LIMB_BASE: u64 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumberError {
    InvalidNumber,
    NumberLimitExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Number {
    negative: bool,
    digits: Vec<u8>,
    exponent: i64,
}

impl Number {
    pub(crate) fn parse(lexeme: &str) -> Result<Self, NumberError> {
        let bytes = lexeme.as_bytes();
        if bytes.is_empty() {
            return Err(NumberError::InvalidNumber);
        }
        if bytes.len() > MAX_LEXEME_BYTES {
            return Err(NumberError::NumberLimitExceeded);
        }
        let mut index = 0;
        let negative = bytes[0] == b'-';
        if negative {
            index += 1;
        }
        let mut raw_digits = Vec::new();
        match bytes.get(index) {
            Some(b'0') => {
                raw_digits.push(b'0');
                index += 1;
                if bytes.get(index).is_some_and(u8::is_ascii_digit) {
                    return Err(NumberError::InvalidNumber);
                }
            }
            Some(b'1'..=b'9') => {
                while let Some(&digit) = bytes.get(index).filter(|byte| byte.is_ascii_digit()) {
                    raw_digits.push(digit);
                    index += 1;
                }
            }
            _ => return Err(NumberError::InvalidNumber),
        }
        let mut fraction_digits = 0;
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            let fraction_start = index;
            while let Some(&digit) = bytes.get(index).filter(|byte| byte.is_ascii_digit()) {
                raw_digits.push(digit);
                index += 1;
            }
            if index == fraction_start {
                return Err(NumberError::InvalidNumber);
            }
            fraction_digits = index - fraction_start;
        }
        let mut explicit_exponent: i64 = 0;
        if matches!(bytes.get(index), Some(b'e' | b'E')) {
            index += 1;
            let exponent_negative = match bytes.get(index) {
                None => return Err(NumberError::InvalidNumber),
                Some(sign @ (b'+' | b'-')) => {
                    index += 1;
                    *sign == b'-'
                }
                Some(_) => false,
            };
            let exponent_start = index;
            while let Some(&digit) = bytes.get(index).filter(|byte| byte.is_ascii_digit()) {
                explicit_exponent = explicit_exponent
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(i64::from(digit - b'0')))
                    .filter(|value| *value <= MAX_EXPONENT_ABS)
                    .ok_or(NumberError::NumberLimitExceeded)?;
                index += 1;
            }
            if index == exponent_start {
                return Err(NumberError::InvalidNumber);
            }
            if exponent_negative {
                explicit_exponent = -explicit_exponent;
            }
        }
        if index != bytes.len() {
            return Err(NumberError::InvalidNumber);
        }
        let Some(first_nonzero) = raw_digits.iter().position(|digit| *digit != b'0') else {
            return Ok(Self {
                negative: false,
                digits: vec![b'0'],
                exponent: 0,
            });
        };
        let significant_end = raw_digits
            .iter()
            .rposition(|digit| *digit != b'0')
            .map_or(raw_digits.len(), |last| last + 1);
        let removed_trailing = raw_digits.len() - significant_end;
        let exponent = i64::try_from(fraction_digits)
            .ok()
            .and_then(|fraction| explicit_exponent.checked_sub(fraction))
            .zip(i64::try_from(removed_trailing).ok())
            .and_then(|(exponent, trailing)| exponent.checked_add(trailing))
            .filter(|exponent| (-MAX_EXPONENT_ABS..=MAX_EXPONENT_ABS).contains(exponent))
            .ok_or(NumberError::NumberLimitExceeded)?;
        Ok(Self {
            negative,
            digits: raw_digits[first_nonzero..significant_end].to_vec(),
            exponent,
        })
    }

    pub(crate) fn is_negative(&self) -> bool {
        self.negative
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.digits == b"0"
    }

    pub(crate) fn is_integer(&self) -> bool {
        self.is_zero() || self.exponent >= 0
    }

    pub(crate) fn order(&self, other: &Self) -> Ordering {
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Ordering::Equal,
            (true, false) if other.negative => return Ordering::Greater,
            (true, false) => return Ordering::Less,
            (false, true) if self.negative => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        if self.negative != other.negative {
            return if self.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let magnitude = self.magnitude_order(other);
        if self.negative {
            magnitude.reverse()
        } else {
            magnitude
        }
    }

    fn magnitude_order(&self, other: &Self) -> Ordering {
        let places = |number: &Self| {
            i64::try_from(number.digits.len()).unwrap_or(i64::MAX) + number.exponent
        };
        places(self).cmp(&places(other)).then_with(|| {
            let width = self.digits.len().max(other.digits.len());
            let digit = |number: &Self, index: usize| *number.digits.get(index).unwrap_or(&b'0');
            (0..width)
                .map(|index| digit(self, index).cmp(&digit(other, index)))
                .find(|order| order.is_ne())
                .unwrap_or(Ordering::Equal)
        })
    }

    pub(crate) fn is_multiple_of(&self, divisor: &Self) -> Result<bool, NumberError> {
        if divisor.is_zero() {
            return Err(NumberError::InvalidNumber);
        }
        if self.is_zero() {
            return Ok(true);
        }
        let delta = self
            .exponent
            .checked_sub(divisor.exponent)
            .ok_or(NumberError::NumberLimitExceeded)?;
        let zeros =
            usize::try_from(delta.unsigned_abs()).map_err(|_| NumberError::NumberLimitExceeded)?;
        let (numerator_zeros, denominator_zeros) = if delta > 0 { (zeros, 0) } else { (0, zeros) };
        let numerator_len = self.digits.len().checked_add(numerator_zeros);
        let denominator_len = divisor.digits.len().checked_add(denominator_zeros);
        if !numerator_len
            .zip(denominator_len)
            .is_some_and(|(numerator, denominator)| {
                numerator <= MAX_EXPANDED_DIGITS && denominator <= MAX_EXPANDED_DIGITS
            })
        {
            return Err(NumberError::NumberLimitExceeded);
        }
        let denominator = Limbs::from_digits(&expanded(&divisor.digits, denominator_zeros));
        let mut remainder = Limbs::default();
        for digit in expanded(&self.digits, numerator_zeros) {
            remainder.push_digit(digit - b'0');
            while remainder.order(&denominator).is_ge() {
                remainder.subtract(&denominator);
            }
        }
        Ok(remainder.is_zero())
    }

    pub(crate) fn to_non_negative_usize(&self) -> Option<usize> {
        if self.negative || !self.is_integer() {
            return None;
        }
        let mut result: usize = 0;
        for digit in &self.digits {
            result = result
                .checked_mul(10)?
                .checked_add(usize::from(digit - b'0'))?;
        }
        for _ in 0..usize::try_from(self.exponent).ok()? {
            result = result.checked_mul(10)?;
        }
        Some(result)
    }
}

fn expanded(digits: &[u8], zeros: usize) -> Vec<u8> {
    let mut output = digits.to_vec();
    output.resize(digits.len() + zeros, b'0');
    output
}

#[derive(Debug, Default)]
struct Limbs(Vec<u64>);

impl Limbs {
    fn from_digits(digits: &[u8]) -> Self {
        let mut limbs = Self::default();
        for digit in digits {
            limbs.push_digit(digit - b'0');
        }
        limbs
    }

    fn push_digit(&mut self, digit: u8) {
        let mut carry = u64::from(digit);
        for limb in &mut self.0 {
            let value = *limb * 10 + carry;
            *limb = value % LIMB_BASE;
            carry = value / LIMB_BASE;
        }
        if carry > 0 {
            self.0.push(carry);
        }
    }

    fn order(&self, other: &Self) -> Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.iter().rev().cmp(other.0.iter().rev()))
    }

    fn subtract(&mut self, other: &Self) {
        let mut borrow = 0;
        for (index, limb) in self.0.iter_mut().enumerate() {
            let subtrahend = other.0.get(index).copied().unwrap_or(0) + borrow;
            if *limb >= subtrahend {
                *limb -= subtrahend;
                borrow = 0;
            } else {
                *limb += LIMB_BASE - subtrahend;
                borrow = 1;
            }
        }
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }

    fn is_zero(&self) -> bool {
        self.0.is_empty()
    }
}

pub(crate) fn from_value(value: &Value) -> Result<Option<Number>, NumberError> {
    match value {
        Value::Number(number) => Number::parse(&number.to_string()).map(Some),
        _ => Ok(None),
    }
}

pub(crate) fn non_negative_u64(value: &Value) -> Option<u64> {
    let number = from_value(value).ok()??;
    u64::try_from(number.to_non_negative_usize()?).ok()
}

pub(crate) fn ttl_milliseconds(value: &Value) -> Option<u64> {
    let number = from_value(value).ok()??;
    if number.negative {
        return Some(0);
    }
    u64::try_from(number.to_non_negative_usize()?).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(lexeme: &str) -> Number {
        Number::parse(lexeme).unwrap()
    }

    #[test]
    fn exact_json_numbers_normalize_signed_zero_exponents_and_high_precision() {
        assert_eq!(parse("0"), parse("-0.0e+999"));
        assert_eq!(parse("1"), parse("1.000e0"));
        assert_eq!(parse("9007199254740993"), parse("9.007199254740993e15"));
        assert_eq!(
            parse("0.12345678901234567890123456789"),
            parse("12345678901234567890123456789e-29")
        );
        assert_eq!(
            parse("9007199254740992").order(&parse("9007199254740993")),
            Ordering::Less
        );
        assert_eq!(parse("1").order(&parse("1.000e0")), Ordering::Equal);
    }

    #[test]
    fn exact_json_numbers_detect_integers_and_multiple_of_without_floating_point() {
        assert!(parse("1e3").is_integer());
        assert!(!parse("1e-3").is_integer());
        let divisor = parse("0.1");
        assert_eq!(
            parse("0.300000000000000000000000000000").is_multiple_of(&divisor),
            Ok(true)
        );
        assert_eq!(
            parse("0.300000000000000000000000000001").is_multiple_of(&divisor),
            Ok(false)
        );
        assert_eq!(parse("0").is_multiple_of(&divisor), Ok(true));
        assert_eq!(
            parse("123456789012345678901234567890").is_multiple_of(&parse("3")),
            Ok(true)
        );
        assert_eq!(
            parse("123456789012345678901234567891").is_multiple_of(&parse("3")),
            Ok(false)
        );
        assert_eq!(
            parse("1").is_multiple_of(&parse("0")),
            Err(NumberError::InvalidNumber)
        );
        assert_eq!(
            parse("1e9000").is_multiple_of(&parse("1")),
            Err(NumberError::NumberLimitExceeded)
        );
    }

    #[test]
    fn exact_json_numbers_order_signed_magnitudes_around_zero() {
        let zero = parse("0");
        let positive = parse("0.0000000000000000000000000001");
        let negative = parse("-0.0000000000000000000000000001");
        assert_eq!(zero.order(&positive), Ordering::Less);
        assert_eq!(zero.order(&negative), Ordering::Greater);
        assert_eq!(positive.order(&zero), Ordering::Greater);
        assert_eq!(negative.order(&zero), Ordering::Less);
        assert_eq!(parse("-2").order(&parse("-10")), Ordering::Greater);
        assert_eq!(parse("10").order(&parse("9.99")), Ordering::Greater);
    }

    #[test]
    fn exact_json_numbers_detect_integers() {
        assert_eq!(parse("1e3").to_non_negative_usize(), Some(1000));
        assert_eq!(parse("1e-3").to_non_negative_usize(), None);
        assert_eq!(parse("-1").to_non_negative_usize(), None);
        assert_eq!(parse("-0").to_non_negative_usize(), Some(0));
        assert_eq!(parse("1e1000").to_non_negative_usize(), None);
    }

    #[test]
    fn malformed_and_oversized_lexemes_are_refused() {
        for lexeme in ["", "-", "01", "1.", ".5", "1e", "1e+", "+1", "1x", "0x10"] {
            assert_eq!(
                Number::parse(lexeme),
                Err(NumberError::InvalidNumber),
                "{lexeme}"
            );
        }
        assert_eq!(
            Number::parse("1e1000001"),
            Err(NumberError::NumberLimitExceeded)
        );
        assert_eq!(
            Number::parse(&"1".repeat(MAX_LEXEME_BYTES + 1)),
            Err(NumberError::NumberLimitExceeded)
        );
    }

    #[test]
    fn result_integers_accept_integral_values_in_any_notation() {
        assert_eq!(non_negative_u64(&json!(42)), Some(42));
        assert_eq!(non_negative_u64(&json!(42.0)), Some(42));
        assert_eq!(non_negative_u64(&json!(4.2e1)), Some(42));
        assert_eq!(non_negative_u64(&json!(4.25)), None);
        assert_eq!(non_negative_u64(&json!(-1)), None);
        assert_eq!(non_negative_u64(&json!("42")), None);
        assert_eq!(non_negative_u64(&json!(u64::MAX)), Some(u64::MAX));
        assert_eq!(non_negative_u64(&json!(1.844_674_407_370_955_2e19)), None);
    }

    #[test]
    fn negative_cache_lifetimes_expire_immediately() {
        assert_eq!(ttl_milliseconds(&json!(-5)), Some(0));
        assert_eq!(ttl_milliseconds(&json!(-0.5)), Some(0));
        assert_eq!(ttl_milliseconds(&json!(1500.0)), Some(1500));
        assert_eq!(ttl_milliseconds(&json!(1.5)), None);
        assert_eq!(ttl_milliseconds(&json!(null)), None);
    }
}
