use std::collections::HashSet;

const WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolArgumentIntegrity {
    Valid,
    MalformedJson,
    NonObjectJson,
}

impl ToolArgumentIntegrity {
    pub fn classify_function_input(serialized: &str) -> Self {
        let integrity = Self::classify_serialized(serialized);
        if integrity == Self::Valid && !serialized.trim_start_matches(WHITESPACE).starts_with('{') {
            return Self::NonObjectJson;
        }
        integrity
    }

    fn classify_serialized(serialized: &str) -> Self {
        match scan(serialized.as_bytes()) {
            Ok(Keys::Unique) => Self::Valid,
            Ok(Keys::Repeated) | Err(_) => Self::MalformedJson,
        }
    }
}

struct Malformed;

enum Keys {
    Unique,
    Repeated,
}

enum Frame {
    Array,
    Object(usize),
}

type Scanned<T> = Result<T, Malformed>;

fn scan(bytes: &[u8]) -> Scanned<Keys> {
    let mut scanner = Scanner {
        bytes,
        at: 0,
        frames: Vec::new(),
        objects: 0,
        keys: HashSet::new(),
        repeated: false,
    };
    scanner.document()?;
    Ok(if scanner.repeated {
        Keys::Repeated
    } else {
        Keys::Unique
    })
}

struct Scanner<'a> {
    bytes: &'a [u8],
    at: usize,
    frames: Vec<Frame>,
    objects: usize,
    keys: HashSet<(usize, Vec<u8>)>,
    repeated: bool,
}

impl Scanner<'_> {
    fn document(&mut self) -> Scanned<()> {
        let mut expects_value = true;
        loop {
            if expects_value && !self.value()? {
                continue;
            }
            self.skip_whitespace();
            let Some(byte) = self.peek() else {
                return if self.frames.is_empty() {
                    Ok(())
                } else {
                    Err(Malformed)
                };
            };
            expects_value = match (byte, self.frames.last()) {
                (b'}', Some(Frame::Object(_))) | (b']', Some(Frame::Array)) => {
                    self.at += 1;
                    self.frames.pop();
                    false
                }
                (b',', Some(Frame::Array)) => {
                    self.at += 1;
                    true
                }
                (b',', Some(Frame::Object(_))) => {
                    self.at += 1;
                    self.expect_significant(b'"')?;
                    self.member()?;
                    true
                }
                _ => return Err(Malformed),
            };
        }
    }

    fn value(&mut self) -> Scanned<bool> {
        match self.significant()? {
            b'{' => {
                self.at += 1;
                self.objects += 1;
                self.frames.push(Frame::Object(self.objects));
                if self.significant()? == b'}' {
                    self.at += 1;
                    self.frames.pop();
                    return Ok(true);
                }
                self.expect_significant(b'"')?;
                self.member()?;
                Ok(false)
            }
            b'[' => {
                self.at += 1;
                self.frames.push(Frame::Array);
                if self.significant()? == b']' {
                    self.at += 1;
                    self.frames.pop();
                    return Ok(true);
                }
                Ok(false)
            }
            b'"' => {
                self.at += 1;
                self.string(None)?;
                Ok(true)
            }
            b'-' | b'0'..=b'9' => {
                self.number()?;
                Ok(true)
            }
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            _ => Err(Malformed),
        }
    }

    fn member(&mut self) -> Scanned<()> {
        self.at += 1;
        let mut key = (!self.repeated).then(Vec::new);
        self.string(key.as_mut())?;
        if let (Some(key), Some(&Frame::Object(object))) = (key, self.frames.last())
            && !self.keys.insert((object, key))
        {
            self.repeated = true;
        }
        self.expect_significant(b':')?;
        self.at += 1;
        Ok(())
    }

    fn string(&mut self, mut decoded: Option<&mut Vec<u8>>) -> Scanned<()> {
        loop {
            let byte = self.byte()?;
            match byte {
                0..0x20 => return Err(Malformed),
                b'"' => {
                    self.at += 1;
                    return Ok(());
                }
                b'\\' => {
                    self.at += 1;
                    let mut buffer = [0; 4];
                    let unescaped = self.escape()?.encode_utf8(&mut buffer);
                    if let Some(decoded) = decoded.as_deref_mut() {
                        decoded.extend_from_slice(unescaped.as_bytes());
                    }
                }
                _ => {
                    self.at += 1;
                    if let Some(decoded) = decoded.as_deref_mut() {
                        decoded.push(byte);
                    }
                }
            }
        }
    }

    fn escape(&mut self) -> Scanned<char> {
        let unescaped = match self.byte()? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                self.at += 1;
                return self.unicode_escape();
            }
            _ => return Err(Malformed),
        };
        self.at += 1;
        Ok(unescaped)
    }

    fn unicode_escape(&mut self) -> Scanned<char> {
        let first = self.hex_digits(4)?;
        if (0xDC00..0xE000).contains(&first) {
            return Err(Malformed);
        }
        if !(0xD800..0xDC00).contains(&first) {
            return char::from_u32(first).ok_or(Malformed);
        }
        self.expect(|byte| byte == b'\\')?;
        self.expect(|byte| byte == b'u')?;
        let high = self.expect(|byte| matches!(byte, b'D' | b'd'))?;
        let second = self.expect(|byte| matches!(byte, b'C'..=b'F' | b'c'..=b'f'))?;
        let rest = self.hex_digits(2)?;
        let low = (hex_value(high) << 12) | (hex_value(second) << 8) | rest;
        char::from_u32(0x10000 + ((first - 0xD800) << 10) + (low - 0xDC00)).ok_or(Malformed)
    }

    fn hex_digits(&mut self, count: usize) -> Scanned<u32> {
        (0..count).try_fold(0, |unit, _| {
            let digit = self.expect(|byte| byte.is_ascii_hexdigit())?;
            Ok((unit << 4) | hex_value(digit))
        })
    }

    fn number(&mut self) -> Scanned<()> {
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        if self.expect(|byte| byte.is_ascii_digit())? != b'0' {
            self.skip_digits();
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            self.expect(|byte| byte.is_ascii_digit())?;
            self.skip_digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.byte()?, b'+' | b'-') {
                self.at += 1;
            }
            self.expect(|byte| byte.is_ascii_digit())?;
            self.skip_digits();
        }
        Ok(())
    }

    fn literal(&mut self, word: &[u8]) -> Scanned<bool> {
        self.at += 1;
        for &expected in &word[1..] {
            self.expect(|byte| byte == expected)?;
        }
        Ok(true)
    }

    fn expect(&mut self, accepts: impl FnOnce(u8) -> bool) -> Scanned<u8> {
        let byte = self.byte()?;
        if !accepts(byte) {
            return Err(Malformed);
        }
        self.at += 1;
        Ok(byte)
    }

    fn expect_significant(&mut self, expected: u8) -> Scanned<()> {
        if self.significant()? == expected {
            Ok(())
        } else {
            Err(Malformed)
        }
    }

    fn significant(&mut self) -> Scanned<u8> {
        self.skip_whitespace();
        self.byte()
    }

    fn byte(&self) -> Scanned<u8> {
        self.peek().ok_or(Malformed)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.at += 1;
        }
    }

    fn skip_digits(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
    }
}

fn hex_value(digit: u8) -> u32 {
    char::from(digit).to_digit(16).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_input_classification_distinguishes_syntax_from_object_shape() {
        let cases = [
            ("{}", ToolArgumentIntegrity::Valid),
            (
                " \n{\"nested\":[1,null,{}]}\t",
                ToolArgumentIntegrity::Valid,
            ),
            ("[]", ToolArgumentIntegrity::NonObjectJson),
            ("42", ToolArgumentIntegrity::NonObjectJson),
            ("null", ToolArgumentIntegrity::NonObjectJson),
            ("true", ToolArgumentIntegrity::NonObjectJson),
            ("\"text\"", ToolArgumentIntegrity::NonObjectJson),
            ("", ToolArgumentIntegrity::MalformedJson),
            ("{} trailing", ToolArgumentIntegrity::MalformedJson),
            ("{\"a\":1,\"a\":2}", ToolArgumentIntegrity::MalformedJson),
        ];
        for (input, expected) in cases {
            assert_eq!(
                ToolArgumentIntegrity::classify_function_input(input),
                expected,
                "{input}"
            );
            if expected == ToolArgumentIntegrity::NonObjectJson {
                assert_eq!(
                    ToolArgumentIntegrity::classify_serialized(input),
                    ToolArgumentIntegrity::Valid,
                    "{input}"
                );
            }
        }
    }

    #[test]
    fn tool_argument_integrity_accepts_complete_serialized_json_roots() {
        for serialized in [
            "  {\"first\":1,\"second\":1e+02} \n",
            "[1,{\"nested\":true}]",
            "null",
            "true",
            "42",
            "\"text\"",
        ] {
            assert_eq!(
                ToolArgumentIntegrity::classify_serialized(serialized),
                ToolArgumentIntegrity::Valid,
                "{serialized}"
            );
        }
    }

    #[test]
    fn tool_argument_integrity_rejects_malformed_trailing_and_duplicate_key_json() {
        for serialized in ["{]", "{} trailing", "{\"depth\":1,\"depth\":2}"] {
            assert_eq!(
                ToolArgumentIntegrity::classify_serialized(serialized),
                ToolArgumentIntegrity::MalformedJson,
                "{serialized}"
            );
        }
    }

    #[test]
    fn objects_the_upstream_parser_accepts_are_valid_whatever_their_numbers_or_depth() {
        let deep = format!("{{\"a\":{}{}}}", "[".repeat(1000), "]".repeat(1000));
        for input in [
            r#"{"limit":1e999,"offset":-1.5E-999}"#,
            r#"{"big":123456789012345678901234567890,"zero":-0}"#,
            r#"{"a":"\u00e9\ud83d\ude00\"\\\/\b\f\n\r\t","b":[true,false,null]}"#,
            r#"{"k":1,"K":1,"k ":1,"":1}"#,
            deep.as_str(),
        ] {
            assert_eq!(
                ToolArgumentIntegrity::classify_function_input(input),
                ToolArgumentIntegrity::Valid,
                "{input}"
            );
        }
    }

    #[test]
    fn repeated_keys_are_compared_after_unescaping_in_every_object() {
        for input in [
            r#"{"a":1,"\u0061":2}"#,
            r#"{"\ud83d\ude00":1,"😀":2}"#,
            r#"{"x":{"y":1,"y":2}}"#,
            r#"[{"a":1,"a":2}]"#,
            r#"{"":1,"":2}"#,
        ] {
            assert_eq!(
                ToolArgumentIntegrity::classify_function_input(input),
                ToolArgumentIntegrity::MalformedJson,
                "{input}"
            );
        }
    }
}
