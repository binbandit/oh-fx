use std::iter;
use std::ops::Range;
use std::slice;

const DECODE_ROUNDS: usize = 3;
const MAX_DECODED_BYTES: usize = 64 * 1024;

pub(crate) fn mask_configured_secrets(text: String, secrets: &[String]) -> String {
    let secrets: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|secret| !secret.is_empty())
        .collect();
    if secrets.is_empty() {
        return text;
    }
    let mut hidden = vec![false; text.len()];
    for secret in &secrets {
        for (start, _) in text.match_indices(secret) {
            hidden[start..start + secret.len()].fill(true);
        }
    }
    let window = &text.as_bytes()[..text.len().min(MAX_DECODED_BYTES)];
    let mut layer = Layer::decode(window, |index| index..index + 1);
    for round in 1..=DECODE_ROUNDS {
        let Some(current) = layer else {
            break;
        };
        current.hide(&secrets, &mut hidden);
        layer = (round < DECODE_ROUNDS).then(|| current.decoded()).flatten();
    }
    if !hidden.contains(&true) {
        return text;
    }
    let mut masked = String::with_capacity(text.len());
    for (index, character) in text.char_indices() {
        let width = character.len_utf8();
        if hidden[index..index + width].contains(&true) {
            masked.extend(iter::repeat_n('*', width));
        } else {
            masked.push(character);
        }
    }
    masked
}

struct Layer {
    bytes: Vec<u8>,
    sources: Vec<Range<usize>>,
}

impl Layer {
    fn hide(&self, secrets: &[&str], hidden: &mut [bool]) {
        for secret in secrets {
            for start in occurrences(&self.bytes, secret) {
                let first = self.sources[start].start;
                let last = self.sources[start + secret.len() - 1].end;
                hidden[first..last].fill(true);
            }
        }
    }

    fn decoded(&self) -> Option<Self> {
        Self::decode(&self.bytes, |index| self.sources[index].clone())
    }

    fn decode(bytes: &[u8], source_of: impl Fn(usize) -> Range<usize>) -> Option<Self> {
        if !bytes.iter().any(|byte| matches!(byte, b'%' | b'\\')) {
            return None;
        }
        let mut decoded = Self {
            bytes: Vec::with_capacity(bytes.len()),
            sources: Vec::with_capacity(bytes.len()),
        };
        let mut changed = false;
        let mut index = 0;
        while index < bytes.len() {
            let (consumed, unit) = match escape_at(&bytes[index..]) {
                Some(escape) => {
                    changed = true;
                    escape
                }
                None => (1, Unit::Byte(bytes[index])),
            };
            let source = source_of(index).start..source_of(index + consumed - 1).end;
            let mut buffer = [0; 4];
            let output = match &unit {
                Unit::Byte(byte) => slice::from_ref(byte),
                Unit::Char(character) => character.encode_utf8(&mut buffer).as_bytes(),
            };
            for &byte in output {
                decoded.bytes.push(byte);
                decoded.sources.push(source.clone());
            }
            index += consumed;
        }
        changed.then_some(decoded)
    }
}

enum Unit {
    Byte(u8),
    Char(char),
}

fn occurrences<'a>(haystack: &'a [u8], needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    let mut offset = 0;
    haystack.utf8_chunks().flat_map(move |chunk| {
        let start = offset;
        offset += chunk.valid().len() + chunk.invalid().len();
        chunk
            .valid()
            .match_indices(needle)
            .map(move |(index, _)| start + index)
    })
}

fn escape_at(bytes: &[u8]) -> Option<(usize, Unit)> {
    match bytes {
        [b'%', high, low, ..] => Some((3, Unit::Byte((hex_digit(*high)? << 4) | hex_digit(*low)?))),
        [b'\\', b'u', ..] => unicode_escape(bytes),
        [b'\\', escaped, ..] => Some((2, Unit::Byte(json_escape(*escaped)?))),
        _ => None,
    }
}

fn json_escape(byte: u8) -> Option<u8> {
    match byte {
        b'"' | b'\\' | b'/' => Some(byte),
        b'b' => Some(0x08),
        b'f' => Some(0x0c),
        b'n' => Some(b'\n'),
        b'r' => Some(b'\r'),
        b't' => Some(b'\t'),
        _ => None,
    }
}

fn unicode_escape(bytes: &[u8]) -> Option<(usize, Unit)> {
    let unit = code_unit(bytes)?;
    if !(0xd800..0xdc00).contains(&unit) {
        return char::from_u32(unit).map(|character| (6, Unit::Char(character)));
    }
    let low = code_unit(&bytes[6..]).filter(|low| (0xdc00..0xe000).contains(low))?;
    let character = char::from_u32(0x1_0000 + ((unit - 0xd800) << 10) + (low - 0xdc00))?;
    Some((12, Unit::Char(character)))
}

fn code_unit(bytes: &[u8]) -> Option<u32> {
    let [b'\\', b'u', digits @ ..] = bytes else {
        return None;
    };
    digits.get(..4)?.iter().try_fold(0, |unit, &digit| {
        Some((unit << 4) | u32::from(hex_digit(digit)?))
    })
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked(text: &str, secret: &str) -> String {
        mask_configured_secrets(text.to_owned(), &[secret.to_owned()])
    }

    #[test]
    fn secrets_are_masked_in_literal_json_and_percent_encoded_forms() {
        let secret = "sk-a+b/c=\\d\"e";
        for form in [
            "sk-a+b/c=\\d\"e",
            "sk-a+b/c=\\\\d\\\"e",
            "sk-a%2Bb%2Fc%3D%5Cd%22e",
            "sk-a%2bb%2fc%3d%5cd%22e",
        ] {
            assert_eq!(
                masked(&format!("rejected key {form} here"), secret),
                format!("rejected key {} here", "*".repeat(form.len())),
                "{form}"
            );
        }
    }

    #[test]
    fn secrets_are_masked_behind_any_escape_and_layered_encodings() {
        let secret = "pk-live-secret-\u{e9}\u{1f600}";
        for form in [
            "%70%6b%2D%6C%69%76%65-secret-%C3%A9%F0%9F%98%80",
            "\\u0070\\u006B-live-secret-\\u00e9\\ud83d\\ude00",
            "%5Cu0070k-live-secret-\u{e9}\u{1f600}",
            "\\u0025\\u0037\\u0030k-live-secret-%C3%A9\\uD83D\\uDE00",
            "%2570k-live-secret-%25C3%25A9\u{1f600}",
            "%255Cu0070k-live-secret-\u{e9}\u{1f600}",
            "pk-live-secret-\\u00e9%F0%9F%98%80",
        ] {
            assert_eq!(
                masked(&format!("rejected key {form}."), secret),
                format!("rejected key {}.", "*".repeat(form.len())),
                "{form}"
            );
        }
    }

    #[test]
    fn long_text_masks_literal_secrets_throughout_and_decodes_its_first_64_kib() {
        let padding = "x".repeat(MAX_DECODED_BYTES);
        let text = format!("%70k-live-secret {padding} pk-live-secret");
        let expected = format!("{} {padding} {}", "*".repeat(16), "*".repeat(14));
        assert_eq!(masked(&text, "pk-live-secret"), expected);
    }

    #[test]
    fn text_without_a_secret_and_malformed_escapes_are_left_alone() {
        for text in [
            "100%25 done\\n with \\u00e9 and %",
            "%G1 %4 \\u12 \\uD800 \\uDC00\\uD800 \\x \\",
            "pk-live-secre",
            "\u{e9}t\u{e9} %C3",
        ] {
            assert_eq!(masked(text, "pk-live-secret"), text);
        }
        assert_eq!(
            mask_configured_secrets("pk".to_owned(), &[String::new()]),
            "pk"
        );
    }
}
