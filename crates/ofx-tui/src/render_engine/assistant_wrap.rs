use ofx_markdown::{Hang, Line};
use ofx_text::{next_tab_stop_column, should_wrap_at};

use super::display_units::{Unit, display_units};
use crate::row_text::{Paint, Row};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy)]
struct WordBreak {
    start: usize,
    end: usize,
    has_preceding_word: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct WordBreaks {
    last: Option<WordBreak>,
    previous: Option<WordBreak>,
}

impl WordBreaks {
    fn record(&mut self, word_break: WordBreak) {
        self.previous = self.last;
        self.last = Some(word_break);
    }
}

pub(crate) fn gutter_width(cols: usize) -> usize {
    match cols {
        0 | 1 => 0,
        2 => 1,
        _ => 2,
    }
}

pub(crate) fn wrap_line(line: &Line, cols: usize, base_gutter: usize, theme: &Theme) -> Vec<Row> {
    if cols == 0 {
        return Vec::new();
    }
    let targets: Vec<Option<String>> = line
        .spans
        .iter()
        .map(|span| {
            span.link
                .as_ref()
                .map(|link| format!("id=fx-{};{}", link.id, link.url))
        })
        .collect();
    let units = line_units(line, &targets, theme);
    if units.is_empty() {
        return vec![Row::new()];
    }
    let mut wrapper = Wrapper {
        cols,
        base_gutter,
        hang: line.hang,
        dim: theme.dim,
        rows: Vec::new(),
        current: Vec::new(),
        prefix: Row::new(),
        col: base_gutter + 1,
        word_breaks: WordBreaks::default(),
        has_word_on_row: false,
    };
    wrapper.prefix.push_spaces(base_gutter);
    wrapper.run(&units);
    wrapper.finish()
}

fn line_units<'a>(line: &'a Line, targets: &'a [Option<String>], theme: &Theme) -> Vec<Unit<'a>> {
    line.spans
        .iter()
        .zip(targets)
        .flat_map(|(span, target)| {
            display_units(&span.text, theme.markdown(span.style), target.as_deref())
        })
        .filter(|unit| unit.text == "\t" || !unit.text.chars().all(char::is_control))
        .collect()
}

struct Wrapper<'a> {
    cols: usize,
    base_gutter: usize,
    hang: Hang,
    dim: Paint,
    rows: Vec<Row>,
    current: Vec<Unit<'a>>,
    prefix: Row,
    col: usize,
    word_breaks: WordBreaks,
    has_word_on_row: bool,
}

impl<'a> Wrapper<'a> {
    fn run(&mut self, units: &[Unit<'a>]) {
        let mut index = 0;
        while index < units.len() {
            let unit = units[index];
            if unit.text == "\t" {
                self.push_tab(unit);
                index += 1;
                continue;
            }
            if unit.width == 0 {
                self.current.push(unit);
                index += 1;
                continue;
            }
            let width = unit.width;
            if unit.is_space() && self.overflows(width) {
                let moved = self.avoid_orphan_at_space(units, index + 1, width);
                if !moved {
                    self.start_continuation(width);
                    index += 1;
                    continue;
                }
            }
            if self.overflows(width) {
                self.reflow_before_overflow(units, index, width);
            }
            if self.overflows(width) && !self.current.is_empty() {
                self.start_continuation(width);
            }
            self.current.push(unit);
            self.col += width;
            if unit.is_space() && self.has_word_on_row && !self.is_marker_separator() {
                let end = self.current.len();
                let has_preceding_word = self.word_breaks.last.is_some();
                self.word_breaks.record(WordBreak {
                    start: end - 1,
                    end,
                    has_preceding_word,
                });
            } else if !unit.is_space() {
                self.has_word_on_row = true;
            }
            index += 1;
        }
    }

    fn overflows(&self, width: usize) -> bool {
        should_wrap_at(clamp_u16(self.col), clamp_u16(width), clamp_u16(self.cols))
    }

    fn push_tab(&mut self, unit: Unit<'a>) {
        let next = usize::from(next_tab_stop_column(
            clamp_u16(self.col.max(1)),
            clamp_u16(self.cols),
        ));
        let spaces = next.saturating_sub(self.col);
        let space = Unit {
            text: " ",
            width: 1,
            ..unit
        };
        self.current.extend(std::iter::repeat_n(space, spaces));
        self.col = self.col.max(next);
    }

    fn avoid_orphan_at_space(
        &mut self,
        units: &[Unit<'a>],
        next_word: usize,
        width: usize,
    ) -> bool {
        let Some(word_break) = self.word_breaks.last else {
            return false;
        };
        if remaining_word_width(units, next_word) == 0
            || !is_final_word_on_line(units, next_word)
            || !self.word_break_fits(word_break, units, next_word)
        {
            return false;
        }
        let next_width = first_visible_width(&units[next_word..]).unwrap_or(width);
        if !self.reflow_at(word_break, next_width) {
            return false;
        }
        self.has_word_on_row = true;
        true
    }

    fn reflow_before_overflow(&mut self, units: &[Unit<'a>], index: usize, width: usize) {
        let Some(last) = self.word_breaks.last else {
            return;
        };
        let mut break_to_use = last;
        if let Some(previous) = self.word_breaks.previous
            && previous.has_preceding_word
            && is_final_word_on_line(units, index)
            && self.word_break_fits(previous, units, index)
        {
            break_to_use = previous;
        }
        self.reflow_at(break_to_use, width);
    }

    fn word_break_fits(
        &self,
        word_break: WordBreak,
        units: &[Unit<'a>],
        word_start: usize,
    ) -> bool {
        let indent = self.continuation_indent();
        if indent >= self.cols {
            return false;
        }
        units_width(&self.current[word_break.end..]) + remaining_word_width(units, word_start)
            <= self.cols - indent
    }

    fn reflow_at(&mut self, word_break: WordBreak, next_width: usize) -> bool {
        let suffix_width = units_width(&self.current[word_break.end..]);
        let first_width =
            first_visible_width(&self.current[word_break.end..]).unwrap_or(next_width);
        let indent = self.continuation_indent();
        if indent >= self.cols || suffix_width + next_width > self.cols - indent {
            return false;
        }
        let suffix = self.current.split_off(word_break.end);
        self.current.truncate(word_break.start);
        self.start_continuation(first_width);
        self.col += suffix_width;
        self.current.extend(suffix);
        true
    }

    fn start_continuation(&mut self, next_width: usize) {
        self.flush_row();
        self.prefix = Row::new();
        self.prefix.push_spaces(self.base_gutter);
        self.col = self.base_gutter + 1;
        let indent = self.hang.width();
        let total_indent = self.base_gutter + indent;
        if indent > 0 && total_indent < self.cols && next_width <= self.cols - total_indent {
            match self.hang {
                Hang::None | Hang::Indent(_) => self.prefix.push_spaces(indent),
                Hang::Quote { indent, depth } => {
                    self.prefix.push_spaces(indent);
                    for _ in 0..depth {
                        self.prefix.push("│ ", self.dim);
                    }
                }
            }
            self.col = total_indent + 1;
        }
        self.word_breaks = WordBreaks::default();
        self.has_word_on_row = false;
    }

    fn continuation_indent(&self) -> usize {
        self.base_gutter + self.hang.width()
    }

    fn is_marker_separator(&self) -> bool {
        let indent = self.hang.width();
        indent > 0 && self.col - 1 == self.base_gutter + indent
    }

    fn flush_row(&mut self) {
        let mut row = std::mem::take(&mut self.prefix);
        for unit in self.current.drain(..) {
            row.push_linked(unit.text, unit.paint, unit.link);
        }
        self.rows.push(row);
    }

    fn finish(mut self) -> Vec<Row> {
        self.flush_row();
        self.rows
    }
}

fn units_width(units: &[Unit<'_>]) -> usize {
    units.iter().map(|unit| unit.width).sum()
}

fn first_visible_width(units: &[Unit<'_>]) -> Option<usize> {
    units
        .iter()
        .find(|unit| unit.width > 0)
        .map(|unit| unit.width)
}

fn remaining_word_width(units: &[Unit<'_>], start: usize) -> usize {
    units[start.min(units.len())..]
        .iter()
        .take_while(|unit| !unit.is_space())
        .map(|unit| unit.width)
        .sum()
}

fn is_final_word_on_line(units: &[Unit<'_>], start: usize) -> bool {
    let mut passed_current_word = false;
    for unit in &units[start.min(units.len())..] {
        if unit.is_space() {
            passed_current_word = true;
        } else if passed_current_word {
            return false;
        }
    }
    true
}

fn clamp_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use ofx_markdown::{Attr, Span, Style};

    use super::*;

    fn theme() -> Theme {
        Theme::builtin(false, false, true)
    }

    fn line(text: &str, hang: Hang) -> Line {
        Line {
            spans: vec![Span {
                text: text.to_owned(),
                style: Style::default(),
                link: None,
            }],
            hang,
            newline: true,
        }
    }

    fn wrapped(text: &str, hang: Hang, cols: usize) -> Vec<String> {
        wrap_line(&line(text, hang), cols, 0, &theme())
            .iter()
            .map(Row::text)
            .collect()
    }

    #[test]
    fn wrap_assistant_text_preserves_explicit_paragraph_indentation() {
        assert_eq!(wrapped("1. Heading", Hang::None, 16), ["1. Heading"]);
        assert_eq!(
            wrapped("   alpha beta gamma delta", Hang::Indent(3), 16),
            ["   alpha beta", "   gamma delta"]
        );
        assert_eq!(wrapped("", Hang::None, 16), [""]);
        assert_eq!(
            wrapped("  alpha beta gamma delta", Hang::Indent(2), 16),
            ["  alpha beta", "  gamma delta"]
        );
        assert_eq!(
            wrapped("alpha beta gamma delta", Hang::None, 16),
            ["alpha beta", "gamma delta"]
        );
    }

    #[test]
    fn wrap_assistant_text_explicit_indentation_respects_narrow_widths_and_wide_units() {
        assert_eq!(
            wrapped("   abcdef", Hang::Indent(3), 3),
            ["   ", "abc", "def"]
        );
        assert_eq!(
            wrapped("  界界界", Hang::Indent(2), 4),
            ["  界", "  界", "  界"]
        );
    }

    #[test]
    fn wrap_assistant_text_wraps_plain_ascii_at_cols() {
        assert_eq!(wrapped("abcdefghij", Hang::None, 5), ["abcde", "fghij"]);
    }

    #[test]
    fn wrap_assistant_text_keeps_paragraph_words_together_and_avoids_a_final_orphan() {
        assert_eq!(
            wrapped("alpha beta gamma delta", Hang::None, 16),
            ["alpha beta", "gamma delta"]
        );
    }

    #[test]
    fn wrap_assistant_text_avoids_an_orphan_after_a_fitting_separator() {
        assert_eq!(
            wrapped("aaaa bbbb cccc dddd", Hang::None, 16),
            ["aaaa bbbb", "cccc dddd"]
        );
    }

    #[test]
    fn wrap_assistant_text_keeps_word_aware_list_continuations_aligned() {
        assert_eq!(
            wrapped("• alpha beta gamma delta", Hang::Indent(2), 18),
            ["• alpha beta", "  gamma delta"]
        );
    }

    #[test]
    fn wrap_assistant_text_repeats_every_styled_nested_blockquote_rule_on_word_aware_continuations()
    {
        let quote = Hang::Quote {
            indent: 0,
            depth: 2,
        };
        assert_eq!(
            wrapped("│ │ alpha beta gamma delta", quote, 18),
            ["│ │ alpha beta", "│ │ gamma delta"]
        );
        let rows = wrap_line(&line("│ │ abcdefghijkl", quote), 9, 0, &theme());
        let texts: Vec<String> = rows.iter().map(Row::text).collect();
        assert_eq!(texts, ["│ │ abcde", "│ │ fghij", "│ │ kl"]);
        assert_eq!(rows[1].segments()[0].paint, Paint::fg(245));
    }

    #[test]
    fn wrap_assistant_text_preserves_bold_sgr_across_wrap_boundary() {
        let bold = Line {
            spans: vec![Span {
                text: "abcdef".to_owned(),
                style: Style::default().with(Attr::Bold),
                link: None,
            }],
            hang: Hang::None,
            newline: true,
        };
        let rows = wrap_line(&bold, 3, 0, &theme());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| {
            row.segments()[0]
                .paint
                .has(crate::row_text::Attribute::Bold)
        }));
    }

    #[test]
    fn wrap_assistant_text_with_cols_0_returns_empty() {
        assert!(wrapped("foo", Hang::None, 0).is_empty());
    }

    #[test]
    fn wrap_assistant_text_indents_unordered_list_continuations() {
        assert_eq!(
            wrapped("• abcdefghijkl", Hang::Indent(2), 7),
            ["• abcde", "  fghij", "  kl"]
        );
    }

    #[test]
    fn wrap_assistant_text_indents_paren_style_ordered_list_continuations() {
        assert_eq!(
            wrapped("12) abcdefghij", Hang::Indent(4), 10),
            ["12) abcdef", "    ghij"]
        );
    }

    #[test]
    fn wrap_assistant_text_indents_ordered_and_nested_list_continuations() {
        assert_eq!(
            wrapped("  12. abcdefghij", Hang::Indent(6), 10),
            ["  12. abcd", "      efgh", "      ij"]
        );
        assert_eq!(
            wrapped("  • abcdefgh", Hang::Indent(4), 8),
            ["  • abcd", "    efgh"]
        );
    }

    #[test]
    fn wrap_assistant_text_keeps_narrow_list_rows_within_terminal_width() {
        for (text, narrowest) in [("• abcdefghijkl", 1), ("• 漢字abcdef", 2)] {
            for cols in narrowest..=5 {
                for row in wrap_line(&line(text, Hang::Indent(2)), cols, 0, &theme()) {
                    assert!(row.width() <= cols, "{cols} {:?}", row.text());
                }
            }
        }
    }

    #[test]
    fn transcript_rows_carry_the_assistant_gutter() {
        let rows = wrap_line(
            &line(
                "Hello from the fake gateway. This is a plain reply.",
                Hang::None,
            ),
            40,
            gutter_width(40),
            &theme(),
        );
        let texts: Vec<String> = rows.iter().map(Row::text).collect();
        assert_eq!(
            texts,
            ["  Hello from the fake gateway. This is a", "  plain reply."]
        );
        assert_eq!(gutter_width(2), 1);
        assert_eq!(gutter_width(1), 0);
    }
}
