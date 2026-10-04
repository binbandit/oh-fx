use std::borrow::Cow;
use std::fmt::Write;

use ofx_text::{
    DisplayUnit, display_unit_at, encode_terminal_safe, is_terminal_control, prefix_by_width,
    suffix_by_width, visible_width,
};

use crate::render_engine::display_units::{Unit, display_units};

const SUMMARY_ELLIPSIS: &str = "\u{2026}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Color {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Attribute {
    Bold,
    Dim,
    Italic,
    Underline,
    Reverse,
    Strike,
}

impl Attribute {
    const ALL: [Self; 6] = [
        Self::Bold,
        Self::Dim,
        Self::Italic,
        Self::Underline,
        Self::Reverse,
        Self::Strike,
    ];

    const fn bit(self) -> u8 {
        match self {
            Self::Bold => 1,
            Self::Dim => 1 << 1,
            Self::Italic => 1 << 2,
            Self::Underline => 1 << 3,
            Self::Reverse => 1 << 4,
            Self::Strike => 1 << 5,
        }
    }

    const fn sgr(self) -> &'static str {
        match self {
            Self::Bold => "1",
            Self::Dim => "2",
            Self::Italic => "3",
            Self::Underline => "4",
            Self::Reverse => "7",
            Self::Strike => "9",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct Paint {
    pub(crate) fg: Option<Color>,
    bg: Option<Color>,
    attributes: u8,
}

impl Paint {
    pub(crate) const PLAIN: Self = Self {
        fg: None,
        bg: None,
        attributes: 0,
    };

    pub(crate) const fn fg(index: u8) -> Self {
        Self::colored(Some(Color::Indexed(index)))
    }

    pub(crate) const fn colored(fg: Option<Color>) -> Self {
        Self {
            fg,
            bg: None,
            attributes: 0,
        }
    }

    #[must_use]
    pub(crate) const fn on(self, index: u8) -> Self {
        Self {
            bg: Some(Color::Indexed(index)),
            ..self
        }
    }

    pub(crate) const fn bold_fg(index: u8) -> Self {
        Self::fg(index).with(Attribute::Bold)
    }

    #[must_use]
    pub(crate) const fn with(self, attribute: Attribute) -> Self {
        Self {
            attributes: self.attributes | attribute.bit(),
            ..self
        }
    }

    pub(crate) const fn has(self, attribute: Attribute) -> bool {
        self.attributes & attribute.bit() != 0
    }

    #[must_use]
    pub(crate) const fn with_bold(self) -> Self {
        self.with(Attribute::Bold)
    }

    #[must_use]
    pub(crate) const fn with_dim(self) -> Self {
        self.with(Attribute::Dim)
    }

    #[must_use]
    pub(crate) const fn with_reverse(self) -> Self {
        self.with(Attribute::Reverse)
    }

    fn write_sgr(self, out: &mut String) {
        out.push_str("\x1b[0");
        for attribute in Attribute::ALL {
            if self.has(attribute) {
                out.push(';');
                out.push_str(attribute.sgr());
            }
        }
        if let Some(color) = self.bg {
            write_color(out, 48, color);
        }
        if let Some(color) = self.fg {
            write_color(out, 38, color);
        }
        out.push('m');
    }
}

fn write_color(out: &mut String, base: u8, color: Color) {
    let _ = match color {
        Color::Indexed(index) => write!(out, ";{base};5;{index}"),
        Color::Rgb(red, green, blue) => write!(out, ";{base};2;{red};{green};{blue}"),
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) text: String,
    pub(crate) paint: Paint,
    pub(crate) link: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Row {
    segments: Vec<Segment>,
}

impl Row {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn plain(text: &str) -> Self {
        let mut row = Self::new();
        row.push(text, Paint::PLAIN);
        row
    }

    pub(crate) fn styled(text: &str, paint: Paint) -> Self {
        let mut row = Self::new();
        row.push(text, paint);
        row
    }

    pub(crate) fn push(&mut self, text: &str, paint: Paint) {
        self.push_linked(text, paint, None);
    }

    pub(crate) fn push_linked(&mut self, text: &str, paint: Paint, link: Option<&str>) {
        if text.is_empty() {
            return;
        }
        let text = terminal_safe(text);
        let link = link.map(terminal_safe);
        let link = link.as_deref();
        if let Some(last) = self.segments.last_mut()
            && last.paint == paint
            && last.link.as_deref() == link
        {
            last.text.push_str(&text);
            return;
        }
        self.segments.push(Segment {
            text: text.into_owned(),
            paint,
            link: link.map(str::to_owned),
        });
    }

    pub(crate) fn push_fmt(&mut self, arguments: std::fmt::Arguments<'_>, paint: Paint) {
        if !matches!(self.segments.last(), Some(last) if last.paint == paint && last.link.is_none())
        {
            self.segments.push(Segment {
                text: String::new(),
                paint,
                link: None,
            });
        }
        let Some(last) = self.segments.last_mut() else {
            return;
        };
        let start = last.text.len();
        let _ = last.text.write_fmt(arguments);
        if last.text[start..].contains(escaped_in_rows) {
            let safe = terminal_safe(&last.text[start..]).into_owned();
            last.text.truncate(start);
            last.text.push_str(&safe);
        }
        if last.text.is_empty() {
            self.segments.pop();
        }
    }

    pub(crate) fn push_spaces(&mut self, count: usize) {
        if count > 0 {
            self.push(&" ".repeat(count), Paint::PLAIN);
        }
    }

    pub(crate) fn pad_to_column(&mut self, column: usize) {
        self.push_spaces(column.saturating_sub(self.width()));
    }

    pub(crate) fn indent(&mut self, spaces: usize) {
        if spaces == 0 {
            return;
        }
        let padding = " ".repeat(spaces);
        match self.segments.first_mut() {
            Some(first) if first.paint == Paint::PLAIN && first.link.is_none() => {
                first.text.insert_str(0, &padding);
            }
            _ => self.segments.insert(
                0,
                Segment {
                    text: padding,
                    paint: Paint::PLAIN,
                    link: None,
                },
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub(crate) fn text_len(&self) -> usize {
        self.segments.iter().map(|segment| segment.text.len()).sum()
    }

    pub(crate) fn width(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| visible_width(&segment.text))
            .sum()
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.segments.iter().map(|segment| segment.text.len()).sum()
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        self.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }

    #[must_use]
    pub(crate) fn clipped(&self, max_width: usize) -> Self {
        let mut clipped = Self::new();
        let mut remaining = max_width;
        for segment in &self.segments {
            if remaining == 0 {
                break;
            }
            let prefix = prefix_by_width(&segment.text, remaining);
            clipped.push_linked(prefix, segment.paint, segment.link.as_deref());
            remaining -= visible_width(prefix);
            if prefix.len() < segment.text.len() {
                break;
            }
        }
        clipped
    }

    #[must_use]
    pub(crate) fn summary_clipped(&self, max_width: usize, lone_paint: Paint) -> Self {
        if self.width() <= max_width {
            return self.clone();
        }
        if max_width <= 1 {
            let mut lone = Self::new();
            if max_width == 1 {
                lone.push(SUMMARY_ELLIPSIS, lone_paint);
            }
            return lone;
        }
        let mut clipped = Self::new();
        let mut remaining = max_width - 1;
        let mut ellipsis = lone_paint;
        for segment in &self.segments {
            let prefix = prefix_by_width(&segment.text, remaining);
            clipped.push_linked(prefix, segment.paint, segment.link.as_deref());
            remaining -= visible_width(prefix);
            if prefix.len() < segment.text.len() {
                ellipsis = segment.paint;
                break;
            }
        }
        clipped.push(SUMMARY_ELLIPSIS, ellipsis);
        clipped
    }

    pub(crate) fn units(&self) -> Vec<Unit<'_>> {
        self.segments
            .iter()
            .flat_map(|segment| {
                display_units(&segment.text, segment.paint, segment.link.as_deref())
            })
            .collect()
    }

    pub(crate) fn from_units(units: &[Unit<'_>]) -> Self {
        let mut row = Self::new();
        for unit in units {
            row.push_linked(unit.text, unit.paint, unit.link);
        }
        row
    }

    pub(crate) fn wrapped(&self, width: usize) -> Vec<Self> {
        let mut rows = vec![Self::new()];
        let mut used = 0;
        for segment in &self.segments {
            let mut index = 0;
            while index < segment.text.len() {
                let unit = display_unit_at(&segment.text, index);
                let end = index + unit.byte_len.max(1);
                if used + unit.cell_width > width.max(1) && used > 0 {
                    rows.push(Self::new());
                    used = 0;
                }
                if let Some(row) = rows.last_mut() {
                    row.push_linked(
                        &segment.text[index..end],
                        segment.paint,
                        segment.link.as_deref(),
                    );
                }
                used += unit.cell_width;
                index = end;
            }
        }
        rows
    }

    pub(crate) fn encode(&self) -> String {
        let mut out = String::new();
        let mut open_link: Option<&str> = None;
        let mut current = Paint::PLAIN;
        for segment in &self.segments {
            if segment.link.as_deref() != open_link {
                if open_link.is_some() {
                    out.push_str("\x1b]8;;\x1b\\");
                }
                if let Some(target) = segment.link.as_deref() {
                    let _ = write!(out, "\x1b]8;{target}\x1b\\");
                }
                open_link = segment.link.as_deref();
            }
            if segment.paint != current {
                segment.paint.write_sgr(&mut out);
                current = segment.paint;
            }
            out.push_str(&segment.text);
        }
        if open_link.is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
        if current != Paint::PLAIN {
            out.push_str("\x1b[0m");
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EllipsisPlacement {
    Trailing,
    Middle,
    PrefixBiased,
}

impl EllipsisPlacement {
    pub(crate) const fn split(self, content_width: usize) -> (usize, usize) {
        match self {
            Self::Trailing => (content_width, 0),
            Self::Middle => (content_width.div_ceil(2), content_width / 2),
            Self::PrefixBiased => (content_width - content_width / 4, content_width / 4),
        }
    }
}

pub(crate) fn single_line_ellipsized(text: &str, width: usize) -> Cow<'_, str> {
    projected_single_line(text, width, EllipsisPlacement::Trailing)
}

pub(crate) fn single_line_middle_ellipsized(text: &str, width: usize) -> Cow<'_, str> {
    projected_single_line(text, width, EllipsisPlacement::Middle)
}

fn projected_single_line(text: &str, width: usize, placement: EllipsisPlacement) -> Cow<'_, str> {
    if width == 0 {
        return Cow::Borrowed("");
    }
    let line = if text.contains(['\n', '\r']) {
        Cow::Owned(text.replace(['\n', '\r'], " "))
    } else {
        Cow::Borrowed(text)
    };
    if visible_width(&line) <= width {
        return line;
    }
    if width == 1 {
        return Cow::Borrowed(SUMMARY_ELLIPSIS);
    }
    let (prefix_width, suffix_width) = placement.split(width - 1);
    Cow::Owned(format!(
        "{}{SUMMARY_ELLIPSIS}{}",
        prefix_by_width(&line, prefix_width),
        display_safe_suffix(&line, suffix_width)
    ))
}

pub(crate) fn display_safe_suffix(source: &str, width: usize) -> &str {
    let suffix = suffix_by_width(source, width);
    if suffix.is_empty() || suffix.len() == source.len() {
        return suffix;
    }
    let mut start = 0;
    while start < suffix.len() {
        let unit = display_unit_at(suffix, start);
        if unit.cell_width != 0 {
            break;
        }
        start += unit.byte_len;
    }
    suffix.get(start..).unwrap_or_default()
}

pub(crate) fn escaped_in_rows(character: char) -> bool {
    character.is_control() || is_terminal_control(character)
}

pub(crate) fn terminal_safe(text: &str) -> Cow<'_, str> {
    escape_where(text, escaped_in_rows)
}

pub(crate) fn terminal_safe_keeping_breaks(text: &str) -> Cow<'_, str> {
    escape_where(text, |character| {
        escaped_in_rows(character) && !matches!(character, '\n' | '\r')
    })
}

pub(crate) fn escaped_unit_at(text: &str, index: usize) -> DisplayUnit {
    let unit = display_unit_at(text, index);
    match text.get(index..index + unit.byte_len) {
        Some(drawn) if drawn.contains(escaped_in_rows) => DisplayUnit {
            byte_len: unit.byte_len,
            cell_width: visible_width(&terminal_safe(drawn)),
        },
        _ => unit,
    }
}

pub(crate) fn escaped_width(text: &str) -> usize {
    let mut width = 0;
    let mut index = 0;
    while index < text.len() {
        let unit = escaped_unit_at(text, index);
        width += unit.cell_width;
        index += unit.byte_len.max(1);
    }
    width
}

pub(crate) fn escaped_prefix_by_width(text: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }
    let mut width = 0;
    let mut index = 0;
    while index < text.len() {
        let unit = escaped_unit_at(text, index);
        if unit.cell_width > max_width - width {
            break;
        }
        width += unit.cell_width;
        index += unit.byte_len.max(1);
    }
    &text[..index]
}

fn escape_where(text: &str, escaped: impl Fn(char) -> bool) -> Cow<'_, str> {
    if !text.contains(&escaped) {
        return Cow::Borrowed(text);
    }
    let mut safe = String::with_capacity(text.len());
    let mut scalar = [0_u8; 4];
    for character in text.chars() {
        if escaped(character) {
            let encoded = character.encode_utf8(&mut scalar);
            safe.push_str(&encode_terminal_safe(encoded.as_bytes(), usize::MAX).text);
        } else {
            safe.push(character);
        }
    }
    Cow::Owned(safe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_text_with_equal_paint_merges() {
        let mut row = Row::new();
        row.push("ab", Paint::fg(245));
        row.push("cd", Paint::fg(245));
        row.push("ef", Paint::PLAIN);
        assert_eq!(row.segments().len(), 2);
        assert_eq!(row.text(), "abcdef");
        assert_eq!(row.width(), 6);
    }

    #[test]
    fn encoding_resets_styles_and_closes_links_at_the_row_end() {
        let mut row = Row::plain("go ");
        row.push_linked("docs", Paint::fg(75), Some(";https://x"));
        assert_eq!(
            row.encode(),
            "go \x1b]8;;https://x\x1b\\\x1b[0;38;5;75mdocs\x1b]8;;\x1b\\\x1b[0m"
        );
        assert_eq!(
            Row::styled("x", Paint::bold_fg(255).with_reverse()).encode(),
            "\x1b[0;1;7;38;5;255mx\x1b[0m"
        );
        assert_eq!(Row::plain("plain").encode(), "plain");
    }

    #[test]
    fn control_characters_never_reach_the_terminal_raw() {
        let mut row = Row::plain("mod\x1b]2;PWNED\x07 \u{9b}31m");
        row.push_linked("docs", Paint::PLAIN, Some(";https://x\x1b]0;T\x07y"));
        let encoded = row.encode();
        assert_eq!(row.text(), "mod\\x1b]2;PWNED\\x07 \\u{009b}31mdocs");
        assert_eq!(encoded.matches('\x1b').count(), 4);
        assert!(!encoded.contains('\x07'));
        assert!(!encoded.contains('\u{9b}'));
        assert!(encoded.contains("\x1b]8;;https://x\\x1b]0;T\\x07y\x1b\\"));
        assert_eq!(Row::plain("👨\u{200d}👩").text(), "👨\u{200d}👩");
    }

    #[test]
    fn bidi_reordering_controls_never_reach_the_terminal_raw() {
        let title = "Reading notes\u{202e}txt.hsab/hss./~ \u{2066}x\u{2069}\u{2028}";
        let row = Row::plain(title);
        assert_eq!(
            row.text(),
            "Reading notes\\u{202e}txt.hsab/hss./~ \\u{2066}x\\u{2069}\\u{2028}"
        );
        let encoded = row.encode();
        for control in ['\u{202e}', '\u{2066}', '\u{2069}', '\u{2028}'] {
            assert!(!encoded.contains(control), "{encoded:?}");
        }
        let mut formatted = Row::new();
        formatted.push_fmt(format_args!("{title}"), Paint::PLAIN);
        assert_eq!(formatted.text(), row.text());
        assert_eq!(Row::plain(&row.text()).text(), row.text());
    }

    #[test]
    fn formatted_text_merges_like_pushed_text_and_stays_terminal_safe() {
        let mut row = Row::plain("a");
        row.push_fmt(format_args!("{}{}", 1, "\x07"), Paint::PLAIN);
        row.push_fmt(format_args!(""), Paint::fg(2));
        assert_eq!(row.segments().len(), 1);
        assert_eq!(row.text(), "a1\\x07");
    }

    #[test]
    fn indenting_prepends_plain_spaces_without_splitting_plain_text() {
        let mut plain = Row::plain("text");
        plain.indent(2);
        assert_eq!(plain.segments().len(), 1);
        assert_eq!(plain.text(), "  text");
        let mut styled = Row::styled("text", Paint::fg(1));
        styled.indent(1);
        styled.indent(0);
        assert_eq!(styled.segments().len(), 2);
        assert_eq!(styled.text(), " text");
        assert_eq!(styled.width(), 5);
    }

    #[test]
    fn escaped_widths_measure_text_as_rows_draw_it() {
        let text = "a\u{202e}é\u{85}b\u{1}";
        assert_eq!(escaped_width(text), Row::plain(text).width());
        assert_eq!(escaped_unit_at(text, 1).cell_width, 8);
        assert_eq!(escaped_unit_at(text, 1).byte_len, 3);
        assert_eq!(escaped_unit_at(text, 0), display_unit_at(text, 0));
        assert_eq!(escaped_prefix_by_width("ab\u{202e}c", 9), "ab");
        assert_eq!(escaped_prefix_by_width("ab\u{202e}c", 10), "ab\u{202e}");
        assert_eq!(escaped_prefix_by_width("ab\u{202e}c", 0), "");
        assert_eq!(escaped_width("plain é"), visible_width("plain é"));
    }

    #[test]
    fn footer_tiny_widths_clip_prefixes_and_suppress_content_units() {
        let input = Row::plain("界\u{301}\n[Image #5]");
        assert_eq!(input.clipped(0).text(), "");
        assert_eq!(input.clipped(2).text(), "界\u{301}");
    }

    #[test]
    fn footer_clipping_never_splits_unicode_display_units() {
        let emoji = "\u{1F469}\u{200D}\u{1F4BB}";
        assert_eq!(Row::plain(emoji).clipped(1).text(), "");
        assert_eq!(Row::plain(emoji).clipped(2).text(), emoji);
        let text_presentation = "\u{2600}\u{FE0E}";
        assert_eq!(
            Row::plain(text_presentation).clipped(1).text(),
            text_presentation
        );
    }

    #[test]
    fn padding_to_a_column_stops_at_the_rows_current_width() {
        let mut row = Row::plain("a界");
        row.pad_to_column(6);
        assert_eq!(row.text(), "a界   ");
        row.pad_to_column(4);
        assert_eq!(row.width(), 6);
    }

    #[test]
    fn single_line_ellipsis_projections_preserve_semantic_tails() {
        assert_eq!(single_line_ellipsized("abcdef", 4), "abc…");
        assert_eq!(single_line_middle_ellipsized("abcdef", 5), "ab…ef");
        assert_eq!(
            projected_single_line("abcdefghijkl", 7, EllipsisPlacement::PrefixBiased),
            "abcde…l"
        );
        assert_eq!(single_line_middle_ellipsized("a\nb", 3), "a b");
        assert_eq!(single_line_middle_ellipsized("界a", 1), "…");
        let unicode = single_line_middle_ellipsized("界abcdef", 5);
        assert_eq!(unicode, "界…ef");
        assert_eq!(visible_width(&unicode), 5);
        assert!(single_line_middle_ellipsized("abcdef", 0).is_empty());
    }

    #[test]
    fn a_shortened_tail_never_starts_with_a_zero_width_mark() {
        assert_eq!(
            single_line_middle_ellipsized("abcdefgh\u{301}ij", 6),
            "abc…ij"
        );
        assert_eq!(
            projected_single_line("zzabcd\u{301}e", 6, EllipsisPlacement::PrefixBiased),
            "zzab…e"
        );
    }

    #[test]
    fn clipping_keeps_whole_display_units() {
        let mut row = Row::plain("a界");
        row.push("bc", Paint::fg(1));
        assert_eq!(row.clipped(2).text(), "a");
        assert_eq!(row.clipped(4).text(), "a界b");
        assert_eq!(row.clipped(0).text(), "");
    }
}
