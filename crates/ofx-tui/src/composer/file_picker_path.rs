use ofx_text::is_terminal_safe_char;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Complete,
    Incomplete,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Token {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) path_start: usize,
    pub(crate) path_end: usize,
    pub(crate) quote_end: Option<usize>,
    pub(crate) status: Status,
    pub(crate) quoted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Query<'a> {
    pub(crate) prefix: &'a str,
    pub(crate) at_offset: usize,
    pub(crate) token_start: usize,
    pub(crate) replace_end: usize,
    pub(crate) quoted: bool,
}

impl Query<'_> {
    pub(crate) fn decoded(&self) -> Option<String> {
        if self.quoted {
            decode(self.prefix)
        } else {
            Some(self.prefix.to_owned())
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EncodeOptions {
    pub(crate) quoted: bool,
    pub(crate) directory: bool,
    pub(crate) workspace_relative: bool,
}

pub(crate) fn is_terminal_safe(text: &str) -> bool {
    text.chars().all(is_terminal_safe_char)
}

pub(crate) fn is_representable(path: &str) -> bool {
    !path.is_empty() && is_terminal_safe(path)
}

pub(crate) fn is_separator(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn is_start_boundary(byte: u8) -> bool {
    is_separator(byte) || matches!(byte, b'(' | b'[' | b'{' | b'<' | b'\'' | b'"' | b'`')
}

fn is_sentence_punctuation(byte: u8) -> bool {
    matches!(byte, b',' | b'.' | b';' | b':' | b'!' | b'?')
}

fn needs_quotes(path: &str) -> bool {
    path.ends_with('.')
        || path.bytes().any(|byte| {
            byte.is_ascii()
                && !byte.is_ascii_alphanumeric()
                && !matches!(byte, b'_' | b'-' | b'.' | b'/' | b'~')
        })
}

fn token_end(text: &[u8], start: usize) -> usize {
    let mut index = start;
    let mut quote = None;
    while index < text.len() {
        let byte = text[index];
        if byte == b'\\' && index + 1 < text.len() {
            index += 2;
            continue;
        }
        match quote {
            Some(active) if byte == active => quote = None,
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if is_separator(byte) => break,
            _ => {}
        }
        index += 1;
    }
    index
}

pub(crate) fn parse_at(text: &str, start: usize) -> Option<Token> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'@') || (start > 0 && !is_start_boundary(bytes[start - 1])) {
        return None;
    }
    let quoted = bytes.get(start + 1) == Some(&b'"');
    let path_start = start + 1 + usize::from(quoted);
    if !quoted {
        let end = if bytes.get(path_start) == Some(&b'\'') {
            token_end(bytes, path_start)
        } else {
            let mut end = path_start;
            while end < bytes.len() && !is_separator(bytes[end]) {
                end += if bytes[end] == b'\\' && end + 1 < bytes.len() {
                    2
                } else {
                    1
                };
            }
            end.min(bytes.len())
        };
        return Some(Token {
            start,
            end,
            path_start,
            path_end: end,
            quote_end: None,
            status: if end == path_start {
                Status::Incomplete
            } else {
                Status::Complete
            },
            quoted: false,
        });
    }
    let mut index = path_start;
    while index < bytes.len() {
        let byte = bytes[index];
        if matches!(byte, b'\n' | b'\r' | b'\t') {
            break;
        }
        if byte == b'\\' {
            if index + 1 == bytes.len() {
                index = bytes.len();
                break;
            }
            if matches!(bytes[index + 1], b'\n' | b'\r' | b'\t') {
                break;
            }
            index += 2;
            continue;
        }
        if byte == b'"' {
            let quote_end = index + 1;
            let end = token_end(bytes, quote_end);
            let valid = index > path_start
                && is_terminal_safe(&text[path_start..index])
                && bytes[quote_end..end]
                    .iter()
                    .copied()
                    .all(is_sentence_punctuation);
            return Some(Token {
                start,
                end,
                path_start,
                path_end: index,
                quote_end: Some(quote_end),
                status: if valid {
                    Status::Complete
                } else {
                    Status::Invalid
                },
                quoted: true,
            });
        }
        index += 1;
    }
    let index = index.min(bytes.len());
    Some(Token {
        start,
        end: index,
        path_start,
        path_end: index,
        quote_end: None,
        status: Status::Incomplete,
        quoted: true,
    })
}

fn tokens(text: &str) -> impl Iterator<Item = Token> + '_ {
    let mut offset = 0;
    std::iter::from_fn(move || {
        while offset < text.len() {
            if let Some(token) = parse_at(text, offset) {
                offset = token.end.max(offset + 1);
                return Some(token);
            }
            offset += 1;
        }
        None
    })
}

pub(crate) fn decode(payload: &str) -> Option<String> {
    if !is_terminal_safe(payload) {
        return None;
    }
    let bytes = payload.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 1;
            if index == bytes.len() {
                return None;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).ok()
}

pub(crate) fn query_at(text: &str, cursor: usize) -> Option<Query<'_>> {
    if cursor > text.len() {
        return None;
    }
    for token in tokens(text) {
        if cursor < token.path_start {
            return None;
        }
        if cursor > token.path_end {
            continue;
        }
        if token.status == Status::Invalid {
            return None;
        }
        let prefix = text.get(token.path_start..cursor)?;
        if !is_terminal_safe(prefix) || (!token.quoted && prefix.bytes().any(is_separator)) {
            return None;
        }
        return Some(Query {
            prefix,
            at_offset: token.start,
            token_start: token.path_start,
            replace_end: token.quote_end.unwrap_or(cursor),
            quoted: token.quoted,
        });
    }
    None
}

pub(crate) fn encode(path: &str, options: EncodeOptions) -> Option<String> {
    if !is_representable(path) {
        return None;
    }
    let quote = options.quoted || needs_quotes(path);
    let mut encoded = String::with_capacity(path.len() + 6);
    encoded.push('@');
    if quote {
        encoded.push('"');
    }
    if options.workspace_relative && path.starts_with('~') {
        encoded.push_str("./");
    }
    for character in path.chars() {
        if quote && matches!(character, '\\' | '"') {
            encoded.push('\\');
        }
        encoded.push(character);
    }
    if options.directory {
        encoded.push('/');
    } else if quote {
        encoded.push('"');
    }
    Some(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(path: &str) -> String {
        let encoded = encode(path, EncodeOptions::default()).unwrap();
        let token = parse_at(&encoded, 0).unwrap();
        assert_eq!(token.status, Status::Complete, "{encoded}");
        let payload = &encoded[token.path_start..token.path_end];
        if token.quoted {
            decode(payload).unwrap()
        } else {
            payload.to_owned()
        }
    }

    #[test]
    fn the_codec_round_trips_punctuation_quotes_backslashes_and_unicode() {
        for path in [
            "src/main.zig",
            "./photo.png,",
            "./$review.txt",
            "./a\\b.png",
            "./a\"b.png",
            "\"leading",
            " leading.png",
            "tail.png ",
            "caf\u{e9}/\u{6587}.txt",
        ] {
            assert_eq!(round_trip(path), path);
        }
        assert_eq!(
            encode("src/main.zig", EncodeOptions::default()).unwrap(),
            "@src/main.zig"
        );
        assert_eq!(
            encode("dir name/file.txt", EncodeOptions::default()).unwrap(),
            "@\"dir name/file.txt\""
        );
        assert!(!is_representable("bad\nname"));
        assert!(!is_representable("bad\u{1b}name"));
        assert!(!is_representable(""));
    }

    #[test]
    fn queries_separate_the_raw_range_from_the_decoded_prefix() {
        let input = "read @\"./a\\\"b.png\" suffix";
        let cursor = "read @\"./a\\\"b".len();
        let query = query_at(input, cursor).unwrap();
        assert_eq!(query.decoded().unwrap(), "./a\"b");
        assert_eq!(query.replace_end, "read @\"./a\\\"b.png\"".len());
        assert!(query_at(input, query.replace_end).is_none());
        assert_eq!(
            parse_at("@\"photo.png\"other.png", 0).unwrap().status,
            Status::Invalid
        );
        assert_eq!(
            parse_at("@\"photo.png", 0).unwrap().status,
            Status::Incomplete
        );
    }

    #[test]
    fn later_mentions_after_prose_quotes_are_found() {
        let input = "Compare \"@src/main.zig\" and @other";
        assert_eq!(query_at(input, input.len()).unwrap().prefix, "other");
        assert_eq!(query_at("mail me@host", 12), None);
        assert_eq!(query_at("(@src", 5).unwrap().prefix, "src");
        let bare = query_at("look at @sr", 11).unwrap();
        assert_eq!(
            (bare.at_offset, bare.token_start, bare.replace_end),
            (8, 9, 11)
        );
        assert!(query_at("@a b", 4).is_none());
    }

    #[test]
    fn decoding_rejects_dangling_escapes() {
        assert_eq!(decode("\\"), None);
        assert_eq!(decode("a\\\\b"), Some("a\\b".to_owned()));
        assert!(query_at("@\"a\\\"b\"tail", 5).is_none());
    }

    #[test]
    fn directories_keep_their_quote_open_and_workspace_tildes_stay_literal() {
        let directory = EncodeOptions {
            directory: true,
            ..EncodeOptions::default()
        };
        assert_eq!(encode("./dir name", directory).unwrap(), "@\"./dir name/");
        assert_eq!(encode("src", directory).unwrap(), "@src/");
        let relative = EncodeOptions {
            workspace_relative: true,
            ..EncodeOptions::default()
        };
        assert_eq!(encode("~notes", relative).unwrap(), "@./~notes");
        assert_eq!(
            encode("~/notes", EncodeOptions::default()).unwrap(),
            "@~/notes"
        );
    }
}
