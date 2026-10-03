use super::Composer;
use super::edit_history::{EditHistory, Prepared};
use super::editor_state::InsertResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EditRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

fn planned_cursor_after(
    input_len: usize,
    range: EditRange,
    replacement_len: usize,
    requested_cursor_after: usize,
) -> Option<usize> {
    if range.start > range.end || range.end > input_len {
        return None;
    }
    let next_len = (input_len - (range.end - range.start)).saturating_add(replacement_len);
    Some(requested_cursor_after.min(next_len))
}

impl Composer {
    pub(crate) fn can_replace_selection_or_insert(
        &self,
        inserted_len: usize,
        max_len: usize,
    ) -> bool {
        match self.edit.selection_range() {
            Some(selection) => {
                self.can_replace_range(selection.start, selection.end, inserted_len, max_len)
            }
            None => self.can_insert(inserted_len, max_len),
        }
    }

    pub(crate) fn insert_text(&mut self, text: &str, max_len: usize) -> InsertResult {
        if self.claim_auto_separator(text) {
            self.vertical.reset();
            self.limit_rejection.clear();
            return InsertResult::Inserted;
        }
        if self.edit.selection_range().is_none() {
            return self.insert_slice_bounded(text, max_len);
        }
        self.replace_selection_bounded(text, max_len)
    }

    pub(crate) fn replace_text(&mut self, text: &str) {
        self.vertical.reset();
        self.edit.discard_selection();
        self.entities.clear();
        self.edit.swap_input(&mut text.to_owned());
        self.edit_history.reset();
        self.limit_rejection.clear();
        self.auto_separator = None;
    }

    pub(crate) fn delete_selection(&mut self) -> bool {
        let Some(selection) = self.edit.selection_range() else {
            return false;
        };
        let range = EditRange {
            start: selection.start,
            end: selection.end,
        };
        if !self.delete_range(range, selection.start) {
            return false;
        }
        self.edit.clear_selection();
        true
    }

    pub(crate) fn replace_range_bounded(
        &mut self,
        range: EditRange,
        replacement: &str,
        cursor_after: usize,
        max_len: usize,
    ) -> InsertResult {
        let Some(cursor_after) = planned_cursor_after(
            self.edit.input.len(),
            range,
            replacement.len(),
            cursor_after,
        ) else {
            return InsertResult::Inactive;
        };
        if !self.can_replace_range(range.start, range.end, replacement.len(), max_len) {
            return InsertResult::LimitExceeded;
        }
        let prepared = self.prepare_replacement_history(range, replacement, cursor_after);
        self.apply_replacement(range, replacement, cursor_after);
        self.edit_history.commit(prepared);
        InsertResult::Inserted
    }

    pub(crate) fn replace_selection_bounded(
        &mut self,
        replacement: &str,
        max_len: usize,
    ) -> InsertResult {
        let Some(selection) = self.edit.selection_range() else {
            return InsertResult::Inactive;
        };
        let result = self.replace_range_bounded(
            EditRange {
                start: selection.start,
                end: selection.end,
            },
            replacement,
            selection.start.saturating_add(replacement.len()),
            max_len,
        );
        if result == InsertResult::Inserted {
            self.edit.clear_selection();
        }
        result
    }

    pub(crate) fn delete_range(&mut self, range: EditRange, cursor_after: usize) -> bool {
        let Some(cursor_after) =
            planned_cursor_after(self.edit.input.len(), range, 0, cursor_after)
        else {
            return false;
        };
        let prepared = self.prepare_replacement_history(range, "", cursor_after);
        self.apply_replacement(range, "", cursor_after);
        self.edit_history.commit(prepared);
        true
    }

    pub(crate) fn delete_input_range(&mut self, start: usize, end: usize) {
        if start >= end || end > self.edit.input.len() {
            return;
        }
        self.entities.discard_pending_separator();
        self.entities.adjust_for_delete(start, end);
        self.edit.delete_text_range(start, end);
    }

    pub(crate) fn remove_entities_overlapping(&mut self, start: usize, end: usize) {
        while let Some(entity) = self.entities.entity_overlapping(start, end) {
            self.entities.remove(entity);
        }
    }

    fn prepare_replacement_history(
        &self,
        range: EditRange,
        replacement: &str,
        cursor_after: usize,
    ) -> Prepared {
        let structured = self
            .entities
            .entity_overlapping(range.start, range.end)
            .is_some()
            || (range.start == range.end
                && self
                    .entities
                    .entity_containing(&self.edit.input, range.start)
                    .is_some());
        if structured {
            return Prepared::Boundary;
        }
        EditHistory::prepare(
            range.start,
            &self.edit.input[range.start..range.end],
            replacement,
            self.edit.cursor,
            cursor_after,
        )
    }

    fn apply_replacement(&mut self, range: EditRange, replacement: &str, cursor_after: usize) {
        self.remove_entities_overlapping(range.start, range.end);
        self.delete_input_range(range.start, range.end);
        self.edit.set_cursor(range.start);
        self.insert_slice_without_history(replacement);
        self.edit.set_cursor(cursor_after);
        self.limit_rejection.clear();
        self.vertical.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::super::SelectionRange;
    use super::super::entity_spans::Span;
    use super::super::pasted_blocks::PastedBlock;
    use super::super::test_fixture::{replace_text, select};
    use super::super::visual_layout::VerticalDirection;
    use super::*;
    use crate::input::TextOwner;

    fn with_paste(text: &str, placeholder: &str, backing: &str) -> Composer {
        let mut composer = Composer::new();
        replace_text(&mut composer, text);
        let start = text.find(placeholder).unwrap();
        composer.entities.register_pasted_block(PastedBlock {
            id: 1,
            text: backing.to_owned(),
            line_count: 1,
            span: Span::new(start, start + placeholder.len()),
        });
        composer
    }

    #[test]
    fn replacement_plan_validates_ranges_and_clamps_the_final_cursor() {
        assert_eq!(
            planned_cursor_after(6, EditRange { start: 5, end: 2 }, 1, 0),
            None
        );
        assert_eq!(
            planned_cursor_after(6, EditRange { start: 2, end: 7 }, 1, 0),
            None
        );
        assert_eq!(
            planned_cursor_after(6, EditRange { start: 2, end: 5 }, 4, 99),
            Some(7)
        );
    }

    #[test]
    fn bounded_selection_replacement_preserves_state_on_rejection() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "abcdef");
        select(&mut composer, 2, 5);
        assert_eq!(
            composer.replace_selection_bounded("WXYZ", 7),
            InsertResult::Inserted
        );
        assert_eq!(composer.text(), "abWXYZf");
        assert_eq!(composer.cursor(), 6);
        assert_eq!(composer.selection(), None);

        select(&mut composer, 2, 6);
        composer.vertical.preferred_column = Some(7);
        composer.note_limit_rejection(TextOwner::Composer);
        assert_eq!(
            composer.replace_selection_bounded("123456789", 7),
            InsertResult::LimitExceeded
        );
        assert_eq!(composer.text(), "abWXYZf");
        assert_eq!(
            composer.selection(),
            Some(SelectionRange { start: 2, end: 6 })
        );
        assert_eq!(composer.preferred_column(), Some(7));
        assert!(!composer.note_limit_rejection(TextOwner::Composer));
    }

    #[test]
    fn an_accepted_replacement_ends_the_vertical_motion() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "abcdef\nabcdef\nabcdef");
        select(&mut composer, 0, 7);
        composer.vertical.preferred_column = Some(0);
        assert_eq!(
            composer.insert_text("x\nxyz", usize::MAX),
            InsertResult::Inserted
        );
        assert_eq!(composer.text(), "x\nxyzabcdef\nabcdef");
        assert_eq!(composer.cursor(), 5);
        assert_eq!(composer.preferred_column(), None);
        composer.move_vertical(VerticalDirection::Down, false, None, 80);
        assert_eq!(composer.cursor(), 15);
    }

    #[test]
    fn invalid_range_replacement_is_inactive_without_mutation() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "abc");
        composer.note_limit_rejection(TextOwner::Composer);
        assert_eq!(
            composer.replace_range_bounded(EditRange { start: 1, end: 4 }, "x", 2, 8),
            InsertResult::Inactive
        );
        assert_eq!(composer.text(), "abc");
        assert_eq!(composer.cursor(), 3);
        assert!(!composer.note_limit_rejection(TextOwner::Composer));
    }

    #[test]
    fn range_replacement_measures_and_releases_expanded_paste_backing() {
        let placeholder = "[Pasted text #1, 1 line]";
        let mut composer = with_paste(placeholder, placeholder, "expanded");
        assert_eq!(
            composer.replace_range_bounded(
                EditRange {
                    start: 0,
                    end: placeholder.len()
                },
                "ok",
                2,
                2
            ),
            InsertResult::Inserted
        );
        assert_eq!(composer.text(), "ok");
        assert!(composer.entities.pasted_blocks.is_empty());
    }

    #[test]
    fn range_replacement_measures_expanded_paste_outside_the_replaced_range() {
        let placeholder = "[Pasted text #1, 1 line]";
        let backing = "expanded backing that exceeds the placeholder width";
        let mut composer = with_paste(&format!("{placeholder}x"), placeholder, backing);
        composer.note_limit_rejection(TextOwner::Composer);
        let end = placeholder.len() + 1;
        assert_eq!(
            composer.replace_range_bounded(
                EditRange {
                    start: placeholder.len(),
                    end
                },
                "ok",
                placeholder.len() + 2,
                placeholder.len() + 2
            ),
            InsertResult::LimitExceeded
        );
        assert_eq!(composer.text(), format!("{placeholder}x"));
        assert_eq!(composer.cursor(), end);
        assert_eq!(composer.entities.pasted_blocks.len(), 1);
        assert_eq!(composer.entities.pasted_blocks[0].text, backing);
        assert!(!composer.note_limit_rejection(TextOwner::Composer));
    }

    #[test]
    fn bounded_composer_edits_count_registered_paste_backing_text() {
        let placeholder = "[Pasted text #1, 1 line]";
        let mut insertion = with_paste(placeholder, placeholder, "expanded");
        assert_eq!(insertion.expanded_len(), Some("expanded".len()));
        assert_eq!(
            insertion.insert_text("!", "expanded".len()),
            InsertResult::LimitExceeded
        );
        assert_eq!(insertion.text(), placeholder);

        let mut replacement = with_paste(placeholder, placeholder, "expanded");
        assert_eq!(
            replacement.replace_range_bounded(
                EditRange {
                    start: 0,
                    end: placeholder.len()
                },
                "ok",
                2,
                2
            ),
            InsertResult::Inserted
        );
        assert_eq!(replacement.text(), "ok");
        assert!(replacement.entities.pasted_blocks.is_empty());
    }

    #[test]
    fn selection_deletion_succeeds_beside_a_large_paste() {
        let placeholder = "[Pasted text #1, 1 line]";
        let mut composer = with_paste(&format!("{placeholder} xy"), placeholder, &"p".repeat(5000));
        select(&mut composer, placeholder.len() + 1, placeholder.len() + 3);
        assert!(composer.delete_selection());
        assert_eq!(composer.text(), format!("{placeholder} "));
        assert_eq!(composer.entities.pasted_blocks.len(), 1);
    }
}
