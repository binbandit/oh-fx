use crate::presentation::ansi::{MAX_LINK_URL_BYTES, write_dim};
use crate::presentation::payload::FootnoteSink;
use crate::presentation::text_util::{
    EscapedPunctuation, append_escaped_punctuation, is_ascii_alpha, is_ascii_alpha_numeric,
    is_ascii_whitespace, is_ascii_word_byte, is_trailing_url_punctuation,
};
use crate::presentation::unicode_classes::{is_punctuation_or_symbol, is_whitespace};
use memchr::{memchr, memchr2};
use ofx_text::is_terminal_safe_char;
use std::collections::HashMap;

use crate::styled::{Attr, Slot, SpanWriter};

pub(crate) fn write_inline_no_bold(
    text: &str,
    out: &mut SpanWriter,
    restore_underline_after_link: bool,
    footnotes: Option<&mut FootnoteSink>,
) {
    render_inline(
        text,
        out,
        InlineOptions {
            restore_underline_after_link,
            suppress_bold: true,
        },
        footnotes,
    );
}

pub(crate) fn write_inline(
    text: &str,
    out: &mut SpanWriter,
    restore_underline_after_link: bool,
    footnotes: Option<&mut FootnoteSink>,
) {
    render_inline(
        text,
        out,
        InlineOptions {
            restore_underline_after_link,
            suppress_bold: false,
        },
        footnotes,
    );
}

#[derive(Clone, Copy)]
struct InlineOptions {
    restore_underline_after_link: bool,
    suppress_bold: bool,
}

fn render_inline(
    text: &str,
    out: &mut SpanWriter,
    options: InlineOptions,
    footnotes: Option<&mut FootnoteSink>,
) {
    let mut tokens = tokenize(text, footnotes);
    let matches = match_delimiters(&mut tokens);
    emit_tokens(text, &tokens, &matches, out, options);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EmphasisStyle {
    Bold,
    Italic,
    Strike,
}

impl EmphasisStyle {
    const fn slot(self) -> usize {
        match self {
            Self::Bold => 0,
            Self::Italic => 1,
            Self::Strike => 2,
        }
    }

    const fn attr(self) -> Attr {
        match self {
            Self::Bold => Attr::Bold,
            Self::Italic => Attr::Italic,
            Self::Strike => Attr::Strike,
        }
    }
}

#[derive(Clone, Copy)]
struct Delimiter {
    marker: u8,
    start: usize,
    orig_len: usize,
    remaining: usize,
    can_open: bool,
    can_close: bool,
    open_head: Option<usize>,
    close_head: Option<usize>,
    close_tail: Option<usize>,
}

enum Token<'a> {
    Text(&'a str),
    Entity(char),
    Code(&'a str),
    Link {
        link: InlineLink<'a>,
        visible_prefix: Option<&'static str>,
    },
    Footnote(usize),
    Delimiter(Delimiter),
}

struct Match {
    style: EmphasisStyle,
    next_open: Option<usize>,
    next_close: Option<usize>,
}

fn tokenize<'a>(text: &'a str, footnotes: Option<&mut FootnoteSink>) -> Vec<Token<'a>> {
    let mut tokenizer = Tokenizer {
        text,
        tokens: Vec::new(),
        index: 0,
        literal_start: 0,
        link_admission_suppressed_until: 0,
        last_close_bracket: text.as_bytes().iter().rposition(|&byte| byte == b']'),
        next_close_bracket: None,
        opening_delimiter_end: 0,
        escapes: EscapedPunctuation::default(),
        bare_url_end: ForwardSearch::new(|bytes| {
            bytes.iter().position(|&byte| is_bare_url_terminator(byte))
        }),
        label_end: ForwardSearch::new(|bytes| memchr2(b']', b'\n', bytes)),
        destination_end: ForwardSearch::new(|bytes| memchr2(b')', b'\n', bytes)),
        angle_end: ForwardSearch::new(|bytes| memchr2(b'>', b'\n', bytes)),
        backtick_runs: None,
        footnotes,
    };
    tokenizer.run();
    tokenizer.tokens
}

struct ForwardSearch {
    search: fn(&[u8]) -> Option<usize>,
    from: usize,
    found: usize,
}

impl ForwardSearch {
    fn new(search: fn(&[u8]) -> Option<usize>) -> Self {
        Self {
            search,
            from: usize::MAX,
            found: 0,
        }
    }

    fn find(&mut self, bytes: &[u8], from: usize) -> usize {
        if from < self.from || from > self.found {
            let rest = bytes.get(from..).unwrap_or_default();
            self.from = from;
            self.found = from + (self.search)(rest).unwrap_or(rest.len());
        }
        self.found
    }
}

struct Tokenizer<'a, 'f> {
    text: &'a str,
    tokens: Vec<Token<'a>>,
    index: usize,
    literal_start: usize,
    link_admission_suppressed_until: usize,
    last_close_bracket: Option<usize>,
    next_close_bracket: Option<usize>,
    opening_delimiter_end: usize,
    escapes: EscapedPunctuation,
    bare_url_end: ForwardSearch,
    label_end: ForwardSearch,
    destination_end: ForwardSearch,
    angle_end: ForwardSearch,
    backtick_runs: Option<BacktickRuns>,
    footnotes: Option<&'f mut FootnoteSink>,
}

impl<'a> Tokenizer<'a, '_> {
    fn run(&mut self) {
        let bytes = self.text.as_bytes();
        while self.index < bytes.len() {
            let byte = bytes[self.index];
            let after_opening_delimiter =
                self.index > 0 && self.opening_delimiter_end == self.index;
            self.refresh_next_close_bracket();
            let bracket_link_possible = self.next_close_bracket.is_some_and(|close| {
                close >= self.index && close + 1 < bytes.len() && bytes[close + 1] == b'('
            });
            let link_admitted = self.index >= self.link_admission_suppressed_until;

            let consumed =
                match byte {
                    b'`' => {
                        self.code_span();
                        true
                    }
                    b'&' => self.entity(),
                    b'\\' if self.is_escape() => {
                        self.escape(bracket_link_possible);
                        true
                    }
                    _ => false,
                } || (bracket_link_possible && link_admitted && byte == b'!' && self.image())
                    || (byte == b'[' && self.footnote())
                    || (bracket_link_possible && link_admitted && byte == b'[' && self.link())
                    || (link_admitted && byte == b'<' && self.angle_autolink())
                    || (link_admitted && self.bare_url(after_opening_delimiter));
            if consumed {
                continue;
            }
            if matches!(byte, b'*' | b'_' | b'~') {
                self.delimiter_run();
                continue;
            }
            self.index += 1;
        }
        self.flush_literal(bytes.len());
    }

    fn refresh_next_close_bracket(&mut self) {
        let index = self.index;
        if self.last_close_bracket.is_some_and(|last| index <= last)
            && self.next_close_bracket.is_none_or(|close| close < index)
        {
            self.next_close_bracket = find_byte(self.text.as_bytes(), index, b']');
        }
    }

    fn flush_literal(&mut self, end: usize) {
        if end > self.literal_start {
            self.tokens
                .push(Token::Text(&self.text[self.literal_start..end]));
        }
    }

    fn push_token(&mut self, token: Token<'a>, end: usize) {
        self.flush_literal(self.index);
        self.tokens.push(token);
        self.index = end;
        self.literal_start = end;
    }

    fn suppress_links_until(&mut self, end: usize) {
        self.link_admission_suppressed_until = self.link_admission_suppressed_until.max(end);
    }

    fn code_span(&mut self) {
        let bytes = self.text.as_bytes();
        let run = backtick_run_length(bytes, self.index);
        let closer = self
            .backtick_runs
            .get_or_insert_with(|| BacktickRuns::new(bytes))
            .closer(self.index + run, run);
        if let Some(closer) = closer {
            let span = code_span(bytes, self.index, run, closer);
            let text = self.text;
            self.push_token(
                Token::Code(&text[span.content_start..span.content_end]),
                span.end,
            );
            return;
        }
        self.index += run;
    }

    fn entity(&mut self) -> bool {
        let Some(entity) = decode_entity(self.text.as_bytes(), self.index) else {
            return false;
        };
        self.push_token(Token::Entity(entity.codepoint), entity.end);
        true
    }

    fn is_escape(&mut self) -> bool {
        let bytes = self.text.as_bytes();
        self.index + 1 < bytes.len() && self.escapes.at(bytes, self.index + 1)
    }

    fn angle_candidate_end(&mut self, start: usize) -> usize {
        let bytes = self.text.as_bytes();
        if bytes.get(start) != Some(&b'<') {
            return start;
        }
        let end = self.angle_end.find(bytes, start + 1);
        if bytes.get(end) == Some(&b'>') {
            end + 1
        } else {
            end
        }
    }

    fn malformed_link_candidate_end(&mut self, start: usize) -> Option<usize> {
        let bytes = self.text.as_bytes();
        if bytes.get(start) != Some(&b'[') {
            return None;
        }
        let label_end = self.label_end.find(bytes, start + 1);
        if bytes.get(label_end) != Some(&b']') || bytes.get(label_end + 1) != Some(&b'(') {
            return None;
        }
        let candidate_end = self.destination_end.find(bytes, label_end + 2);
        if bytes.get(candidate_end) == Some(&b')') {
            Some(candidate_end + 1)
        } else {
            Some(candidate_end)
        }
    }

    fn escape(&mut self, bracket_link_possible: bool) {
        let text = self.text;
        let bytes = text.as_bytes();
        let index = self.index;
        self.flush_literal(index);
        let escaped = bytes[index + 1];
        if escaped == b'<' {
            let end = self.angle_candidate_end(index + 1);
            self.suppress_links_until(end);
        }
        if bracket_link_possible
            && escaped == b'!'
            && bytes.get(index + 2) == Some(&b'[')
            && let Some(candidate_end) = self.malformed_link_candidate_end(index + 2)
        {
            self.tokens
                .push(Token::Text(&text[index + 1..candidate_end]));
            self.suppress_links_until(candidate_end);
            self.index = candidate_end;
            self.literal_start = candidate_end;
            return;
        }
        if bracket_link_possible
            && escaped == b'['
            && let Some(candidate_end) = self.malformed_link_candidate_end(index + 1)
        {
            self.suppress_links_until(candidate_end);
        }
        self.tokens.push(Token::Text(&text[index + 1..index + 2]));
        self.index = index + 2;
        self.literal_start = self.index;
    }

    fn image(&mut self) -> bool {
        let bytes = self.text.as_bytes();
        if bytes.get(self.index + 1) != Some(&b'[') {
            return false;
        }
        if let Some(image) = parse_inline_image(self.text, self.index) {
            let end = image.end;
            self.push_token(
                Token::Link {
                    link: image,
                    visible_prefix: Some("▧ "),
                },
                end,
            );
            return true;
        }
        if let Some(candidate_end) = self.malformed_link_candidate_end(self.index + 1) {
            self.suppress_links_until(candidate_end);
        }
        false
    }

    fn footnote(&mut self) -> bool {
        let Some(close) = self.next_close_bracket else {
            return false;
        };
        let Some(end) = footnote_reference_end(self.text.as_bytes(), self.index, close) else {
            return false;
        };
        let Some(sink) = self.footnotes.as_deref_mut() else {
            return false;
        };
        let number = sink.register(&self.text[self.index + 2..close]);
        self.push_token(Token::Footnote(number), end);
        true
    }

    fn link(&mut self) -> bool {
        if let Some(link) = parse_inline_link(self.text, self.index) {
            let end = link.end;
            self.push_token(
                Token::Link {
                    link,
                    visible_prefix: None,
                },
                end,
            );
            return true;
        }
        if let Some(candidate_end) = self.malformed_link_candidate_end(self.index) {
            self.suppress_links_until(candidate_end);
        }
        false
    }

    fn angle_autolink(&mut self) -> bool {
        if let Some(link) = parse_angle_autolink(self.text, self.index) {
            let end = link.end;
            self.push_token(
                Token::Link {
                    link,
                    visible_prefix: None,
                },
                end,
            );
            return true;
        }
        let end = self.angle_candidate_end(self.index);
        self.suppress_links_until(end);
        false
    }

    fn bare_url(&mut self, after_opening_delimiter: bool) -> bool {
        let text = self.text;
        if !is_bare_url_boundary(text.as_bytes(), self.index, after_opening_delimiter) {
            return false;
        }
        let Some(scheme_len) = text.get(self.index..).and_then(url_scheme_len) else {
            return false;
        };
        let candidate_end = self
            .bare_url_end
            .find(text.as_bytes(), self.index + scheme_len);
        let Some(link) = parse_bare_url(text, self.index, scheme_len, candidate_end) else {
            return false;
        };
        let end = link.end;
        self.push_token(
            Token::Link {
                link,
                visible_prefix: None,
            },
            end,
        );
        true
    }

    fn delimiter_run(&mut self) {
        let run_end = marker_run_end(self.text.as_bytes(), self.index);
        if let Some(delimiter) = delimiter_at(self.text, self.index, run_end) {
            self.push_token(Token::Delimiter(delimiter), run_end);
            if delimiter.can_open {
                self.opening_delimiter_end = run_end;
            }
        }
        self.index = run_end;
    }
}

fn find_byte(bytes: &[u8], start: usize, needle: u8) -> Option<usize> {
    bytes
        .get(start..)?
        .iter()
        .position(|&byte| byte == needle)
        .map(|offset| start + offset)
}

fn marker_run_end(bytes: &[u8], start: usize) -> usize {
    let marker = bytes[start];
    start
        + bytes[start..]
            .iter()
            .take_while(|&&byte| byte == marker)
            .count()
}

fn delimiter_at(text: &str, start: usize, end: usize) -> Option<Delimiter> {
    let marker = text.as_bytes()[start];
    let len = end - start;
    if marker == b'~' && len != 2 {
        return None;
    }

    let before = text[..start].chars().next_back().unwrap_or(' ');
    let after = text[end..].chars().next().unwrap_or(' ');
    let before_space = is_whitespace(before);
    let after_space = is_whitespace(after);
    let before_punct = is_punctuation_or_symbol(before);
    let after_punct = is_punctuation_or_symbol(after);

    let left_flanking = !after_space && (!after_punct || before_space || before_punct);
    let right_flanking = !before_space && (!before_punct || after_space || after_punct);

    let (can_open, can_close) = if marker == b'_' {
        (
            left_flanking && (!right_flanking || before_punct),
            right_flanking && (!left_flanking || after_punct),
        )
    } else {
        (left_flanking, right_flanking)
    };
    if !can_open && !can_close {
        return None;
    }
    Some(Delimiter {
        marker,
        start,
        orig_len: len,
        remaining: len,
        can_open,
        can_close,
        open_head: None,
        close_head: None,
        close_tail: None,
    })
}

#[derive(Clone, Copy)]
struct StackEntry {
    token: usize,
    seq: u32,
}

fn match_delimiters(tokens: &mut [Token<'_>]) -> Vec<Match> {
    let mut matches: Vec<Match> = Vec::new();
    let mut stack: Vec<StackEntry> = Vec::new();
    let mut next_seq: u32 = 1;
    let mut openers_bottom = [[[0_u32; 3]; 2]; 3];

    for token_index in 0..tokens.len() {
        let Token::Delimiter(mut closer) = tokens[token_index] else {
            continue;
        };

        if closer.can_close {
            let bottom = &mut openers_bottom[marker_slot(closer.marker)]
                [usize::from(closer.can_open)][closer.orig_len % 3];
            let mut matched_in_search = false;
            let mut depth = stack.len();
            while closer.remaining > 0 && depth > 0 {
                depth -= 1;
                let entry = stack[depth];
                if entry.seq <= *bottom {
                    break;
                }
                let Token::Delimiter(opener) = &mut tokens[entry.token] else {
                    continue;
                };
                if opener.marker != closer.marker {
                    continue;
                }
                if opener.marker != b'~' && violates_rule_of_three(opener, &closer) {
                    continue;
                }

                let use_len =
                    if opener.marker == b'~' || (opener.remaining >= 2 && closer.remaining >= 2) {
                        2
                    } else {
                        1
                    };
                let style = match opener.marker {
                    b'~' => EmphasisStyle::Strike,
                    _ if use_len == 2 => EmphasisStyle::Bold,
                    _ => EmphasisStyle::Italic,
                };
                let match_index = matches.len();
                matches.push(Match {
                    style,
                    next_open: opener.open_head,
                    next_close: None,
                });
                opener.open_head = Some(match_index);
                if let Some(tail) = closer.close_tail {
                    matches[tail].next_close = Some(match_index);
                } else {
                    closer.close_head = Some(match_index);
                }
                closer.close_tail = Some(match_index);
                opener.remaining -= use_len;
                closer.remaining -= use_len;

                let keep = if opener.remaining == 0 {
                    depth
                } else {
                    depth + 1
                };
                stack.truncate(keep);
                depth = stack.len();
                matched_in_search = closer.remaining == 0;
            }
            if !matched_in_search && let Some(top) = stack.last() {
                *bottom = top.seq;
            }
        }

        if closer.can_open && closer.remaining > 0 {
            stack.push(StackEntry {
                token: token_index,
                seq: next_seq,
            });
            next_seq += 1;
        }
        tokens[token_index] = Token::Delimiter(closer);
    }
    matches
}

fn marker_slot(marker: u8) -> usize {
    match marker {
        b'*' => 0,
        b'_' => 1,
        _ => 2,
    }
}

fn violates_rule_of_three(opener: &Delimiter, closer: &Delimiter) -> bool {
    if !opener.can_close && !closer.can_open {
        return false;
    }
    let sum = opener.orig_len + closer.orig_len;
    if !sum.is_multiple_of(3) {
        return false;
    }
    !opener.orig_len.is_multiple_of(3) || !closer.orig_len.is_multiple_of(3)
}

fn emit_tokens(
    text: &str,
    tokens: &[Token<'_>],
    matches: &[Match],
    out: &mut SpanWriter,
    options: InlineOptions,
) {
    let mut depth = [0_usize; 3];
    for token in tokens {
        match token {
            Token::Text(slice) => out.text(slice),
            Token::Entity(codepoint) => out.char(*codepoint),
            Token::Code(content) => emit_code_span(out, content),
            Token::Link {
                link,
                visible_prefix,
            } => emit_inline_link(
                out,
                link,
                options.restore_underline_after_link,
                *visible_prefix,
            ),
            Token::Footnote(number) => write_footnote_marker(out, *number),
            Token::Delimiter(delimiter) => {
                let mut close = delimiter.close_head;
                while let Some(index) = close {
                    emit_style(out, matches[index].style, false, &mut depth, options);
                    close = matches[index].next_close;
                }
                out.text(&text[delimiter.start..delimiter.start + delimiter.remaining]);
                let mut open = delimiter.open_head;
                while let Some(index) = open {
                    emit_style(out, matches[index].style, true, &mut depth, options);
                    open = matches[index].next_open;
                }
            }
        }
    }
}

fn emit_code_span(out: &mut SpanWriter, content: &str) {
    out.open_slot(Slot::InlineCode);
    if let Some(url_end) = code_span_url_end(content) {
        out.open_link(content[..url_end].to_owned());
        out.text(&content[..url_end]);
        out.close_link();
        out.text(&content[url_end..]);
    } else {
        out.text(content);
    }
    out.close_slot();
}

fn emit_style(
    out: &mut SpanWriter,
    style: EmphasisStyle,
    opening: bool,
    depth: &mut [usize; 3],
    options: InlineOptions,
) {
    let slot = style.slot();
    let visible = !(style == EmphasisStyle::Bold && options.suppress_bold);
    if opening {
        depth[slot] += 1;
        if visible {
            out.open(style.attr());
        }
        return;
    }
    depth[slot] -= 1;
    if !visible {
        return;
    }
    out.close(style.attr());
    if depth[slot] > 0 {
        out.open(style.attr());
    }
}

fn footnote_reference_end(bytes: &[u8], start: usize, close: usize) -> Option<usize> {
    if start + 4 > bytes.len() || bytes[start] != b'[' || bytes[start + 1] != b'^' {
        return None;
    }
    if close <= start + 2 || close >= bytes.len() || bytes[close] != b']' {
        return None;
    }
    if bytes.get(close + 1) == Some(&b':') {
        return None;
    }
    Some(close + 1)
}

fn write_footnote_marker(out: &mut SpanWriter, number: usize) {
    write_dim(out, &format!("[{number}]"));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LabelMode {
    Escaped,
    Literal,
}

struct InlineLink<'a> {
    text: &'a str,
    url: &'a str,
    end: usize,
    destination_prefix: &'static str,
    label_mode: LabelMode,
}

fn emit_inline_link(
    out: &mut SpanWriter,
    link: &InlineLink<'_>,
    restore_underline_after_link: bool,
    visible_prefix: Option<&str>,
) {
    out.open_link(format!("{}{}", link.destination_prefix, link.url));
    out.open_slot(Slot::Link);
    out.open(Attr::Underline);
    if let Some(prefix) = visible_prefix {
        out.text(prefix);
    }
    let visible_text = if link.text.is_empty() && visible_prefix.is_some() {
        "image"
    } else {
        link.text
    };
    match link.label_mode {
        LabelMode::Escaped => {
            let mut unescaped = String::with_capacity(visible_text.len());
            append_escaped_punctuation(&mut unescaped, visible_text);
            out.text(&unescaped);
        }
        LabelMode::Literal => out.text(visible_text),
    }
    out.close(Attr::Underline);
    out.close_slot();
    out.close_link();
    if restore_underline_after_link {
        out.open(Attr::Underline);
    }
}

fn parse_inline_link(text: &str, start: usize) -> Option<InlineLink<'_>> {
    parse_inline_bracket_destination(text, start, false)
}

fn parse_inline_image(text: &str, start: usize) -> Option<InlineLink<'_>> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'!') || bytes.get(start + 1) != Some(&b'[') {
        return None;
    }
    parse_inline_bracket_destination(text, start + 1, true)
}

fn parse_inline_bracket_destination(
    text: &str,
    start: usize,
    allow_empty_text: bool,
) -> Option<InlineLink<'_>> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let text_end = start
        + 1
        + bytes[start + 1..]
            .iter()
            .take_while(|&&byte| byte != b']' && byte != b'\n')
            .count();
    if bytes.get(text_end) != Some(&b']') {
        return None;
    }
    if !allow_empty_text && text_end == start + 1 {
        return None;
    }
    if bytes.get(text_end + 1) != Some(&b'(') {
        return None;
    }
    let destination = parse_link_destination(text, text_end + 2)?;
    if !is_valid_link_url(destination.url) {
        return None;
    }
    Some(InlineLink {
        text: &text[start + 1..text_end],
        url: destination.url,
        end: destination.end,
        destination_prefix: "",
        label_mode: LabelMode::Escaped,
    })
}

struct LinkDestination<'a> {
    url: &'a str,
    end: usize,
}

fn parse_link_destination(text: &str, start: usize) -> Option<LinkDestination<'_>> {
    let bytes = text.as_bytes();
    let mut cursor = skip_inline_spaces(bytes, start);
    if cursor >= bytes.len() {
        return None;
    }

    let url;
    if bytes[cursor] == b'<' {
        let close = find_byte(bytes, cursor + 1, b'>')?;
        url = &text[cursor + 1..close];
        if url.bytes().any(|byte| byte == b'<' || byte == b'\n') {
            return None;
        }
        cursor = close + 1;
    } else {
        let url_start = cursor;
        let mut depth: usize = 0;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if byte == b'\\' && cursor + 1 < bytes.len() {
                cursor += 2;
                continue;
            }
            if byte <= b' ' || byte == 0x7f {
                break;
            }
            if byte == b'(' {
                depth += 1;
            } else if byte == b')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            cursor += 1;
        }
        if depth != 0 || cursor == url_start {
            return None;
        }
        url = &text[url_start..cursor];
    }

    let after_url = cursor;
    cursor = skip_inline_spaces(bytes, cursor);
    if cursor > after_url && cursor < bytes.len() && bytes[cursor] != b')' {
        cursor = link_title_end(bytes, cursor)?;
        cursor = skip_inline_spaces(bytes, cursor);
    }
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    Some(LinkDestination {
        url,
        end: cursor + 1,
    })
}

fn skip_inline_spaces(bytes: &[u8], start: usize) -> usize {
    start
        + bytes
            .get(start..)
            .unwrap_or_default()
            .iter()
            .take_while(|&&byte| byte == b' ' || byte == b'\t')
            .count()
}

fn link_title_end(bytes: &[u8], start: usize) -> Option<usize> {
    let closer = match *bytes.get(start)? {
        b'"' => b'"',
        b'\'' => b'\'',
        b'(' => b')',
        _ => return None,
    };
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' && cursor + 1 < bytes.len() {
            cursor += 2;
            continue;
        }
        if bytes[cursor] == b'\n' {
            return None;
        }
        if bytes[cursor] == closer {
            return Some(cursor + 1);
        }
        cursor += 1;
    }
    None
}

struct CodeSpan {
    content_start: usize,
    content_end: usize,
    end: usize,
}

fn backtick_run_length(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .take_while(|&&byte| byte == b'`')
        .count()
}

struct BacktickRuns {
    starts_by_length: HashMap<usize, Vec<usize>>,
}

impl BacktickRuns {
    fn new(bytes: &[u8]) -> Self {
        let mut starts_by_length: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut cursor = 0;
        while let Some(offset) = memchr(b'`', &bytes[cursor..]) {
            let start = cursor + offset;
            let length = backtick_run_length(bytes, start);
            starts_by_length.entry(length).or_default().push(start);
            cursor = start + length;
        }
        Self { starts_by_length }
    }

    fn closer(&self, from: usize, length: usize) -> Option<usize> {
        let starts = self.starts_by_length.get(&length)?;
        starts
            .get(starts.partition_point(|&start| start < from))
            .copied()
    }
}

fn code_span(bytes: &[u8], start: usize, run: usize, closer: usize) -> CodeSpan {
    let mut content_start = start + run;
    let mut content_end = closer;
    let content = &bytes[content_start..content_end];
    if content.len() >= 2
        && content[0] == b' '
        && content[content.len() - 1] == b' '
        && content.iter().any(|&byte| byte != b' ')
    {
        content_start += 1;
        content_end -= 1;
    }
    CodeSpan {
        content_start,
        content_end,
        end: closer + run,
    }
}

struct DecodedEntity {
    codepoint: char,
    end: usize,
}

const MAX_ENTITY_NAME_LEN: usize = 8;

const NAMED_ENTITIES: [(&str, char); 13] = [
    ("amp", '&'),
    ("lt", '<'),
    ("gt", '>'),
    ("quot", '"'),
    ("apos", '\''),
    ("nbsp", '\u{a0}'),
    ("copy", '\u{a9}'),
    ("reg", '\u{ae}'),
    ("hellip", '\u{2026}'),
    ("mdash", '\u{2014}'),
    ("ndash", '\u{2013}'),
    ("larr", '\u{2190}'),
    ("rarr", '\u{2192}'),
];

fn decode_entity(bytes: &[u8], start: usize) -> Option<DecodedEntity> {
    if bytes.get(start) != Some(&b'&') {
        return None;
    }
    let window_end = bytes.len().min(start + 1 + MAX_ENTITY_NAME_LEN + 1);
    let semicolon = start
        + 1
        + bytes[start + 1..window_end]
            .iter()
            .position(|&byte| byte == b';')?;
    let name = &bytes[start + 1..semicolon];
    if name.is_empty() {
        return None;
    }

    let codepoint = if name[0] == b'#' {
        let hex = name.len() > 1 && (name[1] == b'x' || name[1] == b'X');
        let digits = if hex { &name[2..] } else { &name[1..] };
        if digits.is_empty() {
            return None;
        }
        let value = parse_reference_number(digits, if hex { 16 } else { 10 })?;
        let codepoint = match value {
            0 | 0xd800..=0xdfff => '\u{fffd}',
            _ => char::from_u32(value)?,
        };
        if !is_terminal_safe_char(codepoint) {
            return None;
        }
        codepoint
    } else {
        NAMED_ENTITIES
            .iter()
            .find(|(entity, _)| entity.as_bytes() == name)
            .map(|&(_, codepoint)| codepoint)?
    };
    Some(DecodedEntity {
        codepoint,
        end: semicolon + 1,
    })
}

fn parse_reference_number(digits: &[u8], base: u32) -> Option<u32> {
    const MAX_CODE_UNIT: u32 = 0x1f_ffff;
    let (negative, unsigned) = match digits.split_first() {
        Some((b'+', rest)) => (false, rest),
        Some((b'-', rest)) => (true, rest),
        _ => (false, digits),
    };
    if unsigned.is_empty() || unsigned[0] == b'_' || unsigned[unsigned.len() - 1] == b'_' {
        return None;
    }
    let mut value: u32 = 0;
    for &byte in unsigned {
        if byte == b'_' {
            continue;
        }
        let digit = char::from(byte).to_digit(base)?;
        value = value.checked_mul(base)?.checked_add(digit)?;
        if value > MAX_CODE_UNIT {
            return None;
        }
    }
    if negative && value != 0 {
        return None;
    }
    Some(value)
}

fn parse_angle_autolink(text: &str, start: usize) -> Option<InlineLink<'_>> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'<') {
        return None;
    }
    let end = angle_autolink_candidate_end(bytes, start);
    if end <= start + 1 || end > bytes.len() || bytes[end - 1] != b'>' {
        return None;
    }
    let value = &text[start + 1..end - 1];
    if is_valid_angle_autolink_uri(value.as_bytes()) && is_valid_link_url(value) {
        return Some(InlineLink {
            text: value,
            url: value,
            end,
            destination_prefix: "",
            label_mode: LabelMode::Literal,
        });
    }
    if is_valid_angle_autolink_email(value.as_bytes())
        && is_valid_link_url_with_prefix("mailto:", value)
    {
        return Some(InlineLink {
            text: value,
            url: value,
            end,
            destination_prefix: "mailto:",
            label_mode: LabelMode::Literal,
        });
    }
    None
}

fn angle_autolink_candidate_end(bytes: &[u8], start: usize) -> usize {
    if bytes.get(start) != Some(&b'<') {
        return start;
    }
    let end = start
        + 1
        + bytes[start + 1..]
            .iter()
            .take_while(|&&byte| byte != b'>' && byte != b'\n')
            .count();
    if bytes.get(end) == Some(&b'>') {
        end + 1
    } else {
        end
    }
}

fn is_valid_angle_autolink_uri(value: &[u8]) -> bool {
    let colon = value
        .iter()
        .position(|&byte| byte == b':')
        .unwrap_or(value.len());
    if !(2..=32).contains(&colon) || colon == value.len() || !is_ascii_alpha(value[0]) {
        return false;
    }
    value[1..colon]
        .iter()
        .all(|&byte| is_ascii_alpha_numeric(byte) || matches!(byte, b'+' | b'-' | b'.'))
        && value[colon + 1..]
            .iter()
            .all(|&byte| byte > b' ' && byte != b'<' && byte != b'>')
}

fn is_valid_angle_autolink_email(value: &[u8]) -> bool {
    let mut at_positions = value
        .iter()
        .enumerate()
        .filter(|&(_, &byte)| byte == b'@')
        .map(|(index, _)| index);
    let Some(at) = at_positions.next() else {
        return false;
    };
    if at_positions.next().is_some() || at == 0 || at + 1 >= value.len() {
        return false;
    }
    value[..at]
        .iter()
        .all(|&byte| is_angle_autolink_email_local_byte(byte))
        && value[at + 1..]
            .split(|&byte| byte == b'.')
            .all(is_valid_angle_autolink_email_domain_label)
}

fn is_angle_autolink_email_local_byte(byte: u8) -> bool {
    is_ascii_alpha_numeric(byte)
        || matches!(
            byte,
            b'.' | b'!'
                | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
                | b'-'
        )
}

fn is_valid_angle_autolink_email_domain_label(label: &[u8]) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    label.iter().enumerate().all(|(index, &byte)| {
        if index == 0 || index + 1 == label.len() {
            is_ascii_alpha_numeric(byte)
        } else {
            is_ascii_alpha_numeric(byte) || byte == b'-'
        }
    })
}

fn url_scheme_len(text: &str) -> Option<usize> {
    if text.starts_with("https://") {
        Some("https://".len())
    } else if text.starts_with("http://") {
        Some("http://".len())
    } else {
        None
    }
}

fn code_span_url_end(content: &str) -> Option<usize> {
    let scheme_len = url_scheme_len(content)?;
    let end = if content.len() > scheme_len && content.ends_with('.') {
        content.len() - 1
    } else {
        content.len()
    };
    if end == scheme_len || !is_valid_link_url(&content[..end]) {
        return None;
    }
    if content
        .chars()
        .any(|character| character == ' ' || !is_terminal_safe_char(character))
    {
        return None;
    }
    Some(end)
}

fn parse_bare_url(
    text: &str,
    start: usize,
    scheme_len: usize,
    candidate_end: usize,
) -> Option<InlineLink<'_>> {
    let bytes = text.as_bytes();
    let mut end = candidate_end;
    while end > start + scheme_len && is_trailing_bare_url_byte(bytes[end - 1]) {
        end -= 1;
    }
    let url = &text[start..end];
    if !is_valid_link_url(url) {
        return None;
    }
    Some(InlineLink {
        text: url,
        url,
        end,
        destination_prefix: "",
        label_mode: LabelMode::Escaped,
    })
}

fn is_valid_link_url(url: &str) -> bool {
    !url.is_empty() && url.len() <= MAX_LINK_URL_BYTES && url.chars().all(is_terminal_safe_char)
}

fn is_valid_link_url_with_prefix(prefix: &str, url: &str) -> bool {
    prefix.len() + url.len() <= MAX_LINK_URL_BYTES
        && prefix.chars().all(is_terminal_safe_char)
        && is_valid_link_url(url)
}

fn is_bare_url_boundary(bytes: &[u8], start: usize, after_opening_delimiter: bool) -> bool {
    if start == 0 || after_opening_delimiter {
        return true;
    }
    let previous = bytes[start - 1];
    !is_ascii_word_byte(previous) && previous != b'<'
}

fn is_bare_url_terminator(byte: u8) -> bool {
    is_ascii_whitespace(byte) || matches!(byte, b')' | b']' | b'}' | b'>')
}

fn is_trailing_bare_url_byte(byte: u8) -> bool {
    is_trailing_url_punctuation(byte) || matches!(byte, b'*' | b'_' | b'~')
}
