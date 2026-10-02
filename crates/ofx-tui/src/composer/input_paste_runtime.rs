use super::Composer;
use super::editor_state::InsertResult;
use super::entity_spans::Span;
use super::pasted_blocks::{PastedBlock, count_lines, format_placeholder, should_use_placeholder};

impl Composer {
    pub(crate) fn insert_paste(&mut self, text: &str, max_len: usize) -> InsertResult {
        if text.is_empty() {
            return InsertResult::Inactive;
        }
        if !should_use_placeholder(text) {
            return self.insert_text(text, max_len);
        }
        if !self.can_replace_selection_or_insert(text.len(), max_len) {
            return InsertResult::LimitExceeded;
        }
        let id = self.entities.next_paste_id;
        let line_count = count_lines(text);
        let placeholder = format_placeholder(id, line_count);
        let raw_start = self
            .edit
            .selection_range()
            .map_or(self.edit.cursor, |selection| selection.start);
        let result = self.insert_text(&placeholder, max_len);
        if result != InsertResult::Inserted {
            return result;
        }
        self.entities.register_pasted_block(PastedBlock {
            id,
            text: text.to_owned(),
            line_count,
            span: Span::new(raw_start, raw_start + placeholder.len()),
        });
        self.entities.next_paste_id = id.saturating_add(1);
        self.edit_history.reset();
        InsertResult::Inserted
    }
}

#[cfg(test)]
mod tests {
    use super::super::pasted_blocks::LARGE_PASTE_CHAR_THRESHOLD;
    use super::super::test_fixture::{replace_text, select};
    use super::*;

    #[test]
    fn small_pastes_insert_literal_text_with_undo() {
        let mut composer = Composer::new();
        assert_eq!(
            composer.insert_paste("hi\nthere", 4096),
            InsertResult::Inserted
        );
        assert_eq!(composer.text(), "hi\nthere");
        assert!(composer.entities.pasted_blocks.is_empty());
        assert!(composer.undo());
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn large_pastes_insert_a_placeholder_that_expands_on_submit() {
        let pasted = format!("{}\nend", "x".repeat(LARGE_PASTE_CHAR_THRESHOLD));
        let mut composer = Composer::new();
        replace_text(&mut composer, "ab");
        select(&mut composer, 1, 2);

        assert_eq!(
            composer.insert_paste(&pasted, usize::MAX),
            InsertResult::Inserted
        );

        let placeholder = "[Pasted text #1, 2 lines]";
        assert_eq!(composer.text(), format!("a{placeholder}"));
        assert_eq!(composer.cursor(), 1 + placeholder.len());
        assert_eq!(composer.selection(), None);
        assert_eq!(composer.expanded_text(), format!("a{pasted}"));
        assert_eq!(composer.entities.next_paste_id, 2);
        assert_eq!(composer.edit_history.peek_undo(), None);
    }

    #[test]
    fn large_pastes_respect_the_expanded_input_limit() {
        let pasted = "y".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);
        let mut composer = Composer::new();
        replace_text(&mut composer, "ab");

        assert_eq!(
            composer.insert_paste(&pasted, pasted.len() + 1),
            InsertResult::LimitExceeded
        );
        assert_eq!(composer.text(), "ab");
        assert!(composer.entities.pasted_blocks.is_empty());
        assert_eq!(composer.entities.next_paste_id, 1);
    }
}
