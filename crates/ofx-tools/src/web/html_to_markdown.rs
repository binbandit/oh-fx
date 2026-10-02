use ofx_text::is_posix_space;

const TAG_WHITESPACE: &[u8] = b" \t\r\n";
const SPACES: &[u8] = b" \t\n\r\x0b\x0c";
const MAX_TAG_NAME_BYTES: usize = 32;
const ENTITY_SCAN_BYTES: usize = 32;

pub(crate) fn convert(html: &[u8], max_output_bytes: usize) -> Vec<u8> {
    let mut parser = Parser::new(max_output_bytes);
    parser.convert(html);
    parser.out
}

struct Parser {
    out: Vec<u8>,
    title: Vec<u8>,
    link_text: Vec<u8>,
    max_output_bytes: usize,
    suppress_depth: usize,
    title_active: bool,
    pre_depth: usize,
    active_link_href: Option<Vec<u8>>,
    table_cell_open: bool,
    last_was_space: bool,
}

impl Parser {
    fn new(max_output_bytes: usize) -> Self {
        Self {
            out: Vec::new(),
            title: Vec::new(),
            link_text: Vec::new(),
            max_output_bytes,
            suppress_depth: 0,
            title_active: false,
            pre_depth: 0,
            active_link_href: None,
            table_cell_open: false,
            last_was_space: true,
        }
    }

    fn convert(&mut self, html: &[u8]) {
        let mut index = 0;
        while index < html.len() && !self.full() {
            if html[index] != b'<' {
                let next = find_byte(&html[index..], b'<').unwrap_or(html.len() - index);
                self.append_text(&html[index..index + next]);
                index += next;
                continue;
            }
            if html[index..].starts_with(b"<!--") {
                match find(&html[index + 4..], b"-->") {
                    Some(end) => index += 4 + end + 3,
                    None => index = html.len(),
                }
                continue;
            }
            let Some(tag_end) = find_byte(&html[index..], b'>') else {
                self.append_text(&html[index..]);
                break;
            };
            self.handle_tag(&html[index + 1..index + tag_end]);
            index += tag_end + 1;
        }
        self.trim_trailing_whitespace();
        self.prepend_title();
        self.trim_trailing_whitespace();
        if !self.out.is_empty() {
            self.append_bytes(b"\n");
        }
    }

    fn handle_tag(&mut self, raw_tag: &[u8]) {
        let mut tag = trim(raw_tag, TAG_WHITESPACE);
        if tag.is_empty() || tag[0] == b'!' || tag[0] == b'?' {
            return;
        }
        let closing = tag[0] == b'/';
        if closing {
            tag = trim(&tag[1..], TAG_WHITESPACE);
        }
        let self_closing = tag.last() == Some(&b'/');
        if self_closing {
            tag = trim_end(&tag[..tag.len() - 1], TAG_WHITESPACE);
        }
        let name_end = tag
            .iter()
            .position(|byte| !byte.is_ascii_alphanumeric())
            .unwrap_or(tag.len());
        if name_end == 0 {
            return;
        }
        let name = tag[..name_end.min(MAX_TAG_NAME_BYTES)].to_ascii_lowercase();
        let attributes = &tag[name_end..];
        let name = name.as_slice();

        if name == b"title" {
            if closing {
                self.title_active = false;
            } else if !self_closing && self.title.is_empty() {
                self.title_active = true;
            }
            return;
        }
        if is_suppressed_tag(name) {
            if closing {
                self.suppress_depth = self.suppress_depth.saturating_sub(1);
            } else if !self_closing {
                self.suppress_depth += 1;
            }
            return;
        }
        if self.suppress_depth > 0 {
            return;
        }
        if let Some(level) = heading_level(name) {
            self.block_break();
            if !closing {
                for _ in 0..level {
                    self.append_bytes(b"#");
                }
                self.append_bytes(b" ");
            }
            return;
        }
        match name {
            b"p" | b"div" | b"section" | b"article" | b"main" | b"header" | b"footer"
            | b"blockquote" | b"ul" | b"ol" => self.block_break(),
            b"br" => self.soft_break(),
            b"li" => {
                self.block_break();
                if !closing {
                    self.append_bytes(b"- ");
                }
            }
            b"a" => {
                if !closing {
                    if self.active_link_href.is_none() {
                        self.active_link_href = attribute_value(attributes, b"href");
                        self.link_text.clear();
                    }
                } else {
                    self.flush_link();
                }
            }
            b"code" => {
                if self.pre_depth == 0 {
                    self.append_bytes(b"`");
                }
            }
            b"pre" => {
                if closing {
                    self.pre_depth = self.pre_depth.saturating_sub(1);
                    self.trim_trailing_whitespace();
                    self.append_bytes(b"\n```");
                    self.block_break();
                } else {
                    self.block_break();
                    self.append_bytes(b"```\n");
                    self.pre_depth += 1;
                }
            }
            b"tr" => {
                if closing {
                    if self.table_cell_open {
                        self.append_bytes(b" ");
                        self.table_cell_open = false;
                    }
                    self.append_bytes(b"|");
                }
                self.block_break();
            }
            b"th" | b"td" => {
                if !closing {
                    self.append_bytes(b"| ");
                    self.table_cell_open = true;
                } else if self.table_cell_open {
                    self.append_bytes(b" ");
                    self.table_cell_open = false;
                }
            }
            b"img" if !closing => {
                if let Some(alt) = attribute_value(attributes, b"alt") {
                    self.append_text(&alt);
                }
            }
            _ => {}
        }
    }

    fn append_text(&mut self, raw: &[u8]) {
        if self.title_active {
            self.append_title_text(raw);
            return;
        }
        if self.suppress_depth > 0 {
            return;
        }
        let mut index = 0;
        while index < raw.len() && !self.full() {
            if raw[index] == b'&'
                && let Some((decoded, next)) = decode_entity(raw, index)
            {
                self.append_bytes(decoded.as_bytes());
                index = next;
                continue;
            }
            let byte = raw[index];
            index += 1;
            if self.pre_depth == 0 && is_posix_space(byte) {
                self.append_space();
            } else {
                self.append_bytes(&[byte]);
            }
        }
    }

    fn append_title_text(&mut self, raw: &[u8]) {
        let mut index = 0;
        while index < raw.len() && self.title.len() < self.max_output_bytes {
            if raw[index] == b'&'
                && let Some((decoded, next)) = decode_entity(raw, index)
            {
                for byte in decoded.as_bytes() {
                    self.append_title_byte(*byte);
                }
                index = next;
                continue;
            }
            self.append_title_byte(raw[index]);
            index += 1;
        }
    }

    fn append_title_byte(&mut self, byte: u8) {
        if self.title.len() >= self.max_output_bytes {
            return;
        }
        if is_posix_space(byte) {
            if self.title.last().is_some_and(|last| *last != b' ') {
                self.title.push(b' ');
            }
        } else {
            self.title.push(byte);
        }
    }

    fn append_space(&mut self) {
        if !self.last_was_space {
            self.append_bytes(b" ");
        }
    }

    fn block_break(&mut self) {
        self.trim_trailing_inline_whitespace();
        if self.out.is_empty() || self.out.ends_with(b"\n\n") {
            return;
        }
        if self.out.ends_with(b"\n") {
            self.append_bytes(b"\n");
        } else {
            self.append_bytes(b"\n\n");
        }
    }

    fn soft_break(&mut self) {
        self.trim_trailing_inline_whitespace();
        if !self.out.is_empty() && !self.out.ends_with(b"\n") {
            self.append_bytes(b"\n");
        }
    }

    fn flush_link(&mut self) {
        let Some(href) = self.active_link_href.take() else {
            return;
        };
        let text = trim(&self.link_text, SPACES).to_vec();
        self.link_text.clear();
        if text.is_empty() {
            return;
        }
        self.append_bytes(b"[");
        self.append_bytes(&text);
        self.append_bytes(b"](");
        self.append_bytes(&href);
        self.append_bytes(b")");
    }

    fn append_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.full() {
            return;
        }
        let take = bytes.len().min(self.max_output_bytes - self.out.len());
        let taken = &bytes[..take];
        if self.active_link_href.is_some() && self.pre_depth == 0 {
            self.link_text.extend_from_slice(taken);
        } else {
            self.out.extend_from_slice(taken);
        }
        if let Some(last) = taken.last() {
            self.last_was_space = is_posix_space(*last);
        }
    }

    fn full(&self) -> bool {
        self.out.len() >= self.max_output_bytes
    }

    fn trim_trailing_whitespace(&mut self) {
        while self.out.last().copied().is_some_and(is_posix_space) {
            self.out.pop();
        }
        self.last_was_space = true;
    }

    fn trim_trailing_inline_whitespace(&mut self) {
        while matches!(self.out.last(), Some(b' ' | b'\t')) {
            self.out.pop();
        }
        self.last_was_space = self.out.last().copied().is_none_or(is_posix_space);
    }

    fn prepend_title(&mut self) {
        let title = trim(&self.title, SPACES);
        if title.is_empty() || self.max_output_bytes == 0 {
            return;
        }
        let mut merged =
            Vec::with_capacity(self.max_output_bytes.min(title.len() + self.out.len() + 4));
        let limit = self.max_output_bytes;
        let mut append = |bytes: &[u8]| {
            let take = bytes.len().min(limit - merged.len());
            merged.extend_from_slice(&bytes[..take]);
        };
        append(b"# ");
        append(title);
        if !self.out.is_empty() {
            append(b"\n\n");
            append(&self.out);
        }
        self.out = merged;
    }
}

fn decode_entity(raw: &[u8], start: usize) -> Option<(String, usize)> {
    let window = &raw[start..raw.len().min(start + ENTITY_SCAN_BYTES)];
    let end = start + find_byte(window, b';')?;
    let entity = &raw[start + 1..end];
    let decoded = decode_numeric_entity(entity)
        .map(String::from)
        .or_else(|| named_entity(entity).map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(&raw[start..=end]).into_owned());
    Some((decoded, end + 1))
}

fn decode_numeric_entity(entity: &[u8]) -> Option<char> {
    let digits = entity.strip_prefix(b"#")?;
    let (digits, radix) = match digits {
        [b'x' | b'X', hex @ ..] if !hex.is_empty() => (hex, 16),
        [] => return None,
        _ => (digits, 10),
    };
    let value = parse_separated_unsigned(digits, radix)?;
    if value == 0 {
        return None;
    }
    char::from_u32(value)
}

fn parse_separated_unsigned(text: &[u8], radix: u32) -> Option<u32> {
    let digits = text.strip_prefix(b"+").unwrap_or(text);
    if digits.first().is_none_or(|first| *first == b'_') || digits.last() == Some(&b'_') {
        return None;
    }
    digits
        .iter()
        .filter(|byte| **byte != b'_')
        .try_fold(0_u32, |value, byte| {
            let digit = char::from(*byte).to_digit(radix)?;
            value.checked_mul(radix)?.checked_add(digit)
        })
}

fn named_entity(entity: &[u8]) -> Option<&'static str> {
    Some(match entity {
        b"amp" => "&",
        b"lt" => "<",
        b"gt" => ">",
        b"quot" => "\"",
        b"apos" => "'",
        b"nbsp" => " ",
        b"copy" => "\u{a9}",
        b"reg" => "\u{ae}",
        b"trade" => "\u{2122}",
        b"mdash" => "\u{2014}",
        b"ndash" => "\u{2013}",
        b"hellip" => "\u{2026}",
        b"lsquo" => "\u{2018}",
        b"rsquo" => "\u{2019}",
        b"ldquo" => "\u{201c}",
        b"rdquo" => "\u{201d}",
        b"laquo" => "\u{ab}",
        b"raquo" => "\u{bb}",
        b"times" => "\u{d7}",
        b"divide" => "\u{f7}",
        b"bull" => "\u{2022}",
        b"middot" => "\u{b7}",
        b"sect" => "\u{a7}",
        b"para" => "\u{b6}",
        b"deg" => "\u{b0}",
        b"plusmn" => "\u{b1}",
        b"euro" => "\u{20ac}",
        b"pound" => "\u{a3}",
        b"yen" => "\u{a5}",
        b"cent" => "\u{a2}",
        _ => return None,
    })
}

fn attribute_value(attributes: &[u8], wanted: &[u8]) -> Option<Vec<u8>> {
    let mut index = 0;
    while index < attributes.len() {
        while index < attributes.len() && is_posix_space(attributes[index]) {
            index += 1;
        }
        let name_start = index;
        while index < attributes.len()
            && (attributes[index].is_ascii_alphanumeric()
                || matches!(attributes[index], b'-' | b'_'))
        {
            index += 1;
        }
        if index == name_start {
            index += 1;
            continue;
        }
        let name = &attributes[name_start..index];
        while index < attributes.len() && is_posix_space(attributes[index]) {
            index += 1;
        }
        if index >= attributes.len() || attributes[index] != b'=' {
            continue;
        }
        index += 1;
        while index < attributes.len() && is_posix_space(attributes[index]) {
            index += 1;
        }
        if index >= attributes.len() {
            break;
        }
        let value_start;
        if matches!(attributes[index], b'"' | b'\'') {
            let quote = attributes[index];
            index += 1;
            value_start = index;
            while index < attributes.len() && attributes[index] != quote {
                index += 1;
            }
        } else {
            value_start = index;
            while index < attributes.len() && !is_posix_space(attributes[index]) {
                index += 1;
            }
        }
        let value_end = index;
        if index < attributes.len() && matches!(attributes[index], b'"' | b'\'') {
            index += 1;
        }
        if !name.eq_ignore_ascii_case(wanted) {
            continue;
        }
        let mut decoder = Parser::new(value_end - value_start + 16);
        decoder.append_text(&attributes[value_start..value_end]);
        return Some(decoder.out);
    }
    None
}

fn is_suppressed_tag(name: &[u8]) -> bool {
    matches!(
        name,
        b"script" | b"style" | b"head" | b"noscript" | b"template" | b"svg" | b"canvas"
    )
}

fn heading_level(name: &[u8]) -> Option<u8> {
    match name {
        [b'h', level @ b'1'..=b'6'] => Some(level - b'0'),
        _ => None,
    }
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    memchr::memchr(needle, haystack)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(haystack, needle)
}

fn trim<'a>(bytes: &'a [u8], set: &[u8]) -> &'a [u8] {
    let start = bytes
        .iter()
        .position(|byte| !set.contains(byte))
        .unwrap_or(bytes.len());
    trim_end(&bytes[start..], set)
}

fn trim_end<'a>(bytes: &'a [u8], set: &[u8]) -> &'a [u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !set.contains(byte))
        .map_or(0, |last| last + 1);
    &bytes[..end]
}

#[cfg(test)]
mod tests;
