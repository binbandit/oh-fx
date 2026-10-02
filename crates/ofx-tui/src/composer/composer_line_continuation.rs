use super::Composer;
use super::edit_history::EditHistory;

fn backslash_index_before_cursor(input: &str, cursor: usize) -> Option<usize> {
    if cursor == 0 || cursor > input.len() {
        return None;
    }
    let index = cursor - 1;
    (input.as_bytes()[index] == b'\\').then_some(index)
}

impl Composer {
    pub(crate) fn replace_backslash_before_cursor_with_newline(&mut self) -> bool {
        self.vertical.reset();
        let Some(index) = backslash_index_before_cursor(&self.edit.input, self.edit.cursor) else {
            return false;
        };
        let cursor = self.edit.cursor;
        if !self.edit.replace_char_before_cursor('\\', '\n') {
            return false;
        }
        self.edit_history
            .commit(EditHistory::prepare(index, "\\", "\n", cursor, cursor));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::SelectionRange;
    use super::super::test_fixture::{replace_text, select};
    use super::*;

    #[test]
    fn line_continuation_requires_a_backslash_immediately_before_the_cursor() {
        assert_eq!(backslash_index_before_cursor("", 0), None);
        assert_eq!(backslash_index_before_cursor("x", 0), None);
        assert_eq!(backslash_index_before_cursor("x", 1), None);
        assert_eq!(backslash_index_before_cursor("x\\y", 2), Some(1));
        assert_eq!(backslash_index_before_cursor("x\\y", 4), None);
    }

    #[test]
    fn line_continuation_records_one_edit_and_reconciles_transient_state() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "x \\");
        let end = composer.text().len();
        select(&mut composer, 0, end);
        composer.vertical.preferred_column = Some(5);

        assert!(composer.replace_backslash_before_cursor_with_newline());

        assert_eq!(composer.text(), "x \n");
        assert_eq!(composer.cursor(), 3);
        assert_eq!(composer.selection(), None);
        assert_eq!(composer.preferred_column(), None);

        let entry = composer.edit_history.peek_undo().unwrap();
        assert_eq!(entry.start, 2);
        assert_eq!(entry.removed, "\\");
        assert_eq!(entry.inserted, "\n");
        assert_eq!(entry.cursor_before, 3);
        assert_eq!(entry.cursor_after, 3);
    }

    #[test]
    fn line_continuation_no_op_only_resets_vertical_navigation() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "draft ");
        let end = composer.text().len();
        select(&mut composer, 0, end);
        composer.vertical.preferred_column = Some(4);
        composer
            .edit_history
            .commit(EditHistory::prepare(0, "", "draft ", 0, 6));

        assert!(!composer.replace_backslash_before_cursor_with_newline());

        assert_eq!(composer.text(), "draft ");
        assert_eq!(
            composer.selection(),
            Some(SelectionRange { start: 0, end: 6 })
        );
        assert!(composer.edit_history.peek_undo().is_some());
        assert_eq!(composer.preferred_column(), None);
    }
}
