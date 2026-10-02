use super::Composer;
use super::composer_replacement::EditRange;
use super::registered_entities::Entities;
use super::text_boundaries::{
    char_at, is_word_character, is_word_character_at, next_character_end, previous_character_start,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeletionKind {
    CharacterLeft,
    CharacterRight,
    WordLeft,
    WordRight,
}

impl Composer {
    pub(crate) fn delete(&mut self, kind: DeletionKind) -> bool {
        self.vertical.reset();
        if self.delete_selection() {
            return true;
        }
        let Some(range) = deletion_range(&self.edit.input, self.edit.cursor, &self.entities, kind)
        else {
            return false;
        };
        self.delete_range(range, range.start)
    }
}

fn deletion_range(
    input: &str,
    cursor: usize,
    entities: &Entities,
    kind: DeletionKind,
) -> Option<EditRange> {
    match kind {
        DeletionKind::CharacterLeft => character_left_range(input, cursor, entities),
        DeletionKind::CharacterRight => character_right_range(input, cursor, entities),
        DeletionKind::WordLeft => word_left_range(input, cursor, entities),
        DeletionKind::WordRight => word_right_range(input, cursor, entities),
    }
}

fn entity_range(start: usize, end: usize) -> EditRange {
    EditRange { start, end }
}

fn character_left_range(input: &str, cursor: usize, entities: &Entities) -> Option<EditRange> {
    if cursor == 0 {
        return None;
    }
    if let Some(entity) = entities.entity_ending_at_or_containing(input, cursor) {
        return Some(entity_range(entity.span.raw_start, entity.span.raw_end));
    }
    Some(entity_range(
        previous_character_start(input, cursor),
        cursor,
    ))
}

fn character_right_range(input: &str, cursor: usize, entities: &Entities) -> Option<EditRange> {
    if cursor >= input.len() {
        return None;
    }
    if let Some(entity) = entities.entity_starting_at_or_containing(input, cursor) {
        return Some(entity_range(entity.span.raw_start, entity.span.raw_end));
    }
    Some(entity_range(cursor, next_character_end(input, cursor)))
}

fn word_left_range(input: &str, cursor: usize, entities: &Entities) -> Option<EditRange> {
    if cursor == 0 {
        return None;
    }
    if let Some(entity) = entities.entity_ending_at_or_containing(input, cursor) {
        return Some(entity_range(entity.span.raw_start, entity.span.raw_end));
    }
    let start = scan_left(input, cursor, entities, true);
    let start = scan_left(input, start, entities, false);
    (start != cursor).then(|| entity_range(start, cursor))
}

fn scan_left(input: &str, from: usize, entities: &Entities, stop_at_word: bool) -> usize {
    let mut start = from;
    while start > 0 {
        if let Some(entity) = entities.entity_ending_at(input, start) {
            return entity.span.raw_start;
        }
        let previous = previous_character_start(input, start);
        if is_word_character_at(input, previous) == stop_at_word {
            break;
        }
        start = previous;
    }
    start
}

fn word_right_range(input: &str, cursor: usize, entities: &Entities) -> Option<EditRange> {
    if cursor >= input.len() {
        return None;
    }
    if let Some(entity) = entities.entity_starting_at_or_containing(input, cursor) {
        return Some(entity_range(entity.span.raw_start, entity.span.raw_end));
    }
    let mut end = cursor;
    while let Some(character) = char_at(input, end) {
        if !is_word_character(character) {
            break;
        }
        end += character.len_utf8();
    }
    while let Some(character) = char_at(input, end) {
        if entities.entity_starting_at(input, end).is_some() || is_word_character(character) {
            break;
        }
        end += character.len_utf8();
    }
    (end != cursor).then(|| entity_range(cursor, end))
}

#[cfg(test)]
mod tests {
    use super::super::entity_spans::Span;
    use super::super::pasted_blocks::PastedBlock;
    use super::super::test_fixture::{replace_text, select};
    use super::*;

    fn composer_with(text: &str, cursor: usize) -> Composer {
        let mut composer = Composer::new();
        replace_text(&mut composer, text);
        composer.edit.set_cursor(cursor);
        composer
    }

    fn register_paste(composer: &mut Composer, id: usize, start: usize, backing: &str) {
        let placeholder = super::super::pasted_blocks::format_placeholder(id, 1);
        composer.entities.register_pasted_block(PastedBlock {
            id,
            text: backing.to_owned(),
            line_count: 1,
            span: Span::new(start, start + placeholder.len()),
        });
    }

    #[test]
    fn character_and_word_deletion_use_terminal_independent_ranges() {
        let mut composer = composer_with("Ae\u{301}B", 1);
        assert!(composer.delete(DeletionKind::CharacterRight));
        assert_eq!(composer.text(), "AB");
        assert_eq!(composer.cursor(), 1);

        let mut composer = composer_with("foo-bar  ", "foo-bar  ".len());
        assert!(composer.delete(DeletionKind::WordLeft));
        assert_eq!(composer.text(), "foo-");
        assert_eq!(composer.edit_history.peek_undo().unwrap().removed, "bar  ");
    }

    #[test]
    fn backspace_removes_a_lone_skin_tone_modifier_on_its_own() {
        let mut composer = Composer::new();
        composer.insert_text("ab\u{1f3fd}", usize::MAX);
        assert!(composer.delete(DeletionKind::CharacterLeft));
        assert_eq!(composer.text(), "ab");
    }

    #[test]
    fn selection_deletion_wins_and_picker_policy_respects_empty_boundaries() {
        let mut composer = composer_with("alpha beta", 10);
        select(&mut composer, 2, 7);
        composer.vertical.preferred_column = Some(4);
        assert!(composer.delete(DeletionKind::WordRight));
        assert_eq!(composer.text(), "aleta");
        assert_eq!(composer.selection(), None);
        assert_eq!(composer.preferred_column(), None);

        composer.edit.set_cursor(0);
        assert!(!composer.delete(DeletionKind::CharacterLeft));
        assert!(composer.delete(DeletionKind::CharacterRight));
        assert_eq!(composer.text(), "leta");
    }

    #[test]
    fn backspace_and_forward_delete_remove_input_selection_first() {
        for kind in [DeletionKind::CharacterLeft, DeletionKind::CharacterRight] {
            let mut composer = Composer::new();
            replace_text(&mut composer, "abcdef");
            select(&mut composer, 2, 5);
            assert!(composer.delete(kind));
            assert_eq!(composer.text(), "abf");
            assert_eq!(composer.cursor(), 2);
        }
    }

    #[test]
    fn backspace_removes_full_utf8_rune_from_input() {
        let mut composer = composer_with("a😀", "a😀".len());
        assert!(composer.delete(DeletionKind::CharacterLeft));
        assert_eq!(composer.text(), "a");
    }

    #[test]
    fn delete_word_left_removes_preceding_word_from_input() {
        let mut composer = composer_with("hello world foo", "hello world foo".len());
        assert!(composer.delete(DeletionKind::WordLeft));
        assert_eq!(composer.text(), "hello world ");
        assert_eq!(composer.cursor(), "hello world ".len());
        assert!(composer.delete(DeletionKind::WordLeft));
        assert_eq!(composer.text(), "hello ");
        assert!(composer.delete(DeletionKind::WordLeft));
        assert_eq!(composer.text(), "");
        assert!(!composer.delete(DeletionKind::WordLeft));
    }

    #[test]
    fn delete_word_left_preserves_punctuation_boundaries() {
        let mut composer = composer_with("foo-bar", "foo-bar".len());
        assert!(composer.delete(DeletionKind::WordLeft));
        assert_eq!(composer.text(), "foo-");
    }

    #[test]
    fn delete_word_right_removes_the_next_word_from_input() {
        let mut composer = composer_with("hello world foo", 0);
        assert!(composer.delete(DeletionKind::WordRight));
        assert_eq!(composer.text(), "world foo");
        assert_eq!(composer.cursor(), 0);
        assert!(composer.delete(DeletionKind::WordRight));
        assert_eq!(composer.text(), "foo");
        assert!(composer.delete(DeletionKind::WordRight));
        assert_eq!(composer.text(), "");
        assert!(!composer.delete(DeletionKind::WordRight));
    }

    #[test]
    fn registered_paste_skill_and_image_spans_are_atomic_editor_boundaries() {
        let placeholder = "[Pasted text #7, 1 line]";
        let text = format!("A{placeholder}B");
        let mut composer = composer_with(&text, 1 + placeholder.len());
        register_paste(&mut composer, 7, 1, "pasted");

        composer.move_cursor(crate::input::MoveIntent::new(
            crate::input::MoveKind::CharacterLeft,
        ));
        assert_eq!(composer.cursor(), 1);
        composer.move_cursor(crate::input::MoveIntent::new(
            crate::input::MoveKind::CharacterRight,
        ));
        assert_eq!(composer.cursor(), 1 + placeholder.len());

        composer.edit.set_cursor(placeholder.len());
        assert!(composer.delete(DeletionKind::CharacterLeft));
        assert_eq!(composer.text(), "AB");
        assert!(composer.entities.pasted_blocks.is_empty());

        let text = format!("HEAD {placeholder} TAIL");
        let mut composer = composer_with(&text, 0);
        register_paste(&mut composer, 7, 5, "pasted");
        assert!(composer.delete(DeletionKind::WordRight));
        assert_eq!(composer.text(), format!("{placeholder} TAIL"));
        assert_eq!(composer.entities.pasted_blocks.len(), 1);
        assert!(composer.delete(DeletionKind::WordRight));
        assert_eq!(composer.text(), " TAIL");
        assert!(composer.entities.pasted_blocks.is_empty());

        let text = format!("{placeholder} ");
        let mut composer = composer_with(&text, text.len());
        register_paste(&mut composer, 7, 0, "pasted");
        assert!(composer.delete(DeletionKind::WordLeft));
        assert_eq!(composer.text(), "");
        assert!(composer.entities.pasted_blocks.is_empty());
    }

    #[test]
    fn composer_movement_and_deletion_keep_terminal_characters_atomic() {
        let mut composer = Composer::new();
        composer.insert_slice("Ae\u{301}B");
        composer.move_cursor(crate::input::MoveIntent::new(
            crate::input::MoveKind::CharacterLeft,
        ));
        composer.move_cursor(crate::input::MoveIntent::new(
            crate::input::MoveKind::CharacterLeft,
        ));
        assert_eq!(composer.cursor(), 1);
        composer.insert_slice("X");
        assert_eq!(composer.text(), "AXe\u{301}B");

        for character in ["🇺🇸", "👍🏽", "👨‍👩‍👧‍👦"] {
            let mut composer = Composer::new();
            composer.insert_slice("A");
            composer.insert_slice(character);
            composer.insert_slice("B");
            composer.move_cursor(crate::input::MoveIntent::new(
                crate::input::MoveKind::CharacterLeft,
            ));
            assert!(composer.delete(DeletionKind::CharacterLeft));
            assert_eq!(composer.text(), "AB");
            assert_eq!(composer.cursor(), 1);
        }

        let mut composer = Composer::new();
        composer.insert_slice("A👨‍👩‍👧‍👦B");
        composer.edit.set_cursor(1);
        assert!(composer.delete(DeletionKind::CharacterRight));
        assert_eq!(composer.text(), "AB");
        assert_eq!(composer.cursor(), 1);
    }

    #[test]
    fn input_cursor_movement_edits_inside_line() {
        let mut composer = composer_with("ab😀c", "ab😀c".len());
        composer.move_cursor(crate::input::MoveIntent::new(
            crate::input::MoveKind::CharacterLeft,
        ));
        assert_eq!(composer.text(), "ab😀c");
        assert!(composer.cursor() < composer.text().len());
        assert_eq!(
            composer.insert_text("X", usize::MAX),
            super::super::InsertResult::Inserted
        );
        assert_eq!(composer.text(), "ab😀Xc");
        assert!(composer.delete(DeletionKind::CharacterRight));
        assert_eq!(composer.text(), "ab😀X");
    }
}
