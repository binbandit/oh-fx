use std::collections::HashSet;
use std::ops::Range;

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

pub(super) struct Encoded {
    pub(super) text: String,
    members: Option<Vec<Member>>,
}

struct Member {
    key: String,
    value: Range<usize>,
}

impl Encoded {
    pub(super) fn is_object(&self) -> bool {
        self.members.is_some()
    }

    pub(super) fn sole_object_member(&self, key: &str) -> Option<&str> {
        let [member] = self.members.as_deref()? else {
            return None;
        };
        let value = &self.text[member.value.clone()];
        (member.key == key && value.starts_with('{')).then_some(value)
    }
}

pub(super) fn reencode(json: &str) -> Option<Encoded> {
    let mut encoder = Encoder {
        bytes: json.as_bytes(),
        at: 0,
        out: String::with_capacity(json.len()),
    };
    let mut frames = Vec::new();
    let mut members = None;
    let mut member = None;
    'values: loop {
        if let Start::Open(frame) = encoder.value_start()? {
            if frames.is_empty() && matches!(frame, Frame::Object(_)) {
                members = Some(Vec::new());
            }
            if !encoder.close_empty(&frame) {
                frames.push(frame);
                if let Some(Frame::Object(keys)) = frames.last_mut() {
                    let key = encoder.member_key(keys)?;
                    if frames.len() == 1 {
                        member = Some((key, encoder.out.len()));
                    }
                }
                continue;
            }
        }
        loop {
            let depth = frames.len();
            if depth == 1
                && let (Some(members), Some((key, start))) = (members.as_mut(), member.take())
            {
                members.push(Member {
                    key,
                    value: start..encoder.out.len(),
                });
            }
            let Some(frame) = frames.last_mut() else {
                encoder.skip_whitespace();
                return (encoder.at == encoder.bytes.len()).then_some(Encoded {
                    text: encoder.out,
                    members,
                });
            };
            encoder.skip_whitespace();
            match (frame, encoder.next()?) {
                (Frame::Array, b',') => {
                    encoder.out.push(',');
                    continue 'values;
                }
                (Frame::Object(keys), b',') => {
                    encoder.out.push(',');
                    let key = encoder.member_key(keys)?;
                    if depth == 1 {
                        member = Some((key, encoder.out.len()));
                    }
                    continue 'values;
                }
                (Frame::Array, b']') => encoder.out.push(']'),
                (Frame::Object(_), b'}') => encoder.out.push('}'),
                _ => return None,
            }
            frames.pop();
        }
    }
}

enum Frame {
    Array,
    Object(HashSet<String>),
}

enum Start {
    Scalar,
    Open(Frame),
}

struct Encoder<'a> {
    bytes: &'a [u8],
    at: usize,
    out: String,
}

impl Encoder<'_> {
    fn value_start(&mut self) -> Option<Start> {
        self.skip_whitespace();
        match self.next()? {
            b'{' => {
                self.out.push('{');
                return Some(Start::Open(Frame::Object(HashSet::new())));
            }
            b'[' => {
                self.out.push('[');
                return Some(Start::Open(Frame::Array));
            }
            b'"' => {
                let text = self.string()?;
                self.push_string(&text);
            }
            b't' => self.literal("true")?,
            b'f' => self.literal("false")?,
            b'n' => self.literal("null")?,
            first @ (b'-' | b'0'..=b'9') => self.number(first)?,
            _ => return None,
        }
        Some(Start::Scalar)
    }

    fn close_empty(&mut self, frame: &Frame) -> bool {
        self.skip_whitespace();
        let close = match frame {
            Frame::Array => b']',
            Frame::Object(_) => b'}',
        };
        let closed = self.eat(close);
        if closed {
            self.out.push(char::from(close));
        }
        closed
    }

    fn member_key(&mut self, keys: &mut HashSet<String>) -> Option<String> {
        self.skip_whitespace();
        if !self.eat(b'"') {
            return None;
        }
        let key = self.string()?;
        self.skip_whitespace();
        if !self.eat(b':') || !keys.insert(key.clone()) {
            return None;
        }
        self.push_string(&key);
        self.out.push(':');
        Some(key)
    }

    fn literal(&mut self, word: &str) -> Option<()> {
        let rest = &word.as_bytes()[1..];
        if !self.bytes[self.at..].starts_with(rest) {
            return None;
        }
        self.at += rest.len();
        self.out.push_str(word);
        Some(())
    }

    fn number(&mut self, first: u8) -> Option<()> {
        let start = self.at - 1;
        let lead = if first == b'-' { self.next()? } else { first };
        match lead {
            b'0' => {}
            b'1'..=b'9' => self.skip_digits(),
            _ => return None,
        }
        if self.eat(b'.') {
            self.required_digits()?;
        }
        if self.eat(b'e') || self.eat(b'E') {
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            self.required_digits()?;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at]).ok()?;
        let integer_like = text != "-0" && !text.contains(['.', 'e', 'E']);
        match text.parse::<f64>() {
            Ok(float) if !integer_like && float.is_finite() => {
                self.out.push_str(&float.to_string());
            }
            _ => self.out.push_str(text),
        }
        Some(())
    }

    fn string(&mut self) -> Option<String> {
        let mut text = String::new();
        let mut run_start = self.at;
        loop {
            match self.next()? {
                b'"' => {
                    text.push_str(self.text(run_start, self.at - 1)?);
                    return Some(text);
                }
                b'\\' => {
                    text.push_str(self.text(run_start, self.at - 1)?);
                    text.push(self.escape()?);
                    run_start = self.at;
                }
                0..0x20 => return None,
                _ => {}
            }
        }
    }

    fn escape(&mut self) -> Option<char> {
        Some(match self.next()? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => return self.unicode_escape(),
            _ => return None,
        })
    }

    fn unicode_escape(&mut self) -> Option<char> {
        let first = self.hex_code_unit()?;
        if (0xDC00..0xE000).contains(&first) {
            return None;
        }
        if !(0xD800..0xDC00).contains(&first) {
            return char::from_u32(first);
        }
        if !(self.eat(b'\\') && self.eat(b'u')) {
            return None;
        }
        let second = self.hex_code_unit()?;
        if !(0xDC00..0xE000).contains(&second) {
            return None;
        }
        char::from_u32(0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00))
    }

    fn hex_code_unit(&mut self) -> Option<u32> {
        (0..4).try_fold(0, |unit, _| {
            Some(unit * 16 + char::from(self.next()?).to_digit(16)?)
        })
    }

    fn push_string(&mut self, text: &str) {
        self.out.push('"');
        for character in text.chars() {
            match character {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\u{8}' => self.out.push_str("\\b"),
                '\u{c}' => self.out.push_str("\\f"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                _ => match u8::try_from(character) {
                    Ok(control @ 0..0x20) => {
                        self.out.push_str("\\u00");
                        self.out
                            .push(char::from(HEX_DIGITS[usize::from(control >> 4)]));
                        self.out
                            .push(char::from(HEX_DIGITS[usize::from(control & 0xf)]));
                    }
                    _ => self.out.push(character),
                },
            }
        }
        self.out.push('"');
    }

    fn text(&self, start: usize, end: usize) -> Option<&str> {
        std::str::from_utf8(&self.bytes[start..end]).ok()
    }

    fn required_digits(&mut self) -> Option<()> {
        let start = self.at;
        self.skip_digits();
        (self.at > start).then_some(())
    }

    fn skip_digits(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        let matched = self.peek() == Some(byte);
        if matched {
            self.at += 1;
        }
        matched
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.at += 1;
        Some(byte)
    }
}
