use ofx_text::display_unit_at;

pub(crate) fn next_character_end(text: &str, start: usize) -> usize {
    let mut end = start.min(text.len());
    if end == text.len() {
        return end;
    }
    end += display_unit_at(text, end).byte_len.max(1);
    if is_control_boundary(text, start) {
        return end.min(text.len());
    }
    while end < text.len() {
        let continuation = display_unit_at(text, end);
        if continuation.cell_width != 0 || is_control_boundary(text, end) {
            break;
        }
        end += continuation.byte_len.max(1);
    }
    end.min(text.len())
}

pub(crate) fn previous_character_start(text: &str, end: usize) -> usize {
    let target = end.min(text.len());
    if target == 0 {
        return 0;
    }
    let mut current = logical_line_start(text, target - 1);
    let mut previous = current;
    while current < target {
        previous = current;
        let next = next_character_end(text, current);
        if next >= target {
            return previous;
        }
        current = next;
    }
    previous
}

pub(crate) fn is_word_character(character: char) -> bool {
    character == '_' || character.is_ascii_alphanumeric() || u32::from(character) >= 0xc0
}

pub(crate) fn is_word_character_at(text: &str, index: usize) -> bool {
    char_at(text, index).is_some_and(is_word_character)
}

pub(crate) fn is_whitespace_character_at(text: &str, index: usize) -> bool {
    char_at(text, index)
        .is_some_and(|character| character.is_ascii_whitespace() || character == '\u{b}')
}

pub(crate) fn logical_line_start(text: &str, cursor: usize) -> usize {
    let start = cursor.min(text.len());
    text.as_bytes()[..start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1)
}

pub(crate) fn logical_line_end(text: &str, cursor: usize) -> usize {
    let end = cursor.min(text.len());
    text.as_bytes()[end..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(text.len(), |newline| end + newline)
}

pub(crate) fn previous_paragraph_start(text: &str, cursor: usize) -> usize {
    let mut line_start = logical_line_start(text, cursor);
    while line_is_blank(text, line_start) {
        let Some(previous) = previous_line_start(text, line_start) else {
            return 0;
        };
        line_start = previous;
    }

    let paragraph_start = paragraph_start_from(text, line_start);
    if cursor > paragraph_start {
        return paragraph_start;
    }

    let Some(mut previous) = previous_line_start(text, paragraph_start) else {
        return 0;
    };
    while line_is_blank(text, previous) {
        let Some(earlier) = previous_line_start(text, previous) else {
            return 0;
        };
        previous = earlier;
    }
    paragraph_start_from(text, previous)
}

pub(crate) fn next_paragraph_start(text: &str, cursor: usize) -> usize {
    let mut line_start = logical_line_start(text, cursor);
    while !line_is_blank(text, line_start) {
        let line_end = logical_line_end(text, line_start);
        if line_end == text.len() {
            return text.len();
        }
        line_start = line_end + 1;
    }
    while line_is_blank(text, line_start) {
        let line_end = logical_line_end(text, line_start);
        if line_end == text.len() {
            return text.len();
        }
        line_start = line_end + 1;
    }
    line_start
}

fn paragraph_start_from(text: &str, line_start: usize) -> usize {
    let mut paragraph_start = line_start;
    while let Some(candidate) = previous_line_start(text, paragraph_start) {
        if line_is_blank(text, candidate) {
            break;
        }
        paragraph_start = candidate;
    }
    paragraph_start
}

fn line_is_blank(text: &str, line_start: usize) -> bool {
    let line_end = logical_line_end(text, line_start);
    text.as_bytes()[line_start..line_end]
        .iter()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\r'))
}

fn previous_line_start(text: &str, line_start: usize) -> Option<usize> {
    (line_start > 0).then(|| logical_line_start(text, line_start - 1))
}

fn is_control_boundary(text: &str, start: usize) -> bool {
    char_at(text, start).is_some_and(|character| character < ' ' || character == '\u{7f}')
}

pub(crate) fn char_at(text: &str, index: usize) -> Option<char> {
    text.get(index..)?.chars().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_boundaries_group_terminal_character_continuations() {
        let decomposed = "Ae\u{301}B";
        assert_eq!(next_character_end(decomposed, 1), 4);
        assert_eq!(previous_character_start(decomposed, 4), 1);

        for character in ["🇺🇸", "👍🏽", "👨‍👩‍👧‍👦"] {
            let text = format!("A{character}B");
            assert_eq!(next_character_end(&text, 1), 1 + character.len());
            assert_eq!(previous_character_start(&text, 1 + character.len()), 1);
        }
    }

    #[test]
    fn character_boundaries_preserve_ascii_and_precomposed_utf8() {
        let text = "A\u{e9}B";
        assert_eq!(next_character_end(text, 0), 1);
        assert_eq!(next_character_end(text, 1), 3);
        assert_eq!(previous_character_start(text, 3), 1);
        assert_eq!(previous_character_start(text, text.len()), 3);
        assert_eq!(previous_character_start(text, 0), 0);
        assert_eq!(next_character_end(text, text.len() + 10), text.len());
    }

    #[test]
    fn control_bytes_remain_independent_character_boundaries() {
        let text = "A\n\u{301}\tB";
        assert_eq!(next_character_end(text, 0), 1);
        assert_eq!(next_character_end(text, 1), 2);
        assert_eq!(next_character_end(text, 2), 4);
        assert_eq!(next_character_end(text, 4), 5);
        assert_eq!(previous_character_start(text, 5), 4);
        assert_eq!(previous_character_start(text, 4), 2);
        assert_eq!(previous_character_start(text, 2), 1);
    }

    #[test]
    fn logical_line_boundaries_clamp_to_the_current_line() {
        let text = "alpha\nbeta\ngamma";
        assert_eq!(logical_line_start(text, 5), 0);
        assert_eq!(logical_line_start(text, 8), 6);
        assert_eq!(logical_line_end(text, 6), 10);
        assert_eq!(logical_line_end(text, text.len() + 10), text.len());
    }

    #[test]
    fn paragraph_boundaries_move_across_blank_line_delimited_blocks() {
        let text = "alpha\nbeta\n \t\ngamma\ndelta\n\nlast";
        let gamma = text.find("gamma").unwrap();
        let last = text.find("last").unwrap();
        assert_eq!(previous_paragraph_start(text, "alpha\nbeta".len()), 0);
        assert_eq!(previous_paragraph_start(text, gamma), 0);
        assert_eq!(next_paragraph_start(text, 0), gamma);
        assert_eq!(next_paragraph_start(text, gamma), last);
        assert_eq!(next_paragraph_start(text, last), text.len());
    }

    #[test]
    fn character_boundary_offsets_stay_ordered_and_in_bounds() {
        let cases = ["", "ascii", "e\u{301}", "🇺🇸", "👍🏽", "👨‍👩‍👧‍👦", "\u{301}\u{302}x"];
        for text in cases {
            let mut cursor = 0;
            while cursor < text.len() {
                let next = next_character_end(text, cursor);
                assert!(next > cursor);
                assert!(next <= text.len());
                assert_eq!(cursor, previous_character_start(text, next));
                cursor = next;
            }
            assert_eq!(cursor, text.len());
        }
    }

    #[test]
    fn word_characters_include_letters_digits_underscore_and_non_ascii() {
        assert!(is_word_character('a'));
        assert!(is_word_character('Z'));
        assert!(is_word_character('7'));
        assert!(is_word_character('_'));
        assert!(is_word_character('\u{e9}'));
        assert!(!is_word_character('-'));
        assert!(!is_word_character(' '));
        assert!(!is_word_character('\u{a9}'));
    }
}
