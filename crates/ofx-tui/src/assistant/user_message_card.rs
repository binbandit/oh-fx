use ofx_text::{display_unit_at, prefix_by_width};

use crate::row_text::{Paint, Row, terminal_safe};
use crate::theme::Theme;

const USER_TURN_RAIL: &str = "┃";

struct WrapCut {
    keep_bytes: usize,
    skip_bytes: usize,
}

fn wrap_cut(text: &str, row_budget: usize) -> WrapCut {
    if row_budget == 0 {
        return WrapCut {
            keep_bytes: 0,
            skip_bytes: 0,
        };
    }
    let prefix = prefix_by_width(text, row_budget);
    if prefix.len() == text.len() {
        return WrapCut {
            keep_bytes: prefix.len(),
            skip_bytes: prefix.len(),
        };
    }
    if let Some(space) = prefix.rfind([' ', '\t']) {
        return WrapCut {
            keep_bytes: space,
            skip_bytes: space + 1,
        };
    }
    if prefix.is_empty() {
        let unit = display_unit_at(text, 0).byte_len.max(1);
        return WrapCut {
            keep_bytes: unit,
            skip_bytes: unit,
        };
    }
    WrapCut {
        keep_bytes: prefix.len(),
        skip_bytes: prefix.len(),
    }
}

fn row_with_prefix(theme: &Theme, fragment: &str) -> Row {
    let mut row = Row::styled(USER_TURN_RAIL, theme.user_card_marker);
    row.push(" ", Paint::PLAIN);
    row.push(fragment, Paint::PLAIN.with_bold());
    row
}

pub(crate) fn user_prompt_card(text: &str, cols: usize, theme: &Theme) -> Vec<Row> {
    let window = cols.saturating_sub(2);
    if window == 0 || text.is_empty() || cols <= 2 {
        return Vec::new();
    }
    let display: String = text
        .chars()
        .map(|character| if character == '\t' { ' ' } else { character })
        .filter(|character| *character == '\n' || !character.is_control())
        .collect();
    let mut rows = Vec::new();
    for line in display.split('\n') {
        if line.is_empty() {
            rows.push(row_with_prefix(theme, ""));
            continue;
        }
        let line = terminal_safe(line);
        let mut remaining = line.as_ref();
        while !remaining.is_empty() {
            let cut = wrap_cut(remaining, window);
            rows.push(row_with_prefix(theme, &remaining[..cut.keep_bytes]));
            remaining = &remaining[cut.skip_bytes.min(remaining.len())..];
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::builtin(false, false, true)
    }

    fn texts(text: &str, cols: usize) -> Vec<String> {
        user_prompt_card(text, cols, &theme())
            .iter()
            .map(Row::text)
            .collect()
    }

    #[test]
    fn current_presentation_removes_the_card_background_and_accents_only_the_rail() {
        let card = user_prompt_card("hello", 80, &theme());
        assert_eq!(
            card[0].encode(),
            "\x1b[0;38;5;255m┃\x1b[0m \x1b[0;1mhello\x1b[0m"
        );
    }

    #[test]
    fn prompts_wrap_at_the_width_of_the_escapes_their_rows_show() {
        let rows = user_prompt_card("abc\u{202e}defghijkl\nnext", 16, &theme());
        assert!(rows.iter().all(|row| row.width() <= 16), "{rows:?}");
        let shown: Vec<String> = rows
            .iter()
            .map(|row| row.text().trim_start_matches("┃ ").to_owned())
            .collect();
        assert_eq!(shown.concat(), "abc\\u{202e}defghijklnext");
        assert_eq!(shown.last().map(String::as_str), Some("next"));
    }

    #[test]
    fn current_presentation_uses_a_connected_rail_on_every_prompt_row() {
        assert_eq!(
            texts("line one\nline two", 80),
            ["┃ line one", "┃ line two"]
        );
    }

    #[test]
    fn current_presentation_uses_a_theme_aware_prompt_rail() {
        let light = Theme::builtin(true, false, true);
        let card = user_prompt_card("hello", 80, &light);
        assert_eq!(card[0].segments()[0].paint, Paint::fg(235));
    }

    #[test]
    fn build_user_prompt_card_row_content_width_hugs_message() {
        let card = user_prompt_card("hello", 80, &theme());
        assert_eq!(card[0].width(), 7);
    }

    #[test]
    fn build_user_prompt_card_handles_degenerate_cols() {
        assert!(texts("anything", 1).is_empty());
    }

    #[test]
    fn build_user_prompt_card_handles_empty_input() {
        assert!(texts("", 80).is_empty());
    }

    #[test]
    fn build_user_prompt_card_wraps_on_word_boundary() {
        let rows = user_prompt_card("the quick brown fox jumps over the lazy dog", 20, &theme());
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|row| row.width() <= 20));
        assert_eq!(rows[0].text(), "┃ the quick brown");
    }

    #[test]
    fn build_user_prompt_card_cuts_within_wide_char_grapheme_by_display_width() {
        let rows = user_prompt_card("a😀b", 5, &theme());
        assert!(rows.iter().all(|row| row.width() <= 5));
        assert!(rows.iter().any(|row| row.text().contains('😀')));
    }

    #[test]
    fn build_user_prompt_card_terminates_when_a_wide_glyph_exceeds_window() {
        assert!(!texts("😀", 3).is_empty());
    }

    #[test]
    fn wrapped_rows_match_the_upstream_capture() {
        assert_eq!(
            texts(
                "line one\nline two is quite a bit longer than the width",
                40
            ),
            [
                "┃ line one",
                "┃ line two is quite a bit longer than",
                "┃ the width"
            ]
        );
        assert_eq!(texts("a\n\nb", 40), ["┃ a", "┃ ", "┃ b"]);
    }
}
