use std::iter;
use std::ops::Range;
use std::slice;

const MAX_DECODE_ROUNDS: usize = 16;
const MAX_DECODED_BYTES: usize = 64 * 1024;
const MIN_DECODED_NEEDLE_BYTES: usize = 8;

pub(crate) fn mask_configured_secrets(text: String, secrets: &[String]) -> String {
    let needles = needles(secrets);
    if needles.is_empty() {
        return text;
    }
    if text.len() > MAX_DECODED_BYTES && decodes(text.as_bytes()) {
        return withheld(&text);
    }
    let mut hidden = vec![false; text.len()];
    if text.len() <= MAX_DECODED_BYTES && !hide_decoded(text.as_bytes(), &needles, &mut hidden) {
        return withheld(&text);
    }
    for needle in &needles {
        for (start, _) in text.match_indices(needle.as_str()) {
            hidden[start..start + needle.len()].fill(true);
        }
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

fn withheld(text: &str) -> String {
    format!("content withheld ({} bytes)", text.len())
}

fn needles(secrets: &[String]) -> Vec<String> {
    let mut needles = Vec::new();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        let decoded = iter::successors(Some(secret.as_bytes().to_vec()), |bytes| {
            Layer::decode(bytes, |index| index..index + 1).map(|layer| layer.bytes)
        })
        .skip(1)
        .take(MAX_DECODE_ROUNDS)
        .take_while(|bytes| bytes.len() >= MIN_DECODED_NEEDLE_BYTES)
        .filter_map(|bytes| String::from_utf8(bytes).ok());
        for form in iter::once(secret.clone()).chain(decoded) {
            if !needles.contains(&form) {
                needles.push(form);
            }
        }
    }
    needles
}

fn decodes(text: &[u8]) -> bool {
    (0..text.len()).any(|index| escape_at(&text[index..]).is_some())
}

fn hide_decoded(text: &[u8], needles: &[String], hidden: &mut [bool]) -> bool {
    let mut layer = Layer::decode(text, |index| index..index + 1);
    for _ in 1..MAX_DECODE_ROUNDS {
        let Some(current) = layer else {
            return true;
        };
        current.hide(needles, hidden);
        layer = current.decoded();
    }
    layer.is_none_or(|last| {
        last.hide(needles, hidden);
        !decodes(&last.bytes)
    })
}

struct Layer {
    bytes: Vec<u8>,
    sources: Vec<Range<usize>>,
}

impl Layer {
    fn hide(&self, needles: &[String], hidden: &mut [bool]) {
        for needle in needles {
            for start in occurrences(&self.bytes, needle) {
                let first = self.sources[start].start;
                let last = self.sources[start + needle.len() - 1].end;
                hidden[first..last].fill(true);
            }
        }
    }

    fn decoded(&self) -> Option<Self> {
        Self::decode(&self.bytes, |index| self.sources[index].clone())
    }

    fn decode(bytes: &[u8], source_of: impl Fn(usize) -> Range<usize>) -> Option<Self> {
        if !decodes(bytes) {
            return None;
        }
        let mut decoded = Self {
            bytes: Vec::with_capacity(bytes.len()),
            sources: Vec::with_capacity(bytes.len()),
        };
        let mut index = 0;
        while index < bytes.len() {
            let (consumed, unit) =
                escape_at(&bytes[index..]).unwrap_or((1, Unit::Byte(bytes[index])));
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
        Some(decoded)
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
    use std::time::{Duration, Instant};

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
    fn decoding_repeats_until_a_round_changes_nothing() {
        let form = format!("{}u0070k-live-secret", "\\".repeat(8));
        assert_eq!(
            masked(&format!("rejected {form}."), "pk-live-secret"),
            format!("rejected {}.", "*".repeat(form.len()))
        );
    }

    #[test]
    fn secrets_that_contain_escapes_are_found_after_their_own_escapes_decode() {
        for form in [
            "pk-live%41\"secret",
            "pk-live%41\\\"secret",
            "pk-live%2541%22secret",
        ] {
            assert_eq!(
                masked(&format!("rejected key {form}."), "pk-live%41\"secret"),
                format!("rejected key {}.", "*".repeat(form.len())),
                "{form}"
            );
        }
    }

    #[test]
    fn text_that_still_decodes_after_the_last_round_is_withheld_quickly() {
        for text in [
            format!("%{}70k-live-secret", "25".repeat(30_000)),
            format!("\\u005C{}u0070k-live-secret", "u005C".repeat(10_000)),
        ] {
            let started = Instant::now();
            assert_eq!(
                masked(&text, "pk-live-secret"),
                format!("content withheld ({} bytes)", text.len())
            );
            assert!(started.elapsed() < Duration::from_secs(20));
        }
    }

    #[test]
    fn short_decoded_forms_of_a_secret_are_not_matched_on_their_own() {
        assert_eq!(masked("A /// A", "%25252541"), "A /// A");
        assert_eq!(masked("A %25252541", "%25252541"), "A *********");
    }

    #[test]
    fn backslash_runs_that_fit_the_scan_window_settle_within_the_bound() {
        let run = format!("{}x", "\\".repeat(MAX_DECODED_BYTES - 1));
        assert_eq!(masked(&run, "pk-live-secret"), run);
    }

    #[test]
    fn text_longer_than_the_scan_window_is_masked_literally_or_withheld_if_it_decodes() {
        let padding = "x".repeat(MAX_DECODED_BYTES);
        let plain = format!("pk-live-secret {padding} pk-live-secret");
        assert_eq!(
            masked(&plain, "pk-live-secret"),
            format!("{} {padding} {}", "*".repeat(14), "*".repeat(14))
        );
        assert_eq!(masked(&padding, "pk-live-secret"), padding);
        let straddling = format!("{}%70k-live-secret", &padding[8..]);
        assert_eq!(
            masked(&straddling, "pk-live-secret"),
            format!("content withheld ({} bytes)", straddling.len())
        );
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
