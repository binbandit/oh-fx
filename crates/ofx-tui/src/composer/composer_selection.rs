use super::Composer;

impl Composer {
    pub(crate) fn select_all(&mut self) -> bool {
        self.vertical.reset();
        self.edit.select_all()
    }
}

#[cfg(test)]
mod tests {
    use super::super::SelectionRange;
    use super::super::test_fixture::{replace_text, select};
    use super::*;

    #[test]
    fn composer_select_all_clamps_to_the_draft_and_clears_vertical_intent() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "alpha beta");
        composer.vertical.preferred_column = Some(3);
        assert!(composer.select_all());
        assert_eq!(
            composer.selection(),
            Some(SelectionRange {
                start: 0,
                end: "alpha beta".len()
            })
        );
        assert_eq!(composer.preferred_column(), None);
    }

    #[test]
    fn editor_state_owns_selection_and_unicode_cursor_transitions() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "Ae\u{301}B");
        let left = crate::input::MoveIntent::new(crate::input::MoveKind::CharacterLeft);
        assert!(composer.move_cursor(left));
        assert_eq!(composer.cursor(), 4);
        assert!(composer.move_cursor(left));
        assert_eq!(composer.cursor(), 1);

        select(&mut composer, 1, 4);
        assert_eq!(composer.selected_text(), Some("e\u{301}"));
        let right = crate::input::MoveIntent::new(crate::input::MoveKind::CharacterRight);
        assert!(composer.move_cursor(right));
        assert_eq!(composer.cursor(), 4);
        assert_eq!(composer.selection(), None);
    }
}
