use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolArgValue {
    String(String),
    Integer(i64),
    Bool(bool),
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolArgsError {
    InvalidJson,
    NotObject,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolArgs {
    fields: Vec<(String, ToolArgValue)>,
}

impl ToolArgs {
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().map(|(name, _)| name.as_str())
    }

    pub fn get(&self, key: &str) -> Option<&ToolArgValue> {
        self.fields
            .iter()
            .find_map(|(name, value)| (name == key).then_some(value))
    }

    pub fn optional_string(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            ToolArgValue::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn optional_bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            ToolArgValue::Bool(enabled) => Some(*enabled),
            _ => None,
        }
    }

    pub fn optional_int(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            ToolArgValue::Integer(value) => Some(*value),
            _ => None,
        }
    }
}

pub fn parse_tool_args_object(args_json: &str) -> Result<ToolArgs, ToolArgsError> {
    let mut scanner = Scanner {
        bytes: args_json.as_bytes(),
        at: 0,
    };
    match scanner.document() {
        Some(Document::Object(arguments)) => Ok(arguments),
        Some(Document::Other) => Err(ToolArgsError::NotObject),
        None => Err(ToolArgsError::InvalidJson),
    }
}

enum Document {
    Object(ToolArgs),
    Other,
}

enum Token {
    Value(ToolArgValue),
    Open(Frame),
}

enum Frame {
    Array,
    Object(HashSet<String>),
}

struct Scanner<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scanner<'_> {
    fn document(&mut self) -> Option<Document> {
        let mut frames: Vec<Frame> = Vec::new();
        let mut root: Option<ToolArgs> = None;
        let mut root_key = None;
        'values: loop {
            let mut value = match self.token()? {
                Token::Value(value) => value,
                Token::Open(frame) => {
                    if frames.is_empty() && matches!(frame, Frame::Object(_)) {
                        root = Some(ToolArgs::default());
                    }
                    self.skip_whitespace();
                    let close = match frame {
                        Frame::Array => b']',
                        Frame::Object(_) => b'}',
                    };
                    if self.eat(close) {
                        ToolArgValue::Other
                    } else {
                        frames.push(frame);
                        let depth = frames.len();
                        if let Some(Frame::Object(keys)) = frames.last_mut() {
                            let key = self.member_key(keys)?;
                            if depth == 1 {
                                root_key = Some(key);
                            }
                        }
                        continue;
                    }
                }
            };
            loop {
                let depth = frames.len();
                if depth == 1
                    && let (Some(root), Some(key)) = (root.as_mut(), root_key.take())
                {
                    root.fields.push((key, value));
                }
                let Some(frame) = frames.last_mut() else {
                    self.skip_whitespace();
                    return (self.at == self.bytes.len())
                        .then(|| root.map_or(Document::Other, Document::Object));
                };
                self.skip_whitespace();
                match (frame, self.next()?) {
                    (Frame::Array, b',') => continue 'values,
                    (Frame::Object(keys), b',') => {
                        let key = self.member_key(keys)?;
                        if depth == 1 {
                            root_key = Some(key);
                        }
                        continue 'values;
                    }
                    (Frame::Array, b']') | (Frame::Object(_), b'}') => {
                        frames.pop();
                        value = ToolArgValue::Other;
                    }
                    _ => return None,
                }
            }
        }
    }

    fn token(&mut self) -> Option<Token> {
        self.skip_whitespace();
        let value = match self.next()? {
            b'{' => return Some(Token::Open(Frame::Object(HashSet::new()))),
            b'[' => return Some(Token::Open(Frame::Array)),
            b'"' => ToolArgValue::String(self.string()?),
            b't' => self.literal(b"rue", ToolArgValue::Bool(true))?,
            b'f' => self.literal(b"alse", ToolArgValue::Bool(false))?,
            b'n' => self.literal(b"ull", ToolArgValue::Other)?,
            first @ (b'-' | b'0'..=b'9') => self.number(first)?,
            _ => return None,
        };
        Some(Token::Value(value))
    }

    fn member_key(&mut self, keys: &mut HashSet<String>) -> Option<String> {
        self.skip_whitespace();
        if !self.eat(b'"') {
            return None;
        }
        let key = self.string()?;
        self.skip_whitespace();
        if !self.eat(b':') {
            return None;
        }
        keys.insert(key.clone()).then_some(key)
    }

    fn string(&mut self) -> Option<String> {
        let mut text = String::new();
        let start = self.at;
        let mut run_start = start;
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

    fn text(&self, start: usize, end: usize) -> Option<&str> {
        std::str::from_utf8(&self.bytes[start..end]).ok()
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

    fn number(&mut self, first: u8) -> Option<ToolArgValue> {
        let start = self.at - 1;
        let lead = if first == b'-' { self.next()? } else { first };
        match lead {
            b'0' => {}
            b'1'..=b'9' => self.skip_digits(),
            _ => return None,
        }
        let mut integer = true;
        if self.eat(b'.') {
            integer = false;
            self.required_digits()?;
        }
        if self.eat(b'e') || self.eat(b'E') {
            integer = false;
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            self.required_digits()?;
        }
        let text = self.text(start, self.at)?;
        if !integer || text == "-0" {
            return Some(ToolArgValue::Other);
        }
        Some(
            text.parse()
                .map_or(ToolArgValue::Other, ToolArgValue::Integer),
        )
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

    fn literal(&mut self, rest: &[u8], value: ToolArgValue) -> Option<ToolArgValue> {
        self.bytes[self.at..].starts_with(rest).then(|| {
            self.at += rest.len();
            value
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn field(args_json: &str, key: &str) -> Option<ToolArgValue> {
        parse_tool_args_object(args_json).unwrap().get(key).cloned()
    }

    #[test]
    fn parse_tool_args_object_parses_object_roots() {
        let args = parse_tool_args_object(r#"{"path":"src/main.zig"}"#).unwrap();

        assert_eq!(args.optional_string("path"), Some("src/main.zig"));
    }

    #[test]
    fn parse_tool_args_object_rejects_non_object_roots() {
        for args_json in ["[]", "5", "\"text\"", "null", " true "] {
            assert_eq!(
                parse_tool_args_object(args_json),
                Err(ToolArgsError::NotObject),
                "{args_json}"
            );
        }
    }

    #[test]
    fn optional_typed_args_return_payloads_only_for_matching_tags() {
        let args = parse_tool_args_object(r#"{"name":"fx","enabled":true,"count":3,"other":1.25}"#)
            .unwrap();

        assert_eq!(args.optional_string("name"), Some("fx"));
        assert_eq!(args.optional_string("missing"), None);
        assert_eq!(args.optional_string("enabled"), None);

        assert_eq!(args.optional_bool("enabled"), Some(true));
        assert_eq!(args.optional_bool("missing"), None);
        assert_eq!(args.optional_bool("name"), None);

        assert_eq!(args.optional_int("count"), Some(3));
        assert_eq!(args.optional_int("missing"), None);
        assert_eq!(args.optional_int("other"), None);
    }

    #[test]
    fn numbers_are_integers_only_when_written_as_integers_that_fit_i64() {
        let cases = [
            ("1", ToolArgValue::Integer(1)),
            ("0", ToolArgValue::Integer(0)),
            ("-7", ToolArgValue::Integer(-7)),
            ("9223372036854775807", ToolArgValue::Integer(i64::MAX)),
            ("-9223372036854775808", ToolArgValue::Integer(i64::MIN)),
            ("9223372036854775808", ToolArgValue::Other),
            ("-9223372036854775809", ToolArgValue::Other),
            ("123456789012345678901234567890", ToolArgValue::Other),
            ("-0", ToolArgValue::Other),
            ("1.0", ToolArgValue::Other),
            ("1e2", ToolArgValue::Other),
            ("1E+2", ToolArgValue::Other),
            ("1e400", ToolArgValue::Other),
            ("-1e-400", ToolArgValue::Other),
        ];
        for (number, expected) in cases {
            assert_eq!(
                field(&format!(r#"{{"n":{number}}}"#), "n"),
                Some(expected),
                "{number}"
            );
        }
    }

    #[test]
    fn rejects_repeated_keys_at_any_depth_after_unescaping() {
        for args_json in [
            r#"{"a":1,"a":2}"#,
            r#"{"a":1,"a":2}"#,
            r#"{"outer":{"a":1,"a":2}}"#,
            r#"{"list":[{"a":1,"a":2}]}"#,
        ] {
            assert_eq!(
                parse_tool_args_object(args_json),
                Err(ToolArgsError::InvalidJson),
                "{args_json}"
            );
        }
        assert!(parse_tool_args_object(r#"{"a":{"a":1},"b":[{"a":1},{"a":2}]}"#).is_ok());
    }

    #[test]
    fn rejects_malformed_json() {
        for args_json in [
            "",
            "  ",
            "{",
            "{} x",
            "{}{}",
            r#"{"a":1,}"#,
            r#"{"a" 1}"#,
            "{a:1}",
            r#"{"a":[1,]}"#,
            r#"{"a":01}"#,
            r#"{"a":1.}"#,
            r#"{"a":.5}"#,
            r#"{"a":-}"#,
            r#"{"a":+1}"#,
            r#"{"a":1e}"#,
            r#"{"a":tru}"#,
            r#"{"a":nullx}"#,
            "{\"a\":\"tab\there\"}",
            r#"{"a":"\x"}"#,
            r#"{"a":"\u12"}"#,
            r#"{"a":"\ud800"}"#,
            r#"{"a":"\ud800A"}"#,
            r#"{"a":"\udc00"}"#,
            "\u{feff}{}",
        ] {
            assert_eq!(
                parse_tool_args_object(args_json),
                Err(ToolArgsError::InvalidJson),
                "{args_json:?}"
            );
        }
    }

    #[test]
    fn decodes_string_escapes_and_keeps_only_top_level_fields() {
        let args = parse_tool_args_object(
            "{ \"path\" : \"a\\\"b\\\\c\\/d\\b\\f\\n\\r\\t\\u00e9\\ud83d\\ude00\\u0000\" ,\n\"nested\":{\"path\":\"inner\"},\"list\":[1,{\"x\":[]}],\"flag\":false,\"none\":null}",
        )
        .unwrap();

        assert_eq!(
            args.optional_string("path"),
            Some("a\"b\\c/d\u{8}\u{c}\n\r\t\u{e9}\u{1f600}\0")
        );
        assert_eq!(args.get("nested"), Some(&ToolArgValue::Other));
        assert_eq!(args.get("list"), Some(&ToolArgValue::Other));
        assert_eq!(args.optional_bool("flag"), Some(false));
        assert_eq!(args.get("none"), Some(&ToolArgValue::Other));
        assert_eq!(args.get("x"), None);
    }

    #[test]
    fn accepts_nesting_deeper_than_recursive_parsers_allow() {
        let depth = 100_000;
        let args_json = format!(
            r#"{{"path":"a","deep":{}{}}}"#,
            "[".repeat(depth),
            "]".repeat(depth)
        );

        let args = parse_tool_args_object(&args_json).unwrap();

        assert_eq!(args.optional_string("path"), Some("a"));
        assert_eq!(args.get("deep"), Some(&ToolArgValue::Other));
    }
}
