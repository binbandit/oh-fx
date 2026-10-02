use super::Composer;
use super::editor_state::{EditorState, SelectionEdge};
use super::registered_entities::Entities;
use super::text_boundaries::{
    is_word_character_at, logical_line_end, logical_line_start, next_character_end,
    next_paragraph_start, previous_character_start, previous_paragraph_start,
};
use crate::input::{MoveIntent, MoveKind};

impl Composer {
    pub(crate) fn move_cursor(&mut self, intent: MoveIntent) -> bool {
        self.vertical.reset();
        let edit = &mut self.edit;
        let extend = intent.extend_selection;
        match intent.kind {
            MoveKind::CharacterLeft => move_character_left(edit, &self.entities, extend),
            MoveKind::CharacterRight => move_character_right(edit, &self.entities, extend),
            MoveKind::DraftStart => edit.move_cursor_to(0, extend),
            MoveKind::DraftEnd => edit.move_cursor_to(edit.input.len(), extend),
            MoveKind::LineStart => {
                edit.move_cursor_to(logical_line_start(&edit.input, edit.cursor), extend)
            }
            MoveKind::LineEnd => {
                edit.move_cursor_to(logical_line_end(&edit.input, edit.cursor), extend)
            }
            MoveKind::ParagraphUp => {
                edit.move_cursor_to(previous_paragraph_start(&edit.input, edit.cursor), extend)
            }
            MoveKind::ParagraphDown => {
                edit.move_cursor_to(next_paragraph_start(&edit.input, edit.cursor), extend)
            }
            MoveKind::WordLeft => move_word_left(edit, &self.entities, extend),
            MoveKind::WordRight => move_word_right(edit, &self.entities, extend),
            MoveKind::VisualUp | MoveKind::VisualDown | MoveKind::PageUp | MoveKind::PageDown => {
                false
            }
        }
    }
}

fn move_character_left(edit: &mut EditorState, entities: &Entities, extend: bool) -> bool {
    if !extend && edit.collapse_selection(SelectionEdge::Start) {
        return true;
    }
    if edit.cursor == 0 {
        return false;
    }
    let target = entities
        .entity_ending_at(&edit.input, edit.cursor)
        .map_or_else(
            || previous_character_start(&edit.input, edit.cursor),
            |entity| entity.span.raw_start,
        );
    edit.move_cursor_to(target, extend)
}

fn move_character_right(edit: &mut EditorState, entities: &Entities, extend: bool) -> bool {
    if !extend && edit.collapse_selection(SelectionEdge::End) {
        return true;
    }
    if edit.cursor >= edit.input.len() {
        return false;
    }
    let target = entities
        .entity_starting_at(&edit.input, edit.cursor)
        .map_or_else(
            || next_character_end(&edit.input, edit.cursor),
            |entity| entity.span.raw_end,
        );
    edit.move_cursor_to(target, extend)
}

fn move_word_left(edit: &mut EditorState, entities: &Entities, extend: bool) -> bool {
    if !extend && edit.collapse_selection(SelectionEdge::Start) {
        return true;
    }
    if edit.cursor == 0 {
        return false;
    }
    let target = word_left_target(&edit.input, edit.cursor, entities);
    edit.move_cursor_to(target, extend)
}

fn word_left_target(input: &str, cursor: usize, entities: &Entities) -> usize {
    let mut raw_offset = cursor;
    for stop_at_word in [true, false] {
        while raw_offset > 0 {
            if let Some(entity) = entities.entity_ending_at(input, raw_offset) {
                return entity.span.raw_start;
            }
            let previous = previous_character_start(input, raw_offset);
            if is_word_character_at(input, previous) == stop_at_word {
                break;
            }
            raw_offset = previous;
        }
    }
    raw_offset
}

fn move_word_right(edit: &mut EditorState, entities: &Entities, extend: bool) -> bool {
    if !extend && edit.collapse_selection(SelectionEdge::End) {
        return true;
    }
    if edit.cursor >= edit.input.len() {
        return false;
    }
    let target = word_right_target(&edit.input, edit.cursor, entities);
    edit.move_cursor_to(target, extend)
}

fn word_right_target(input: &str, cursor: usize, entities: &Entities) -> usize {
    let mut raw_offset = cursor;
    while raw_offset < input.len() {
        if let Some(entity) = entities.entity_starting_at(input, raw_offset) {
            return entity.span.raw_end;
        }
        if is_word_character_at(input, raw_offset) {
            break;
        }
        raw_offset = next_character_end(input, raw_offset);
    }
    while raw_offset < input.len() {
        if entities.entity_starting_at(input, raw_offset).is_some()
            || !is_word_character_at(input, raw_offset)
        {
            break;
        }
        raw_offset = next_character_end(input, raw_offset);
    }
    raw_offset
}

#[cfg(test)]
mod tests {
    use super::super::entity_spans::Span;
    use super::super::pasted_blocks::{PastedBlock, format_placeholder};
    use super::super::test_fixture::{replace_text, select, selected_text};
    use super::*;

    fn intent(kind: MoveKind) -> MoveIntent {
        MoveIntent::new(kind)
    }

    fn composer_with(text: &str, cursor: usize) -> Composer {
        let mut composer = Composer::new();
        replace_text(&mut composer, text);
        composer.edit.set_cursor(cursor);
        composer
    }

    fn register_paste(composer: &mut Composer, id: usize, start: usize) {
        let placeholder = format_placeholder(id, 1);
        composer.entities.register_pasted_block(PastedBlock {
            id,
            text: "pasted".to_owned(),
            line_count: 1,
            span: Span::new(start, start + placeholder.len()),
        });
    }

    #[test]
    fn horizontal_character_motion_collapses_selections_at_the_matching_edge() {
        let mut composer = composer_with("abcdef", 6);
        composer.vertical.preferred_column = Some(4);
        select(&mut composer, 2, 5);
        assert!(composer.move_cursor(intent(MoveKind::CharacterLeft)));
        assert_eq!(composer.cursor(), 2);
        assert_eq!(composer.selection(), None);
        assert_eq!(composer.preferred_column(), None);

        select(&mut composer, 2, 5);
        assert!(composer.move_cursor(intent(MoveKind::CharacterRight)));
        assert_eq!(composer.cursor(), 5);
        assert_eq!(composer.selection(), None);
    }

    #[test]
    fn horizontal_character_motion_preserves_display_units_and_registered_entities() {
        let placeholder = format_placeholder(2, 1);
        let text = format!("Ae\u{301}{placeholder}B");
        let mut composer = composer_with(&text, text.len());
        register_paste(&mut composer, 2, "Ae\u{301}".len());

        assert!(composer.move_cursor(intent(MoveKind::CharacterLeft)));
        assert_eq!(composer.cursor(), "Ae\u{301}".len() + placeholder.len());
        assert!(composer.move_cursor(intent(MoveKind::CharacterLeft)));
        assert_eq!(composer.cursor(), "Ae\u{301}".len());
        assert!(composer.move_cursor(intent(MoveKind::CharacterLeft)));
        assert_eq!(composer.cursor(), "A".len());
    }

    #[test]
    fn horizontal_word_motion_preserves_punctuation_unicode_and_entity_boundaries() {
        let placeholder = format_placeholder(1, 1);
        let prefix = "h\u{e9}llo.a_b ";
        let text = format!("{prefix}{placeholder} next");
        let mut composer = composer_with(&text, 0);
        register_paste(&mut composer, 1, prefix.len());

        assert!(composer.move_cursor(intent(MoveKind::WordRight)));
        assert_eq!(composer.cursor(), "h\u{e9}llo".len());
        assert!(composer.move_cursor(intent(MoveKind::WordRight)));
        assert_eq!(composer.cursor(), "h\u{e9}llo.a_b".len());
        assert!(composer.move_cursor(intent(MoveKind::WordRight)));
        assert_eq!(composer.cursor(), prefix.len() + placeholder.len());
        assert!(composer.move_cursor(intent(MoveKind::WordLeft)));
        assert_eq!(composer.cursor(), prefix.len());
    }

    #[test]
    fn horizontal_line_and_input_motions_keep_distinct_boundaries() {
        let mut composer = composer_with("FIRST\nSECOND\nTHIRD", "FIRST\nSEC".len());
        composer.vertical.preferred_column = Some(3);
        assert!(composer.move_cursor(intent(MoveKind::LineStart)));
        assert_eq!(composer.cursor(), "FIRST\n".len());
        assert!(composer.move_cursor(intent(MoveKind::LineEnd)));
        assert_eq!(composer.cursor(), "FIRST\nSECOND".len());
        assert!(composer.move_cursor(intent(MoveKind::DraftStart)));
        assert_eq!(composer.cursor(), 0);
        assert!(composer.move_cursor(intent(MoveKind::DraftEnd)));
        assert_eq!(composer.cursor(), composer.text().len());

        composer.vertical.preferred_column = Some(7);
        assert!(!composer.move_cursor(intent(MoveKind::CharacterRight)));
        assert_eq!(composer.preferred_column(), None);
    }

    #[test]
    fn editor_state_moves_across_logical_lines() {
        let mut composer = composer_with("alpha beta\ngamma", "alpha beta\ngamma".len());
        assert!(composer.move_cursor(intent(MoveKind::LineStart)));
        assert_eq!(composer.cursor(), 11);
        assert!(composer.move_cursor(intent(MoveKind::DraftStart)));
        assert_eq!(composer.cursor(), 0);
        assert!(composer.move_cursor(intent(MoveKind::LineEnd)));
        assert_eq!(composer.cursor(), 10);
    }

    #[test]
    fn line_boundary_cursor_movement_stays_on_the_current_logical_line() {
        let text = "FIRST\nSECOND\n\nTHIRD";
        let mut composer = composer_with(text, "FIRST\nSEC".len());
        composer.vertical.preferred_column = Some(3);
        composer.move_cursor(intent(MoveKind::LineStart));
        assert_eq!(composer.cursor(), "FIRST\n".len());
        assert_eq!(composer.preferred_column(), None);

        composer.edit.set_cursor("FIRST\nSEC".len());
        composer.move_cursor(intent(MoveKind::LineEnd));
        assert_eq!(composer.cursor(), "FIRST\nSECOND".len());

        composer.edit.set_cursor("FIRST\nSECOND\n".len());
        composer.move_cursor(intent(MoveKind::LineStart));
        assert_eq!(composer.cursor(), "FIRST\nSECOND\n".len());
        composer.move_cursor(intent(MoveKind::LineEnd));
        assert_eq!(composer.cursor(), "FIRST\nSECOND\n".len());
    }

    #[test]
    fn shifted_motion_extends_the_selection_from_the_original_cursor() {
        let mut composer = composer_with("alpha beta", 5);
        let extend = MoveIntent {
            kind: MoveKind::WordRight,
            extend_selection: true,
        };
        assert!(composer.move_cursor(extend));
        assert_eq!(selected_text(&composer), Some(" beta"));
    }

    #[test]
    fn paragraph_motions_jump_between_blank_line_blocks() {
        let text = "one\ntwo\n\nthree";
        let mut composer = composer_with(text, 0);
        assert!(composer.move_cursor(intent(MoveKind::ParagraphDown)));
        assert_eq!(composer.cursor(), text.find("three").unwrap());
        assert!(composer.move_cursor(intent(MoveKind::ParagraphUp)));
        assert_eq!(composer.cursor(), 0);
    }
}
