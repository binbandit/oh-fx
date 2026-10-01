use std::collections::HashSet;

pub(super) fn is_json_document(text: &str) -> bool {
    Scanner { text, index: 0 }.document().is_some()
}

enum Container {
    Array,
    Object(HashSet<String>),
}

struct Scanner<'a> {
    text: &'a str,
    index: usize,
}

impl Scanner<'_> {
    fn document(&mut self) -> Option<()> {
        let mut open: Vec<Container> = Vec::new();
        'value: loop {
            self.skip_whitespace();
            match self.peek()? {
                b'{' => {
                    self.index += 1;
                    self.skip_whitespace();
                    if !self.eat(b'}') {
                        let mut keys = HashSet::new();
                        self.member_key(&mut keys)?;
                        open.push(Container::Object(keys));
                        continue 'value;
                    }
                }
                b'[' => {
                    self.index += 1;
                    self.skip_whitespace();
                    if !self.eat(b']') {
                        open.push(Container::Array);
                        continue 'value;
                    }
                }
                b'"' => {
                    self.string()?;
                }
                b't' => self.literal("true")?,
                b'f' => self.literal("false")?,
                b'n' => self.literal("null")?,
                b'-' | b'0'..=b'9' => self.number()?,
                _ => return None,
            }
            loop {
                self.skip_whitespace();
                let Some(container) = open.last_mut() else {
                    return (self.index == self.text.len()).then_some(());
                };
                let byte = self.peek()?;
                self.index += 1;
                match (byte, container) {
                    (b',', Container::Array) => continue 'value,
                    (b',', Container::Object(keys)) => {
                        self.skip_whitespace();
                        self.member_key(keys)?;
                        continue 'value;
                    }
                    (b']', Container::Array) | (b'}', Container::Object(_)) => {
                        open.pop();
                    }
                    _ => return None,
                }
            }
        }
    }

    fn member_key(&mut self, keys: &mut HashSet<String>) -> Option<()> {
        if self.peek()? != b'"' {
            return None;
        }
        let key = self.string()?;
        if !keys.insert(key) {
            return None;
        }
        self.skip_whitespace();
        self.eat(b':').then_some(())
    }

    fn string(&mut self) -> Option<String> {
        let bytes = self.text.as_bytes();
        self.index += 1;
        let mut decoded = String::new();
        loop {
            match *bytes.get(self.index)? {
                b'"' => {
                    self.index += 1;
                    return Some(decoded);
                }
                b'\\' => {
                    self.index += 1;
                    decoded.push(self.escape()?);
                }
                0x00..=0x1f => return None,
                _ => {
                    let start = self.index;
                    while bytes
                        .get(self.index)
                        .is_some_and(|&byte| byte >= 0x20 && byte != b'"' && byte != b'\\')
                    {
                        self.index += 1;
                    }
                    decoded.push_str(&self.text[start..self.index]);
                }
            }
        }
    }

    fn escape(&mut self) -> Option<char> {
        let escaped = self.peek()?;
        self.index += 1;
        match escaped {
            b'"' => Some('"'),
            b'\\' => Some('\\'),
            b'/' => Some('/'),
            b'b' => Some('\u{8}'),
            b'f' => Some('\u{c}'),
            b'n' => Some('\n'),
            b'r' => Some('\r'),
            b't' => Some('\t'),
            b'u' => self.unicode_escape(),
            _ => None,
        }
    }

    fn unicode_escape(&mut self) -> Option<char> {
        let unit = self.hex_unit()?;
        match unit {
            0xd800..=0xdbff => {
                if !self.eat(b'\\') || !self.eat(b'u') {
                    return None;
                }
                let low = self.hex_unit()?;
                if !(0xdc00..=0xdfff).contains(&low) {
                    return None;
                }
                char::from_u32(0x1_0000 + ((unit - 0xd800) << 10) + (low - 0xdc00))
            }
            0xdc00..=0xdfff => None,
            _ => char::from_u32(unit),
        }
    }

    fn hex_unit(&mut self) -> Option<u32> {
        let digits = self.text.as_bytes().get(self.index..self.index + 4)?;
        let mut unit = 0;
        for &digit in digits {
            unit = unit * 16 + char::from(digit).to_digit(16)?;
        }
        self.index += 4;
        Some(unit)
    }

    fn number(&mut self) -> Option<()> {
        self.eat(b'-');
        match self.peek()? {
            b'0' => self.index += 1,
            b'1'..=b'9' => self.digits(),
            _ => return None,
        }
        if self.eat(b'.') {
            self.required_digits()?;
        }
        if self.eat(b'e') || self.eat(b'E') {
            let _ = self.eat(b'+') || self.eat(b'-');
            self.required_digits()?;
        }
        Some(())
    }

    fn required_digits(&mut self) -> Option<()> {
        if !self.peek()?.is_ascii_digit() {
            return None;
        }
        self.digits();
        Some(())
    }

    fn digits(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.index += 1;
        }
    }

    fn literal(&mut self, word: &str) -> Option<()> {
        if !self.text[self.index..].starts_with(word) {
            return None;
        }
        self.index += word.len();
        Some(())
    }

    fn skip_whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.index += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.index).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        let matched = self.peek() == Some(byte);
        self.index += usize::from(matched);
        matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_numbers_beyond_f64_and_i64_like_zig_std_json() {
        for text in [
            "[1e400]",
            "[-1e400]",
            "[1.0e-400]",
            "[123456789012345678901234567890]",
            "[-0, 0.5, 1E+2, 2e-3, -12.25e10]",
        ] {
            assert!(is_json_document(text), "{text:?}");
        }
    }

    #[test]
    fn nests_without_a_depth_limit() {
        for depth in [127, 128, 129, 200_000] {
            let arrays = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
            assert!(is_json_document(&arrays), "{depth}");
            let objects = format!("{}0{}", "{\"a\":".repeat(depth), "}".repeat(depth));
            assert!(is_json_document(&objects), "{depth}");
        }
    }

    #[test]
    fn rejects_duplicate_keys_after_decoding_escapes() {
        assert!(!is_json_document("{\"a\":1,\"a\":2}"));
        assert!(!is_json_document("{\"a\":1,\"\\u0061\":2}"));
        assert!(!is_json_document("{\"\\ud83d\\ude00\":1,\"😀\":2}"));
        assert!(is_json_document(
            "{\"a\":{\"a\":1},\"b\":[{\"a\":1},{\"a\":2}]}"
        ));
    }

    #[test]
    fn follows_the_rfc_8259_grammar_without_extensions() {
        for text in [
            "{}",
            "[]",
            " [ ] ",
            "{\"\":\"\"}",
            "[true,false,null]",
            "[\"\\\"\\\\\\/\\b\\f\\n\\r\\t\\u00e9\\uD83D\\uDE00\"]",
            "[\"\u{7f}é\"]",
        ] {
            assert!(is_json_document(text), "{text:?}");
        }
        for text in [
            "",
            "[",
            "[1,]",
            "[,1]",
            "[01]",
            "[1.]",
            "[.5]",
            "[+1]",
            "[1e]",
            "[-]",
            "[0x1]",
            "[NaN]",
            "[tru]",
            "[truex]",
            "{a:1}",
            "{\"a\" 1}",
            "{\"a\":1,}",
            "[\"\\x\"]",
            "[\"\\u12\"]",
            "[\"\\ud800\"]",
            "[\"\\ud800\\u0041\"]",
            "[\"\\udc00\"]",
            "[\"tab\tinside\"]",
            "[1] [2]",
            "[1]]",
            "\u{feff}[1]",
            "[1]\u{a0}",
        ] {
            assert!(!is_json_document(text), "{text:?}");
        }
    }
}
