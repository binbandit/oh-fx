use std::borrow::Cow;
use std::fmt::Write;

pub fn contains_ignore_case(haystack: impl AsRef<[u8]>, needle: impl AsRef<[u8]>) -> bool {
    let (haystack, needle) = (haystack.as_ref(), needle.as_ref());
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

pub(crate) fn is_posix_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c')
}

pub fn is_model_safe_text(text: &[u8]) -> bool {
    !text.contains(&0) && std::str::from_utf8(text).is_ok()
}

pub fn write_head_tail_bounded(text: &[u8], max_content_bytes: usize, marker: &str) -> Vec<u8> {
    if text.len() <= max_content_bytes {
        return text.to_vec();
    }
    let marker = marker.as_bytes();
    if max_content_bytes <= marker.len() {
        return marker[..max_content_bytes].to_vec();
    }
    let retained_bytes = max_content_bytes - marker.len();
    let head_bytes = retained_bytes.div_ceil(2);
    let tail_bytes = retained_bytes - head_bytes;
    let head_end = utf8_backward_boundary(text, head_bytes);
    let tail_start = utf8_forward_boundary(text, text.len() - tail_bytes);
    let mut bounded = Vec::with_capacity(head_end + marker.len() + text.len() - tail_start);
    bounded.extend_from_slice(&text[..head_end]);
    bounded.extend_from_slice(marker);
    bounded.extend_from_slice(&text[tail_start..]);
    bounded
}

fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

fn utf8_backward_boundary(text: &[u8], index: usize) -> usize {
    let mut boundary = index.min(text.len());
    while boundary > 0 && boundary < text.len() && is_utf8_continuation(text[boundary]) {
        boundary -= 1;
    }
    boundary
}

fn utf8_forward_boundary(text: &[u8], index: usize) -> usize {
    let mut boundary = index.min(text.len());
    while boundary < text.len() && is_utf8_continuation(text[boundary]) {
        boundary += 1;
    }
    boundary
}

pub fn normalize_line_endings_in_place(bytes: &mut Vec<u8>) {
    let mut read = 0;
    let mut write = 0;
    while read < bytes.len() {
        if bytes[read] == b'\r' {
            bytes[write] = b'\n';
            write += 1;
            read += 1;
            if read < bytes.len() && bytes[read] == b'\n' {
                read += 1;
            }
            continue;
        }
        bytes[write] = bytes[read];
        write += 1;
        read += 1;
    }
    bytes.truncate(write);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedText {
    pub text: String,
    pub truncated: bool,
}

fn utf8_sequence_length(first_byte: u8) -> Option<usize> {
    match first_byte {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

pub fn encode_terminal_safe(raw: &[u8], max_encoded_bytes: usize) -> EncodedText {
    let marker = "...";
    let marker_len = marker.len().min(max_encoded_bytes);
    let content_limit = max_encoded_bytes - marker_len;
    let mut text = String::new();
    let mut marker_boundary = 0;
    let mut truncated = false;
    let mut index = 0;
    while index < raw.len() {
        let (source_len, token) = terminal_safe_token(raw, index);
        let Some(next_len) = text.len().checked_add(token.encoded_len()) else {
            truncated = true;
            break;
        };
        if next_len > max_encoded_bytes {
            truncated = true;
            break;
        }
        token.write(&mut text);
        index += source_len;
        if next_len <= content_limit {
            marker_boundary = next_len;
        }
    }
    if truncated {
        text.truncate(marker_boundary);
        text.push_str(&marker[..marker_len]);
    }
    EncodedText { text, truncated }
}

pub fn encode_terminal_safe_path_tail(raw: &[u8], max_encoded_bytes: usize) -> Option<String> {
    let basename_len = path_basename(raw).len();
    if basename_len == 0 {
        return None;
    }
    let basename_source_start = raw.len() - basename_len;
    let mut encoded = String::new();
    let mut boundaries = vec![0];
    let mut encoded_basename_start = None;
    let mut index = 0;
    while index < raw.len() {
        if index == basename_source_start {
            encoded_basename_start = Some(encoded.len());
        }
        let (source_len, token) = terminal_safe_token(raw, index);
        token.write(&mut encoded);
        index += source_len;
        boundaries.push(encoded.len());
    }
    let basename_start = encoded_basename_start?;
    let basename_bytes = encoded.len() - basename_start;
    if basename_bytes > max_encoded_bytes {
        return None;
    }
    if encoded.len() <= max_encoded_bytes {
        return Some(encoded);
    }
    let marker = "\u{2026}";
    let suffix_budget =
        max_encoded_bytes.checked_sub(marker.len() + basename_bytes)? + basename_bytes;
    let start = boundaries
        .into_iter()
        .find(|boundary| encoded.len() - boundary <= suffix_budget)?;
    (start <= basename_start).then(|| format!("{marker}{}", &encoded[start..]))
}

fn path_basename(path: &[u8]) -> &[u8] {
    let trimmed_len = path.len() - path.iter().rev().take_while(|byte| **byte == b'/').count();
    let trimmed = &path[..trimmed_len];
    let start = trimmed
        .iter()
        .rposition(|byte| *byte == b'/')
        .map_or(0, |separator| separator + 1);
    &trimmed[start..]
}

pub fn sanitize_model_text_owned(text: Vec<u8>) -> String {
    match String::from_utf8(text) {
        Ok(valid) if !valid.contains('\0') => valid,
        Ok(valid) => omitted_binary_output(valid.len()),
        Err(invalid) => omitted_binary_output(invalid.as_bytes().len()),
    }
}

fn omitted_binary_output(len: usize) -> String {
    format!("binary or non-utf8 tool output omitted ({len} bytes)")
}

pub fn mask_secrets(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let spans = union_of_spans(
        secret_spans(bytes, upstream_secret_span),
        secret_spans(bytes, hardened_secret_span),
    );
    if spans.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut masked = String::with_capacity(text.len());
    let mut copied_until = 0;
    for span in spans {
        masked.push_str(&text[copied_until..span.start]);
        masked.push_str("[redacted]");
        copied_until = span.end;
    }
    masked.push_str(&text[copied_until..]);
    Cow::Owned(masked)
}

pub fn sanitize_assistant_text(text: &str) -> &str {
    const TRIMMED: [char; 4] = [' ', '\r', '\n', '\t'];
    let trimmed = text.trim_matches(TRIMMED);
    if trimmed.is_empty() {
        return trimmed;
    }
    let Some(first_break) = trimmed.find('\n') else {
        return trimmed;
    };
    let first_line = trimmed[..first_break].trim_matches(TRIMMED).as_bytes();
    let is_intro = contains_ignore_case(first_line, b"i'm oh-fx")
        || contains_ignore_case(first_line, b"i am oh-fx")
        || contains_ignore_case(first_line, b"local coding assistant");
    if !is_intro {
        return trimmed;
    }
    trimmed[first_break + 1..].trim_start_matches(TRIMMED)
}

#[derive(Clone, Copy, Debug)]
enum SafeToken<'a> {
    Literal(&'a str),
    ByteEscape(u8),
    CodepointEscape(u32),
}

impl SafeToken<'_> {
    fn write(self, out: &mut String) {
        match self {
            Self::Literal(text) => out.push_str(text),
            Self::ByteEscape(byte) => {
                let _ = write!(out, "\\x{byte:02x}");
            }
            Self::CodepointEscape(codepoint) => {
                let _ = write!(out, "\\u{{{codepoint:04x}}}");
            }
        }
    }

    fn encoded_len(self) -> usize {
        match self {
            Self::Literal(text) => text.len(),
            Self::ByteEscape(_) => "\\x00".len(),
            Self::CodepointEscape(0..=0xffff) => "\\u{0000}".len(),
            Self::CodepointEscape(0x1_0000..=0xf_ffff) => "\\u{00000}".len(),
            Self::CodepointEscape(_) => "\\u{000000}".len(),
        }
    }
}

fn terminal_safe_token(raw: &[u8], index: usize) -> (usize, SafeToken<'_>) {
    let byte = raw[index];
    if byte < 0x20 || byte == 0x7f {
        return (1, SafeToken::ByteEscape(byte));
    }
    let Some(sequence_len) = utf8_sequence_length(byte) else {
        return (1, SafeToken::ByteEscape(byte));
    };
    if index + sequence_len > raw.len() {
        return (1, SafeToken::ByteEscape(byte));
    }
    let Ok(sequence) = std::str::from_utf8(&raw[index..index + sequence_len]) else {
        return (1, SafeToken::ByteEscape(byte));
    };
    match sequence.chars().next() {
        Some(character) if !is_terminal_safe_char(character) => (
            sequence_len,
            SafeToken::CodepointEscape(u32::from(character)),
        ),
        _ => (sequence_len, SafeToken::Literal(sequence)),
    }
}

pub fn is_terminal_safe_char(character: char) -> bool {
    let codepoint = u32::from(character);
    codepoint >= 0x20 && codepoint != 0x7f && !is_non_printing_codepoint(codepoint)
}

pub fn escape_terminal_controls(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_terminal_control) {
        return Cow::Borrowed(text);
    }
    let mut escaped = String::with_capacity(text.len() + 16);
    for character in text.chars() {
        if is_terminal_control(character) {
            control_escape(character).write(&mut escaped);
        } else {
            escaped.push(character);
        }
    }
    Cow::Owned(escaped)
}

fn is_terminal_control(character: char) -> bool {
    matches!(
        u32::from(character),
        0x00..=0x08 | 0x0a..=0x1f | 0x7f..=0x9f | 0x2028..=0x202e | 0x2066..=0x2069
    )
}

fn control_escape(character: char) -> SafeToken<'static> {
    match u8::try_from(character) {
        Ok(byte) if byte.is_ascii() => SafeToken::ByteEscape(byte),
        _ => SafeToken::CodepointEscape(u32::from(character)),
    }
}

fn is_non_printing_codepoint(codepoint: u32) -> bool {
    matches!(
        codepoint,
        0x80..=0x9f
            | 0xad
            | 0x34f
            | 0x61c
            | 0x115f..=0x1160
            | 0x17b4..=0x17b5
            | 0x180b..=0x180f
            | 0x200b..=0x200f
            | 0x2028..=0x202e
            | 0x2060..=0x206f
            | 0x3164
            | 0xfe00..=0xfe0d
            | 0xfeff
            | 0xffa0
            | 0xfff0..=0xfffb
            | 0x1bca0..=0x1bca3
            | 0x1d173..=0x1d17a
            | 0xe0000..=0xe0fff
    )
}

#[derive(Clone, Copy)]
struct SecretSpan {
    start: usize,
    end: usize,
}

impl SecretSpan {
    fn new(start: usize, end: usize) -> Option<Self> {
        (end > start).then_some(Self { start, end })
    }
}

type SecretMatcher = fn(&[u8], usize) -> Option<SecretSpan>;

fn secret_spans(text: &[u8], matcher: SecretMatcher) -> Vec<SecretSpan> {
    let mut spans = Vec::new();
    let mut index = 0;
    while index < text.len() {
        match matcher(text, index) {
            Some(span) => {
                index = span.end;
                spans.push(span);
            }
            None => index += 1,
        }
    }
    spans
}

fn union_of_spans(mut spans: Vec<SecretSpan>, more: Vec<SecretSpan>) -> Vec<SecretSpan> {
    spans.extend(more);
    spans.sort_unstable_by_key(|span| (span.start, span.end));
    let mut union: Vec<SecretSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        match union.last_mut() {
            Some(last) if span.start < last.end => last.end = last.end.max(span.end),
            _ => union.push(span),
        }
    }
    union
}

fn upstream_secret_span(text: &[u8], start: usize) -> Option<SecretSpan> {
    https_credentials(text, start)
        .or_else(|| aws_access_key(text, start))
        .or_else(|| sensitive_assignment(text, start))
        .or_else(|| vendor_token(text, start))
}

fn hardened_secret_span(text: &[u8], start: usize) -> Option<SecretSpan> {
    url_credentials(text, start)
        .or_else(|| sensitive_key_value(text, start))
        .or_else(|| authorization_credentials(text, start))
        .or_else(|| json_web_token(text, start))
}

fn https_credentials(text: &[u8], start: usize) -> Option<SecretSpan> {
    let credential_start = start + "https://".len();
    if !text[start..].starts_with(b"https://") {
        return None;
    }
    let end = credential_start
        + run_length(&text[credential_start..], |byte| {
            !is_posix_space(byte) && !matches!(byte, b'/' | b'?' | b'#' | b'@')
        });
    if text.get(end) != Some(&b'@') || !text[credential_start..end].contains(&b':') {
        return None;
    }
    SecretSpan::new(credential_start, end)
}

fn url_credentials(text: &[u8], start: usize) -> Option<SecretSpan> {
    if start == 0 || !text[start - 1].is_ascii_alphanumeric() || !text[start..].starts_with(b"://")
    {
        return None;
    }
    let userinfo_start = start + "://".len();
    let at_sign = credentials_at_sign(text, userinfo_start)?;
    if !text[userinfo_start..at_sign].contains(&b':') {
        return None;
    }
    SecretSpan::new(userinfo_start, at_sign)
}

fn credentials_at_sign(text: &[u8], authority_start: usize) -> Option<usize> {
    let mut at_sign = None;
    let mut url_ended = false;
    for (index, &byte) in text.iter().enumerate().skip(authority_start) {
        if is_posix_space(byte) || matches!(byte, b'/' | b'?' | b'#') {
            break;
        }
        if byte == b'@' {
            at_sign = Some(index);
            if url_ended {
                break;
            }
        } else if !url_ended && is_outside_url(byte) {
            if at_sign.is_some() {
                break;
            }
            url_ended = true;
        }
    }
    at_sign
}

fn is_outside_url(byte: u8) -> bool {
    byte.is_ascii_control()
        || matches!(
            byte,
            b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}'
        )
}

fn aws_access_key(text: &[u8], start: usize) -> Option<SecretSpan> {
    for prefix in ["AKIA", "ASIA", "AIDA", "AGPA", "AROA", "ANPA"] {
        if start + 20 > text.len() || !text[start..].starts_with(prefix.as_bytes()) {
            continue;
        }
        let candidate = &text[start..start + 20];
        if !candidate
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            return None;
        }
        if start > 0 && is_token_char(text[start - 1]) {
            return None;
        }
        if start + 20 < text.len() && is_token_char(text[start + 20]) {
            return None;
        }
        return SecretSpan::new(start, start + 20);
    }
    None
}

fn sensitive_assignment(text: &[u8], start: usize) -> Option<SecretSpan> {
    if start > 0 && is_assignment_key_char(text[start - 1]) {
        return None;
    }
    let equals = start + run_length(&text[start..], is_assignment_key_char);
    if equals == start
        || text.get(equals) != Some(&b'=')
        || !is_secret_env_key(&text[start..equals])
    {
        return None;
    }
    secret_assignment_value_span(text, start, equals + 1)
}

fn vendor_token(text: &[u8], start: usize) -> Option<SecretSpan> {
    if start > 0 && is_token_char(text[start - 1]) {
        return None;
    }
    let rest = &text[start..];
    for prefix in [
        "sk-",
        "sk_live_",
        "pk_live_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "Bearer ",
    ] {
        let prefix = prefix.as_bytes();
        if rest.len() > prefix.len() && rest.starts_with(prefix) {
            let len = prefix.len() + run_length(&rest[prefix.len()..], is_token_char);
            if len >= 16 {
                return SecretSpan::new(start, start + len);
            }
        }
    }
    github_token(text, start)
}

fn github_token(text: &[u8], start: usize) -> Option<SecretSpan> {
    if start + 40 > text.len() {
        return None;
    }
    let candidate = &text[start..];
    if !["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
        .iter()
        .any(|prefix| candidate.starts_with(prefix.as_bytes()))
    {
        return None;
    }
    let body_len = run_length(&candidate[4..], is_token_char);
    if body_len < 36 {
        return None;
    }
    SecretSpan::new(start, start + 4 + body_len)
}

fn authorization_credentials(text: &[u8], start: usize) -> Option<SecretSpan> {
    if start > 0 && is_token_char(text[start - 1]) {
        return None;
    }
    let rest = &text[start..];
    let scheme = ["bearer", "basic", "token"].into_iter().find(|scheme| {
        rest.len() > scheme.len() && rest[..scheme.len()].eq_ignore_ascii_case(scheme.as_bytes())
    })?;
    let spaces = run_length(&rest[scheme.len()..], is_blank);
    if spaces == 0 {
        return None;
    }
    let credential_start = scheme.len() + spaces;
    let end = credential_start + run_length(&rest[credential_start..], is_credential_char);
    let long_bearer = scheme == "bearer" && end >= 16;
    if !long_bearer && !looks_like_credential(&rest[credential_start..end]) {
        return None;
    }
    SecretSpan::new(start, start + end)
}

fn looks_like_credential(candidate: &[u8]) -> bool {
    candidate.len() >= 8
        && candidate
            .iter()
            .any(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'/' | b'='))
}

fn sensitive_key_value(text: &[u8], start: usize) -> Option<SecretSpan> {
    if start > 0 && is_key_char(text[start - 1]) {
        return None;
    }
    let key_end = start + run_length(&text[start..], is_key_char);
    let dashes = run_length(&text[start..key_end], |byte| byte == b'-');
    let key_start = start + dashes;
    if dashes > 2 || key_start == key_end || !is_assignment_key_char(text[key_start]) {
        return None;
    }
    let key = &text[key_start..key_end];
    let name = key.rsplit(|&byte| byte == b'.').next().unwrap_or(key);
    if !is_secret_key_name(name) {
        return None;
    }
    match text.get(key_end)? {
        b'=' => {
            let last_word = key
                .iter()
                .rposition(|&byte| !is_assignment_key_char(byte))
                .map_or(key_start, |offset| key_start + offset + 1);
            secret_assignment_value_span(text, last_word, key_end + 1)
        }
        b' ' | b'\t' if dashes > 0 => flag_value(text, key_end),
        b':' if dashes == 0 => header_value(text, key_start, key_end + 1),
        _ if dashes == 0 => json_member_value(text, key_start, key_end),
        _ => None,
    }
}

fn flag_value(text: &[u8], flag_end: usize) -> Option<SecretSpan> {
    let value_start = flag_end + run_length(&text[flag_end..], is_blank);
    if text
        .get(value_start)
        .is_none_or(|byte| b"-<>|&;".contains(byte))
    {
        return None;
    }
    secret_assignment_value_span(text, value_start, value_start)
}

fn header_value(text: &[u8], key_start: usize, after_colon: usize) -> Option<SecretSpan> {
    if key_start > 0 && matches!(text[key_start - 1], b'/' | b'\\') {
        return None;
    }
    let enclosing = Quote::before(text, key_start);
    let value_start = after_colon + run_length(&text[after_colon..], is_blank);
    let value_is_empty = enclosing.is_some_and(|quote| quote.closes_at(text, value_start));
    if value_is_empty || (value_start == after_colon && enclosing.is_none()) {
        return None;
    }
    match text.get(value_start)? {
        b':' | b'{' | b'[' => None,
        _ if Quote::at(text, value_start).is_some() => quoted_secret(text, value_start),
        _ => {
            let end = enclosing.map_or_else(
                || unquoted_value_end(text, value_start),
                |quote| quote.content_end(text, value_start),
            );
            let end = value_start + text[value_start..end].trim_ascii_end().len();
            plain_secret(text, value_start, end)
        }
    }
}

fn json_member_value(text: &[u8], key_start: usize, key_end: usize) -> Option<SecretSpan> {
    let quote = Quote::before(text, key_start)?;
    if !quote.closes_at(text, key_end) {
        return None;
    }
    let colon = key_end + quote.len() + run_length(&text[key_end + quote.len()..], is_blank);
    if text.get(colon) != Some(&b':') {
        return None;
    }
    let value_start = colon + 1 + run_length(&text[colon + 1..], is_blank);
    quoted_secret(text, value_start)
}

fn quoted_secret(text: &[u8], quote_start: usize) -> Option<SecretSpan> {
    let quote = Quote::at(text, quote_start)?;
    let content_start = quote_start + quote.len();
    plain_secret(text, content_start, quote.content_end(text, content_start))
}

fn plain_secret(text: &[u8], start: usize, end: usize) -> Option<SecretSpan> {
    if is_pure_shell_variable_reference(&text[start..end]) {
        return None;
    }
    SecretSpan::new(start, end)
}

fn unquoted_value_end(text: &[u8], start: usize) -> usize {
    (start..text.len())
        .find(|&index| match text[index] {
            b'\n' | b'\r' | b'"' => true,
            b'\\' => matches!(text.get(index + 1), Some(b'n' | b'r' | b'"')),
            _ => false,
        })
        .unwrap_or(text.len())
}

#[derive(Clone, Copy)]
struct Quote {
    byte: u8,
    escaped: bool,
}

impl Quote {
    fn before(text: &[u8], index: usize) -> Option<Self> {
        let byte = *text.get(index.checked_sub(1)?)?;
        is_quote(byte).then(|| Self {
            byte,
            escaped: index >= 2 && text[index - 2] == b'\\',
        })
    }

    fn at(text: &[u8], index: usize) -> Option<Self> {
        match text.get(index..)? {
            [b'\\', byte, ..] if is_quote(*byte) => Some(Self {
                byte: *byte,
                escaped: true,
            }),
            [byte, ..] if is_quote(*byte) => Some(Self {
                byte: *byte,
                escaped: false,
            }),
            _ => None,
        }
    }

    fn len(self) -> usize {
        1 + usize::from(self.escaped)
    }

    fn closes_at(self, text: &[u8], index: usize) -> bool {
        Self::at(text, index)
            .is_some_and(|quote| quote.byte == self.byte && quote.escaped == self.escaped)
    }

    fn content_end(self, text: &[u8], start: usize) -> usize {
        let mut index = start;
        while index < text.len() && !matches!(text[index], b'\n' | b'\r') {
            if text[index] == b'\\' {
                if self.closes_at(text, index) {
                    break;
                }
                index += 1;
                if index < text.len() && !matches!(text[index], b'\n' | b'\r') {
                    index += 1;
                }
                continue;
            }
            if self.closes_at(text, index) {
                break;
            }
            index += 1;
        }
        index
    }
}

fn is_quote(byte: u8) -> bool {
    matches!(byte, b'"' | b'\'')
}

fn json_web_token(text: &[u8], start: usize) -> Option<SecretSpan> {
    if start > 0 && is_token_char(text[start - 1]) {
        return None;
    }
    let header_end = start + run_length(&text[start..], is_base64url_char);
    if text.get(header_end) != Some(&b'.') || !opens_json_object(&text[start..header_end]) {
        return None;
    }
    let payload_end = header_end + 1 + run_length(&text[header_end + 1..], is_base64url_char);
    if payload_end == header_end + 1 || text.get(payload_end) != Some(&b'.') {
        return None;
    }
    let end = payload_end + 1 + run_length(&text[payload_end + 1..], is_base64url_char);
    SecretSpan::new(start, end)
}

fn opens_json_object(base64url: &[u8]) -> bool {
    let mut significant =
        base64url_decoded(base64url).filter(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
    significant.next() == Some(b'{') && significant.next() == Some(b'"')
}

fn base64url_decoded(base64url: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut buffer = 0_u16;
    let mut buffered_bits = 0_u32;
    base64url.iter().filter_map(move |&character| {
        buffer = (buffer << 6) | u16::from(base64url_value(character));
        buffered_bits += 6;
        if buffered_bits < 8 {
            return None;
        }
        buffered_bits -= 8;
        let byte = buffer >> buffered_bits;
        buffer &= (1 << buffered_bits) - 1;
        u8::try_from(byte).ok()
    })
}

fn base64url_value(character: u8) -> u8 {
    match character {
        b'A'..=b'Z' => character - b'A',
        b'a'..=b'z' => character - b'a' + 26,
        b'0'..=b'9' => character - b'0' + 52,
        b'-' => 62,
        _ => 63,
    }
}

fn secret_assignment_value_span(
    text: &[u8],
    assignment_start: usize,
    value_start: usize,
) -> Option<SecretSpan> {
    let value = assignment_value_span(text, assignment_start, value_start)?;
    if is_pure_symbolic_assignment(text, assignment_start, value_start, &value) {
        return None;
    }
    Some(value)
}

fn is_pure_symbolic_assignment(
    text: &[u8],
    assignment_start: usize,
    value_start: usize,
    value: &SecretSpan,
) -> bool {
    if text[value_start] == b'\'' {
        return false;
    }
    if !is_pure_shell_variable_reference(&text[value.start..value.end]) {
        return false;
    }
    let outer_double_quoted = assignment_start > 0 && text[assignment_start - 1] == b'"';
    if text[value_start] != b'"' && !outer_double_quoted {
        return value.end == text.len() || is_shell_word_boundary(text, value.end);
    }
    let closing_quote = value.end;
    if closing_quote >= text.len() || text[closing_quote] != b'"' {
        return false;
    }
    let next = closing_quote + 1;
    next == text.len() || is_shell_word_boundary(text, next)
}

fn is_shell_word_boundary(text: &[u8], index: usize) -> bool {
    match text[index] {
        b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b')' => true,
        b'<' | b'>' => index + 1 == text.len() || text[index + 1] != b'(',
        _ => false,
    }
}

fn is_pure_shell_variable_reference(value: &[u8]) -> bool {
    if value.len() < 2 || value[0] != b'$' {
        return false;
    }
    if value[1] == b'{' {
        if value.len() < 4 || value[value.len() - 1] != b'}' {
            return false;
        }
        return is_shell_variable_name(&value[2..value.len() - 1]);
    }
    is_shell_variable_name(&value[1..])
}

fn is_shell_variable_name(value: &[u8]) -> bool {
    let Some((&first, rest)) = value.split_first() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && rest
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn assignment_value_span(
    text: &[u8],
    assignment_start: usize,
    value_start: usize,
) -> Option<SecretSpan> {
    if value_start >= text.len() {
        return None;
    }

    if text[value_start] == b'"' || text[value_start] == b'\'' {
        let quote = text[value_start];
        let content_start = value_start + 1;
        let len = run_length(&text[content_start..], |byte| {
            !matches!(byte, b'\n' | b'\r') && byte != quote
        });
        return SecretSpan::new(content_start, content_start + len);
    }

    if assignment_start > 0 && text[assignment_start - 1] == b'"' {
        let len = run_length(&text[value_start..], |byte| {
            !matches!(byte, b'\n' | b'\r' | b'"')
        });
        return SecretSpan::new(value_start, value_start + len);
    }

    let len = run_length(&text[value_start..], |byte| {
        !is_posix_space(byte) && !matches!(byte, b'"' | b'\'')
    });
    SecretSpan::new(value_start, value_start + len)
}

const SECRET_ENV_KEY_WORDS: [&str; 7] = [
    "access_key",
    "api_key",
    "apikey",
    "password",
    "private_key",
    "secret",
    "token",
];

const HARDENED_SECRET_KEY_WORDS: [&str; 4] = ["cookie", "credential", "passwd", "virtual_key"];

fn is_secret_env_key(key: &[u8]) -> bool {
    key.eq_ignore_ascii_case(b"passwd")
        || key.eq_ignore_ascii_case(b"database_url")
        || SECRET_ENV_KEY_WORDS
            .iter()
            .any(|word| contains_ignore_case(key, word.as_bytes()))
}

fn is_secret_key_name(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"database_url")
        || SECRET_ENV_KEY_WORDS
            .iter()
            .chain(&HARDENED_SECRET_KEY_WORDS)
            .any(|word| contains_key_word(name, word.as_bytes()))
        || names_authentication(name)
}

fn contains_key_word(key: &[u8], word: &[u8]) -> bool {
    key.windows(word.len()).any(|window| {
        window
            .iter()
            .zip(word)
            .all(|(&byte, &expected)| normalized_key_byte(byte) == expected)
    })
}

fn normalized_key_byte(byte: u8) -> u8 {
    if byte == b'-' {
        b'_'
    } else {
        byte.to_ascii_lowercase()
    }
}

fn names_authentication(key: &[u8]) -> bool {
    key.windows(4).enumerate().any(|(index, window)| {
        let after = &key[index + 4..];
        window.eq_ignore_ascii_case(b"auth")
            && !(index >= 2 && key[index - 2..index].eq_ignore_ascii_case(b"un"))
            && (!starts_with_ignore_case(after, b"or")
                || starts_with_ignore_case(after, b"oriz")
                || starts_with_ignore_case(after, b"oris"))
    })
}

fn starts_with_ignore_case(text: &[u8], prefix: &[u8]) -> bool {
    text.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

fn run_length(text: &[u8], matches: impl Fn(u8) -> bool) -> usize {
    text.iter().take_while(|&&byte| matches(byte)).count()
}

fn is_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

fn is_assignment_key_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_key_char(byte: u8) -> bool {
    is_assignment_key_char(byte) || matches!(byte, b'-' | b'.')
}

fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

fn is_base64url_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn is_credential_char(byte: u8) -> bool {
    is_token_char(byte) || matches!(byte, b'~' | b'+' | b'/' | b'=')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_model_safe_text_rejects_nul_bytes_and_invalid_utf_8() {
        assert!(is_model_safe_text(b"hello"));
        assert!(!is_model_safe_text(b"hello\x00world"));
        assert!(!is_model_safe_text(b"\xff"));
    }

    #[test]
    fn normalize_line_endings_in_place_compacts_crlf_and_normalizes_lone_cr() {
        let cases = [
            ("", ""),
            ("alpha\nbeta", "alpha\nbeta"),
            ("alpha\r\nbeta", "alpha\nbeta"),
            ("alpha\rbeta", "alpha\nbeta"),
            ("\r\n\r\n", "\n\n"),
            ("\r\r\n\n", "\n\n\n"),
            ("é\r\n🙂\r尾", "é\n🙂\n尾"),
        ];
        for (input, expected) in cases {
            let mut storage = input.as_bytes().to_vec();
            normalize_line_endings_in_place(&mut storage);
            assert_eq!(storage, expected.as_bytes());
        }
    }

    #[test]
    fn contains_ignore_case_handles_empty_needles_and_oversized_needles() {
        assert!(contains_ignore_case(b"Haystack", b""));
        assert!(!contains_ignore_case(b"short", b"longer needle"));
        assert!(contains_ignore_case(b"Local Coding Assistant", b"coding"));
        assert!(contains_ignore_case(b"\xffPARSE error\xfe", b"parse ERROR"));
        assert!(contains_ignore_case("openai/GPT-5", "gpt"));
    }

    #[test]
    fn sanitize_model_text_owned_reuses_valid_buffers_and_replaces_unsafe_ones() {
        let valid = b"plain text".to_vec();
        let pointer = valid.as_ptr();
        let sanitized = sanitize_model_text_owned(valid);
        assert_eq!(sanitized, "plain text");
        assert_eq!(sanitized.as_ptr(), pointer);
        assert_eq!(
            sanitize_model_text_owned(b"bad\xff".to_vec()),
            "binary or non-utf8 tool output omitted (4 bytes)"
        );
        assert_eq!(
            sanitize_model_text_owned(b"nul\0".to_vec()),
            "binary or non-utf8 tool output omitted (4 bytes)"
        );
    }

    #[test]
    fn mask_secrets_masks_env_style_secrets() {
        assert_eq!(
            mask_secrets("AI_GATEWAY_API_KEY=abcdefghijklmnop end"),
            "AI_GATEWAY_API_KEY=[redacted] end"
        );
    }

    #[test]
    fn mask_secrets_masks_quoted_sensitive_assignments() {
        let input =
            "API_KEY=\"double-secret\"\nPASSWORD='single-secret'\nACCESS_TOKEN=\"access-secret\"";
        let masked = mask_secrets(input);
        assert_eq!(
            masked,
            "API_KEY=\"[redacted]\"\nPASSWORD='[redacted]'\nACCESS_TOKEN=\"[redacted]\""
        );
        assert!(!masked.contains("double-secret"));
        assert!(!masked.contains("single-secret"));
        assert!(!masked.contains("access-secret"));
    }

    #[test]
    fn mask_secrets_preserves_pure_symbolic_sensitive_assignments() {
        let input = concat!(
            "AI_GATEWAY_API_KEY=\"$key\"\n",
            "sandbox -e \"AI_GATEWAY_API_KEY=$key\" \"$@\"\n",
            "GITHUB_TOKEN=$token\n",
            "DATABASE_URL=\"${database_url}\"\n",
            "TOKEN=\"$token\"; run-next\n",
            "PASSWORD=\"$password\"\tcheck-next\n",
            "ACCESS_TOKEN=\"$token\">token.out\n",
            "SECRET_KEY=\"$key\")",
        );
        assert_eq!(mask_secrets(input), input);
    }

    #[test]
    fn mask_secrets_masks_compound_or_literal_sensitive_assignments() {
        let input = concat!(
            "AI_GATEWAY_API_KEY=\"literal-value\"\n",
            "sandbox -e \"AI_GATEWAY_API_KEY=literal-value\"\n",
            "sandbox -e \"AI_GATEWAY_API_KEY=$key literal-suffix\"\n",
            "sandbox -e \"GITHUB_TOKEN=$token;literal-suffix\"\n",
            "sandbox -e \"ACCESS_TOKEN=$token>token.out\"\n",
            "sandbox -e \"SECRET_KEY=$key$tail\"\n",
            "sandbox -e \"GITHUB_TOKEN=$token-suffix\"\n",
            "sandbox -e \"API_KEY=$(load-key)\"\n",
            "GITHUB_TOKEN=\"$token-suffix\"\n",
            "DATABASE_URL=\"${database_url:-fallback}\"\n",
            "API_KEY=\"$(load-key)\"\n",
            "VERCEL_OIDC_TOKEN=\"$token\"literal-suffix\n",
            "OPENAI_API_KEY=$key\"literal-suffix\"\n",
            "PASSWORD=\"$password\"\x0bliteral-suffix\n",
            "ACCESS_TOKEN=\"$token\"\x0cliteral-suffix\n",
            "SECRET=\"$secret\"\rliteral-suffix\n",
            "AI_GATEWAY_API_KEY=\"$key\"(literal-suffix)\n",
            "GITHUB_TOKEN=\"$token\"<(literal-suffix)\n",
            "ACCESS_TOKEN=\"$token\">(literal-suffix)\n",
            "SECRET_KEY=\"$key",
        );
        let expected = concat!(
            "AI_GATEWAY_API_KEY=\"[redacted]\"\n",
            "sandbox -e \"AI_GATEWAY_API_KEY=[redacted]\"\n",
            "sandbox -e \"AI_GATEWAY_API_KEY=[redacted]\"\n",
            "sandbox -e \"GITHUB_TOKEN=[redacted]\"\n",
            "sandbox -e \"ACCESS_TOKEN=[redacted]\"\n",
            "sandbox -e \"SECRET_KEY=[redacted]\"\n",
            "sandbox -e \"GITHUB_TOKEN=[redacted]\"\n",
            "sandbox -e \"API_KEY=[redacted]\"\n",
            "GITHUB_TOKEN=\"[redacted]\"\n",
            "DATABASE_URL=\"[redacted]\"\n",
            "API_KEY=\"[redacted]\"\n",
            "VERCEL_OIDC_TOKEN=\"[redacted]\"literal-suffix\n",
            "OPENAI_API_KEY=[redacted]\"literal-suffix\"\n",
            "PASSWORD=\"[redacted]\"\x0bliteral-suffix\n",
            "ACCESS_TOKEN=\"[redacted]\"\x0cliteral-suffix\n",
            "SECRET=\"[redacted]\"\rliteral-suffix\n",
            "AI_GATEWAY_API_KEY=\"[redacted]\"(literal-suffix)\n",
            "GITHUB_TOKEN=\"[redacted]\"<(literal-suffix)\n",
            "ACCESS_TOKEN=\"[redacted]\">(literal-suffix)\n",
            "SECRET_KEY=\"[redacted]",
        );
        assert_eq!(mask_secrets(input), expected);
    }

    #[test]
    fn mask_secrets_preserves_non_sensitive_quoted_assignments() {
        let input = "PROJECT_NAME=\"secret-service\"\nGREETING='hello world'";
        assert_eq!(mask_secrets(input), input);
    }

    #[test]
    fn mask_secrets_masks_inline_tokens() {
        assert_eq!(
            mask_secrets("token sk-abcdefghijklmnop now"),
            "token [redacted] now"
        );
    }

    #[test]
    fn mask_secrets_preserves_inline_key_substrings_inside_ordinary_tokens() {
        let input = "printf output > ask-turn-default-auto.txt";
        assert_eq!(mask_secrets(input), input);
    }

    #[test]
    fn mask_secrets_masks_expanded_model_facing_secret_patterns() {
        let input = concat!(
            "aws=AKIA0123456789ABCDEF\n",
            "github=ghs_abcdefghijklmnopqrstuvwxyz0123456789AB\n",
            "url=https://user:token@example.com/path\n",
            "password=hunter2\n",
            "CUSTOM_API_KEY=abc123",
        );
        let masked = mask_secrets(input);
        assert_eq!(
            masked,
            concat!(
                "aws=[redacted]\n",
                "github=[redacted]\n",
                "url=https://[redacted]@example.com/path\n",
                "password=[redacted]\n",
                "CUSTOM_API_KEY=[redacted]",
            )
        );
        assert!(!masked.contains("AKIA0123456789ABCDEF"));
    }

    #[test]
    fn sanitize_assistant_text_strips_first_line_assistant_intro() {
        assert_eq!(
            sanitize_assistant_text(
                " \tI'm Fx, your local coding assistant.\n\n  Here is the answer.\n"
            ),
            "Here is the answer."
        );
        assert_eq!(
            sanitize_assistant_text("I'm Fx, your local coding assistant."),
            "I'm Fx, your local coding assistant."
        );
    }

    #[test]
    fn sanitize_assistant_text_recognizes_the_renamed_assistant() {
        assert_eq!(
            sanitize_assistant_text("I'm oh-fx.\nHere is the answer."),
            "Here is the answer."
        );
    }

    #[test]
    fn encode_terminal_safe_visibly_escapes_controls_line_breaks_and_invalid_utf_8() {
        let encoded = encode_terminal_safe(b"A\x1b[31m\n\r\x07\x7fB\xff", 128);
        assert_eq!(encoded.text, "A\\x1b[31m\\x0a\\x0d\\x07\\x7fB\\xff");
        assert!(!encoded.text.contains('\x1b'));
        assert!(!encoded.text.contains('\n'));
        assert!(!encoded.truncated);
    }

    #[test]
    fn encode_terminal_safe_makes_every_byte_value_terminal_safe_utf_8() {
        let raw: Vec<u8> = (0..=255).collect();
        let encoded = encode_terminal_safe(&raw, 2048);
        assert!(
            encoded
                .text
                .bytes()
                .all(|byte| byte >= 0x20 && byte != 0x7f)
        );
        assert!(!encoded.text.contains('\x1b'));
        assert!(!encoded.truncated);
    }

    #[test]
    fn encode_terminal_safe_escapes_utf_8_c1_controls() {
        let encoded = encode_terminal_safe("\u{0080}".as_bytes(), 64);
        assert_eq!(encoded.text, "\\u{0080}");
        assert_eq!(encoded.text.len(), 8);
    }

    #[test]
    fn encode_terminal_safe_escapes_invisible_and_bidi_formatting_code_points() {
        for (raw, expected) in [
            ("x\u{061c}abc", "x\\u{061c}abc"),
            ("soft\u{00ad}hyphen", "soft\\u{00ad}hyphen"),
            ("a\u{2066}b\u{2069}c", "a\\u{2066}b\\u{2069}c"),
            ("\u{202e}txt.exe", "\\u{202e}txt.exe"),
            ("joined\u{034f}", "joined\\u{034f}"),
            ("\u{3164}\u{ffa0}\u{115f}", "\\u{3164}\\u{ffa0}\\u{115f}"),
            ("\u{180e}\u{fff9}", "\\u{180e}\\u{fff9}"),
            ("\u{fe00}", "\\u{fe00}"),
            ("hi\u{e0041}\u{e007f}", "hi\\u{e0041}\\u{e007f}"),
            ("\u{1d173}\u{1bca0}", "\\u{1d173}\\u{1bca0}"),
        ] {
            assert_eq!(
                encode_terminal_safe(raw.as_bytes(), 128).text,
                expected,
                "{raw:?}"
            );
        }
    }

    #[test]
    fn encode_terminal_safe_keeps_emoji_presentation_and_ordinary_letters() {
        for text in [
            "\u{2764}\u{fe0f}",
            "\u{263a}\u{fe0e}",
            "café",
            "日本語",
            "مرحبا",
        ] {
            assert_eq!(encode_terminal_safe(text.as_bytes(), 128).text, text);
        }
    }

    #[test]
    fn terminal_safe_characters_are_exactly_those_the_encoder_keeps() {
        for codepoint in (0..=0x3_0000).chain(0xe_0000..=0xe_1000) {
            let Some(character) = char::from_u32(codepoint) else {
                continue;
            };
            let mut buffer = [0; 4];
            let encoded = encode_terminal_safe(character.encode_utf8(&mut buffer).as_bytes(), 64);
            assert_eq!(
                is_terminal_safe_char(character),
                encoded.text == character.to_string(),
                "{codepoint:#x}"
            );
        }
    }

    #[test]
    fn escape_terminal_controls_escapes_controls_and_bidi_reordering_once() {
        assert_eq!(
            escape_terminal_controls("a\x1b[31mb\x07c\rd\u{7f}"),
            "a\\x1b[31mb\\x07c\\x0dd\\x7f"
        );
        assert_eq!(
            escape_terminal_controls("\u{9b}2J \u{85} \u{2028}\u{2029}"),
            "\\u{009b}2J \\u{0085} \\u{2028}\\u{2029}"
        );
        assert_eq!(
            escape_terminal_controls("\u{202a}\u{202e}gpj.exe\u{2066}\u{2069}"),
            "\\u{202a}\\u{202e}gpj.exe\\u{2066}\\u{2069}"
        );
        let escaped = escape_terminal_controls("\x1b]8;;http://evil\x07");
        assert_eq!(escape_terminal_controls(&escaped), escaped);
    }

    #[test]
    fn escape_terminal_controls_keeps_tabs_scripts_and_emoji_joiners() {
        for text in [
            "tab\tseparated",
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
            "\u{0645}\u{06cc}\u{200c}\u{062e}\u{0648}\u{0627}\u{0647}\u{0645}",
            "\u{05e9}\u{200f}abc\u{200e}",
            "soft\u{ad}hyphen \u{2764}\u{fe0f}",
            "caf\u{e9} \u{65e5}\u{672c}",
        ] {
            assert!(
                matches!(escape_terminal_controls(text), Cow::Borrowed(_)),
                "{text:?}"
            );
        }
    }

    #[test]
    fn encode_terminal_safe_reserves_a_marker_when_source_encoding_is_truncated() {
        let encoded = encode_terminal_safe(b"abcdef", 5);
        assert_eq!(encoded.text, "ab...");
        assert_eq!(encoded.text.len(), 5);
        assert!(encoded.truncated);
    }

    #[test]
    fn path_tails_keep_the_basename_and_cut_leading_directories_with_an_ellipsis() {
        assert_eq!(
            encode_terminal_safe_path_tail(b"src/a\x1bb.rs", 64).as_deref(),
            Some("src/a\\x1bb.rs")
        );
        assert_eq!(
            encode_terminal_safe_path_tail(b"alpha/beta/gamma.txt", 16).as_deref(),
            Some("\u{2026}eta/gamma.txt")
        );
        assert_eq!(
            encode_terminal_safe_path_tail(b"alpha/beta/gamma.txt", 12).as_deref(),
            Some("\u{2026}gamma.txt")
        );
        assert_eq!(encode_terminal_safe_path_tail(b"alpha/gamma.txt", 9), None);
        assert_eq!(encode_terminal_safe_path_tail(b"/", 64), None);
        assert_eq!(encode_terminal_safe_path_tail(b"", 64), None);
        assert_eq!(
            encode_terminal_safe_path_tail(b"dir/", 64).as_deref(),
            Some("dir/")
        );
    }

    fn assert_masks(cases: &[(&str, &str)]) {
        for &(input, expected) in cases {
            assert_eq!(mask_secrets(input), expected, "{input:?}");
        }
    }

    #[test]
    fn mask_secrets_keeps_upstream_masked_shapes_byte_identical() {
        assert_masks(&[
            (
                "Authorization: Bearer abcdefghijklmnop",
                "Authorization: [redacted]",
            ),
            (
                "export OPENAI_API_KEY='sk-proj-abc'",
                "export OPENAI_API_KEY='[redacted]'",
            ),
            ("--token=abc123", "--token=[redacted]"),
            ("--max-tokens=4096", "--max-tokens=[redacted]"),
            ("key sk-ant-api03-abcdefgh", "key [redacted]"),
            (
                "https://x-access-token:ghs_abc@github.com/o/r",
                "https://[redacted]@github.com/o/r",
            ),
            (
                "aws=AKIA0123456789ABCDEF github=ghs_abcdefghijklmnopqrstuvwxyz0123456789AB",
                "aws=[redacted] github=[redacted]",
            ),
            (
                "DATABASE_URL=postgres://user:pass@db/app",
                "DATABASE_URL=[redacted]",
            ),
            (
                "x.DATABASE_URL=postgres://db/app",
                "x.DATABASE_URL=[redacted]",
            ),
            ("token sk_live_abcdefghijklmnop now", "token [redacted] now"),
            (
                "slack xoxb-1234567890-abcdefgh and xoxp-1234567890-abcdefgh",
                "slack [redacted] and [redacted]",
            ),
            (
                "pat github_pat_11ABCDEFG0123456789_abcdef",
                "pat [redacted]",
            ),
            (
                "curl -H \"Authorization: Bearer abcdefghijklmnop\" https://api.example.com",
                "curl -H \"Authorization: [redacted]\" https://api.example.com",
            ),
            (
                "{\"Authorization\": \"Bearer abcdefghijklmnop\"}",
                "{\"Authorization\": \"[redacted]\"}",
            ),
            (
                "MY_SECRET=hunter2 OTHER=fine",
                "MY_SECRET=[redacted] OTHER=fine",
            ),
            ("PASSWORD='$literal'", "PASSWORD='[redacted]'"),
            (
                "Bearer authentication is required here",
                "[redacted] is required here",
            ),
            ("\"SECRET_KEY=$key\" next", "\"SECRET_KEY=$key\" next"),
        ]);
    }

    #[test]
    fn mask_secrets_masks_credentials_in_urls_of_any_scheme() {
        assert_masks(&[
            (
                "git clone http://deploy:hunter2secret@git.internal/repo.git",
                "git clone http://[redacted]@git.internal/repo.git",
            ),
            (
                "psql postgres://admin:hunter2secret@db:5432/app",
                "psql postgres://[redacted]@db:5432/app",
            ),
            (
                "HTTPS://user:hunter2secret@example.com/",
                "HTTPS://[redacted]@example.com/",
            ),
            (
                "https://user:p@ss-tail-secret@example.com/",
                "https://[redacted]@example.com/",
            ),
            (
                "{\"url\":\"redis://:hunter2@cache:6379\",\"email\":\"a@b.example\"}",
                "{\"url\":\"redis://[redacted]@cache:6379\",\"email\":\"a@b.example\"}",
            ),
            (
                "ssh://git@github.com/org/repo",
                "ssh://git@github.com/org/repo",
            ),
        ]);
    }

    #[test]
    fn mask_secrets_masks_sensitive_header_and_yaml_values() {
        assert_masks(&[
            (
                "curl -H 'x-portkey-api-key: pk_9f8e7d6c5b4a3210' https://api.portkey.ai",
                "curl -H 'x-portkey-api-key: [redacted]' https://api.portkey.ai",
            ),
            (
                "x-portkey-virtual-key: vk-abc123",
                "x-portkey-virtual-key: [redacted]",
            ),
            (
                "authorization: bearer abcdefghijklmnopqrstuvwxyz",
                "authorization: [redacted]",
            ),
            (
                "Authorization: Bearer abcd/efghijklmnopqrstuvwxyz+0123==",
                "Authorization: [redacted]",
            ),
            (
                "Authorization: Basic dXNlcjpodW50ZXIyc2VjcmV0",
                "Authorization: [redacted]",
            ),
            (
                "Proxy-Authorization: Basic Zm9vOmJhcg==\r\nHost: example.com",
                "Proxy-Authorization: [redacted]\r\nHost: example.com",
            ),
            ("Cookie: session=abc123; theme=dark", "Cookie: [redacted]"),
            ("api_key: abcdef0123456789", "api_key: [redacted]"),
            (
                "  client_credential: abc\n  oauth_token=xyz",
                "  client_credential: [redacted]\n  oauth_token=[redacted]",
            ),
            (
                "{\"command\":\"curl -H \\\"x-api-key: abc123\\\" https://x.io\"}",
                "{\"command\":\"curl -H \\\"x-api-key: [redacted]\\\" https://x.io\"}",
            ),
            ("  password: ${DB_PASSWORD}", "  password: ${DB_PASSWORD}"),
            (
                "{\"content\":\"fn verify(token: &str) -> bool {\\n    ok\\n}\",\"path\":\"a.rs\"}",
                "{\"content\":\"fn verify(token: [redacted]\\n    ok\\n}\",\"path\":\"a.rs\"}",
            ),
        ]);
    }

    #[test]
    fn mask_secrets_masks_sensitive_json_string_members() {
        assert_masks(&[
            (
                "{\"api_key\": \"abcdef0123456789\"}",
                "{\"api_key\": \"[redacted]\"}",
            ),
            (
                "{\"headers\":{\"x-portkey-api-key\":\"pk_9f8e7d6c5b4a3210\"}}",
                "{\"headers\":{\"x-portkey-api-key\":\"[redacted]\"}}",
            ),
            (
                "{\"password\": \"pa\\\"ss\", \"user\": \"me\"}",
                "{\"password\": \"[redacted]\", \"user\": \"me\"}",
            ),
            (
                "{\\\"password\\\": \\\"hunter2\\\"}",
                "{\\\"password\\\": \\\"[redacted]\\\"}",
            ),
            ("{'secret': 'abc'}", "{'secret': '[redacted]'}"),
            (
                "{\"max_tokens\": 4096, \"auth\": false, \"token\": \"\"}",
                "{\"max_tokens\": 4096, \"auth\": false, \"token\": \"\"}",
            ),
        ]);
    }

    #[test]
    fn mask_secrets_masks_sensitive_command_line_flags() {
        assert_masks(&[
            (
                "mysql --password hunter2secret",
                "mysql --password [redacted]",
            ),
            (
                "tool --api-key=abc123 --verbose",
                "tool --api-key=[redacted] --verbose",
            ),
            ("tool -token 'abc def'", "tool -token '[redacted]'"),
            (
                "gh --token \"$GITHUB_TOKEN\" repo",
                "gh --token \"$GITHUB_TOKEN\" repo",
            ),
            ("mysql --password -v", "mysql --password -v"),
        ]);
    }

    #[test]
    fn mask_secrets_masks_authorization_schemes_and_json_web_tokens() {
        assert_masks(&[
            (
                "send Basic dXNlcjpodW50ZXIyc2VjcmV0 now",
                "send [redacted] now",
            ),
            (
                "Token 9944b09199c62bcf9418ad846dd0e4bbdfc6ee4b",
                "[redacted]",
            ),
            ("BEARER abc123def456", "[redacted]"),
            (
                "id_token eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJlLXNlY3JldA",
                "id_token [redacted]",
            ),
            (
                "unsigned eyJhbGciOiJub25lIn0.eyJzdWIiOiIxIn0.",
                "unsigned [redacted]",
            ),
            (
                "spaced IHsiYWxnIjoiSFMyNTYifQ.eyJzdWIiOiIxIn0.c2lnbmF0dXJl",
                "spaced [redacted]",
            ),
            (
                "pretty CnsKICAiYWxnIjogIkhTMjU2Igp9.eyJzdWIiOiIxIn0.c2ln end",
                "pretty [redacted] end",
            ),
            (
                "reordered eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln",
                "reordered [redacted]",
            ),
        ]);
    }

    #[test]
    fn mask_secrets_leaves_ordinary_prose_paths_and_code_alone() {
        for text in [
            "author: Jane Doe",
            "AUTHORITY=example.com",
            "Basic configuration is documented below",
            "token verification failed",
            "failed to read /run/secrets/db_password: permission denied",
            "auth.rs: permission denied",
            "use crate::token::Token;",
            "Error: 401 Unauthorized: invalid request",
            "docker login --password-stdin < token.txt",
            "ssh://git@github.com/org/repo",
            "eyJustAWord and eyJ.only",
            "e0.tar.gz and ex.y.z and IHs.a.b",
            "release v1.2.3 and archive.tar.gz",
            "printf output > ask-turn-default-auto.txt",
        ] {
            assert_eq!(mask_secrets(text), text);
        }
    }

    struct Xorshift(u64);

    impl Xorshift {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % u64::try_from(bound).unwrap_or(u64::MAX)).unwrap_or(0)
        }

        fn text(&mut self, pieces: &[&str], max_pieces: usize) -> String {
            let len = self.below(max_pieces + 1);
            (0..len).map(|_| pieces[self.below(pieces.len())]).collect()
        }
    }

    const HOSTILE_PIECES: [&str; 48] = [
        "TOKEN=",
        "token",
        "api_key",
        "x-api-key",
        "Authorization",
        "--",
        "-",
        ".",
        ":",
        ": ",
        "=",
        "\"",
        "'",
        "\\",
        "\\\"",
        "$",
        "${",
        "}",
        "{",
        "[",
        "x",
        "é",
        "😀",
        " ",
        "\n",
        "\r",
        "\t",
        "\x0b",
        ";",
        "https://",
        "://",
        "u:p@",
        "@",
        "/",
        "?",
        "#",
        "&",
        "%41",
        "sk-",
        "Bearer ",
        "basic ",
        "eyJ",
        "ghp_",
        "AKIA0123456789ABCDEF",
        "abc123+/=",
        "\u{1f469}\u{200d}\u{1f4bb}",
        "\\x1b",
        "\\u{0080}",
    ];

    #[test]
    fn text_utilities_survive_hostile_input_within_their_budgets() {
        let mut rng = Xorshift(0x9e37_79b9_7f4a_7c15);
        for _ in 0..20_000 {
            let text = rng.text(&HOSTILE_PIECES, 12);
            let budget = rng.below(24);
            let _ = mask_secrets(&text);
            let _ = sanitize_assistant_text(&text);
            let encoded = encode_terminal_safe(text.as_bytes(), budget);
            assert!(encoded.text.len() <= budget, "{text:?} {budget}");
            let unbounded = encode_terminal_safe(text.as_bytes(), usize::MAX).text;
            assert!(
                unbounded.chars().all(|character| !character.is_control()
                    && !is_non_printing_codepoint(u32::from(character))),
                "{unbounded:?}"
            );
        }
    }

    #[test]
    fn mask_secrets_stays_linear_on_adversarial_repetition() {
        for unit in [
            "a-",
            "token: ",
            "x://",
            "\"api_key\": \"$X\" ",
            "eyJ",
            "https://a:b",
            "--token ",
            "Bearer ",
            "TOKEN=\"$x ",
            "-H 'x-api-key: $X' ",
            "@",
            "'token:",
            "ICAg",
            "IHsi.",
        ] {
            let text = unit.repeat(10_000);
            let _ = mask_secrets(&text);
        }
    }

    #[test]
    fn utf8_forward_boundary_snaps_past_the_codepoint_tail() {
        let text = b"ab\xc3\xa9z";
        assert_eq!(utf8_forward_boundary(text, 0), 0);
        assert_eq!(utf8_forward_boundary(text, 2), 2);
        assert_eq!(utf8_forward_boundary(text, 3), 4);
        assert_eq!(utf8_forward_boundary(text, 4), 4);
        assert_eq!(utf8_forward_boundary(text, text.len() + 3), text.len());
    }

    #[test]
    fn utf8_backward_boundary_keeps_whole_codepoints() {
        let text = b"ab\xc3\xa9z";
        assert_eq!(utf8_backward_boundary(text, 3), 2);
        assert_eq!(utf8_backward_boundary(text, 4), 4);
        assert_eq!(utf8_backward_boundary(text, 9), 5);
    }

    #[test]
    fn head_tail_bounds_keep_both_ends_around_the_marker() {
        assert_eq!(write_head_tail_bounded(b"short", 5, "|"), b"short");
        assert_eq!(write_head_tail_bounded(b"abcdefghij", 6, "|"), b"abc|ij");
        assert_eq!(write_head_tail_bounded(b"abcdefghij", 7, "|"), b"abc|hij");
        assert_eq!(write_head_tail_bounded(b"abcdefghij", 2, "<->"), b"<-");
        assert_eq!(write_head_tail_bounded(b"abcdefghij", 3, "<->"), b"<->");
        let text = "\u{e9}".repeat(10);
        let bounded = write_head_tail_bounded(text.as_bytes(), 8, "|");
        assert_eq!(bounded, "\u{e9}\u{e9}|\u{e9}".as_bytes());
    }
}
