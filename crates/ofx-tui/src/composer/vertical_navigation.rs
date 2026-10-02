use super::Composer;
use super::editor_state::EditorState;
use super::registered_entities::Entities;
use super::visual_layout::VerticalDirection;
use crate::input::{MoveIntent, MoveKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerticalOutcome {
    Moved,
    Unchanged,
    ReachedTop,
    ReachedBottom,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct VerticalNavigation {
    pub(crate) preferred_column: Option<usize>,
}

impl VerticalNavigation {
    pub(crate) fn reset(&mut self) {
        self.preferred_column = None;
    }

    pub(crate) fn preferred_column(self) -> Option<usize> {
        self.preferred_column
    }

    pub(crate) fn apply_target(
        &mut self,
        edit: &mut EditorState,
        entities: &Entities,
        target_raw_offset: Option<usize>,
        preferred_column: usize,
        extend_selection: bool,
    ) -> bool {
        let Some(raw_offset) = target_raw_offset
            .filter(|offset| entities.entity_containing(&edit.input, *offset).is_none())
        else {
            self.reset();
            return false;
        };
        edit.move_cursor_to(raw_offset, extend_selection);
        self.preferred_column = Some(preferred_column);
        true
    }
}

impl Composer {
    pub(crate) fn move_vertical(
        &mut self,
        direction: VerticalDirection,
        extend_selection: bool,
        page_rows: Option<usize>,
        terminal_cols: u16,
    ) -> VerticalOutcome {
        let layout = self.visual_layout(terminal_cols);
        let preferred = self.vertical.preferred_column();
        let scan = match page_rows {
            Some(rows) => layout.scan_row_delta(direction, rows, preferred),
            None => layout.scan_adjacent_row(direction, preferred),
        };
        if self.vertical.apply_target(
            &mut self.edit,
            &self.entities,
            scan.target.map(|target| target.raw_offset),
            scan.preferred_column,
            extend_selection,
        ) {
            return VerticalOutcome::Moved;
        }
        if page_rows.is_some() || extend_selection {
            return VerticalOutcome::Unchanged;
        }
        match direction {
            VerticalDirection::Up if self.edit.cursor > 0 => {
                self.move_cursor(MoveIntent::new(MoveKind::DraftStart));
                VerticalOutcome::Moved
            }
            VerticalDirection::Up => VerticalOutcome::ReachedTop,
            VerticalDirection::Down if self.edit.cursor < self.edit.input.len() => {
                self.move_cursor(MoveIntent::new(MoveKind::DraftEnd));
                VerticalOutcome::Moved
            }
            VerticalDirection::Down => VerticalOutcome::ReachedBottom,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::SelectionRange;
    use super::super::entity_spans::Span;
    use super::super::pasted_blocks::PastedBlock;
    use super::super::test_fixture::{replace_text, select};
    use super::super::visual_layout::{CursorPoint, VisualLayout};
    use super::*;

    fn composer_with(text: &str, cursor: usize) -> Composer {
        let mut composer = Composer::new();
        replace_text(&mut composer, text);
        composer.edit.set_cursor(cursor);
        composer
    }

    fn move_vertical(composer: &mut Composer, direction: VerticalDirection, cols: u16) -> bool {
        let scan = composer
            .visual_layout(cols)
            .scan_adjacent_row(direction, composer.vertical.preferred_column());
        composer.vertical.apply_target(
            &mut composer.edit,
            &composer.entities,
            scan.target.map(|target| target.raw_offset),
            scan.preferred_column,
            false,
        )
    }

    fn expect_scan_matches_layout(composer: &Composer, direction: VerticalDirection, cols: u16) {
        let actual = composer
            .visual_layout(cols)
            .scan_adjacent_row(direction, composer.preferred_column());
        let expected = VisualLayout::new(
            composer.text(),
            composer.cursor(),
            cols,
            &composer.entities.pasted_blocks,
        )
        .scan_adjacent_row(direction, None);
        assert_eq!(actual, expected);
    }

    #[test]
    fn vertical_navigation_applies_a_legal_target_and_clears_selection() {
        let mut composer = composer_with("abcdef", 6);
        select(&mut composer, 1, 4);
        assert!(composer.vertical.apply_target(
            &mut composer.edit,
            &composer.entities,
            Some(5),
            3,
            false
        ));
        assert_eq!(composer.cursor(), 5);
        assert_eq!(composer.selection(), None);
        assert_eq!(composer.preferred_column(), Some(3));
    }

    #[test]
    fn vertical_navigation_reset_decisions_preserve_the_editor() {
        let mut composer = composer_with("abcdef", 6);
        select(&mut composer, 1, 4);
        composer.vertical.preferred_column = Some(4);
        assert!(!composer.vertical.apply_target(
            &mut composer.edit,
            &composer.entities,
            None,
            0,
            false
        ));
        assert_eq!(composer.cursor(), 4);
        assert_eq!(
            composer.selection(),
            Some(SelectionRange { start: 1, end: 4 })
        );
        assert_eq!(composer.preferred_column(), None);
    }

    #[test]
    fn vertical_navigation_rejects_targets_inside_registered_entities() {
        let placeholder = "[Pasted text #1, 1 line]";
        let mut composer = composer_with(placeholder, 0);
        composer.entities.register_pasted_block(PastedBlock {
            id: 1,
            text: "pasted".to_owned(),
            line_count: 1,
            span: Span::new(0, placeholder.len()),
        });
        composer.vertical.preferred_column = Some(2);
        assert!(!composer.vertical.apply_target(
            &mut composer.edit,
            &composer.entities,
            Some(3),
            3,
            false
        ));
        assert_eq!(composer.cursor(), 0);
        assert_eq!(composer.preferred_column(), None);
    }

    #[test]
    fn vertical_navigation_extends_from_the_original_cursor_when_requested() {
        let mut composer = composer_with("abcdef", 2);
        assert!(composer.vertical.apply_target(
            &mut composer.edit,
            &composer.entities,
            Some(5),
            3,
            true
        ));
        assert_eq!(
            composer.selection(),
            Some(SelectionRange { start: 2, end: 5 })
        );
        assert_eq!(composer.preferred_column(), Some(3));
    }

    #[test]
    fn vertical_movement_scans_hard_newline_rows_and_preserves_preferred_column() {
        let text = "abcdef\nxy\nabcdef";
        let mut composer = composer_with(text, text.len());
        expect_scan_matches_layout(&composer, VerticalDirection::Up, 80);
        assert!(move_vertical(&mut composer, VerticalDirection::Up, 80));
        assert_eq!(composer.cursor(), "abcdef\nxy".len());
        assert_eq!(composer.preferred_column(), Some(6));
        assert!(move_vertical(&mut composer, VerticalDirection::Up, 80));
        assert_eq!(composer.cursor(), "abcdef".len());
        assert_eq!(composer.preferred_column(), Some(6));
    }

    #[test]
    fn vertical_movement_scans_soft_wraps_and_boundary_owned_rows() {
        let composer = composer_with("abcdefgh", 4);
        let scan = composer
            .visual_layout(6)
            .scan_adjacent_row(VerticalDirection::Up, None);
        assert!(scan.target.is_some());
        assert_eq!(scan.total_rows, 2);
        assert_eq!(scan.cursor_row, 1);
    }

    #[test]
    fn vertical_movement_targets_visual_units_exactly() {
        let cases: [(&str, usize, u16); 4] = [
            ("界\nab", "界\nab".len(), 80),
            ("a\u{301}\nb", "a\u{301}\nb".len(), 80),
            ("\tX\nz", "\tX\nz".len(), 10),
            ("😀\nz", 0, 80),
        ];
        for (input, cursor, cols) in cases {
            let composer = composer_with(input, cursor);
            expect_scan_matches_layout(&composer, VerticalDirection::Down, cols);
        }
    }

    #[test]
    fn vertical_movement_never_targets_inside_a_registered_paste() {
        let placeholder = "[Pasted text #7, 1 line]";
        let input = format!("x\n{placeholder}");
        let mut composer = composer_with(&input, input.len());
        composer.entities.register_pasted_block(PastedBlock {
            id: 7,
            text: "P".repeat(1001),
            line_count: 1,
            span: Span::new("x\n".len(), input.len()),
        });

        assert!(move_vertical(&mut composer, VerticalDirection::Up, 80));
        composer.move_cursor(MoveIntent::new(MoveKind::CharacterLeft));
        composer.move_cursor(MoveIntent::new(MoveKind::CharacterRight));
        let down = composer
            .visual_layout(80)
            .scan_adjacent_row(VerticalDirection::Down, composer.preferred_column());
        assert_eq!(down.target.unwrap().raw_offset, "x\n".len());
        assert!(
            !composer.entities.pasted_blocks[0]
                .span
                .contains(down.target.unwrap().raw_offset)
        );

        let cursor_before = composer.cursor();
        let inside = composer.entities.pasted_blocks[0].span.raw_start + 1;
        assert!(!composer.vertical.apply_target(
            &mut composer.edit,
            &composer.entities,
            Some(inside),
            1,
            false
        ));
        assert_eq!(composer.cursor(), cursor_before);
    }

    #[test]
    fn vertical_scan_reports_null_target_facts_at_visual_edges() {
        let mut composer = composer_with("abc", 0);
        let scan = composer
            .visual_layout(80)
            .scan_adjacent_row(VerticalDirection::Up, None);
        assert_eq!(scan.target, None);
        assert_eq!(scan.total_rows, 1);
        assert_eq!(scan.cursor_row, 0);
        assert!(!move_vertical(&mut composer, VerticalDirection::Up, 80));
        assert_eq!(composer.preferred_column(), None);
    }

    #[test]
    fn vertical_intent_resets_before_nonvertical_no_op_cursor_movement() {
        let mut top = composer_with("abc\n", 4);
        assert!(move_vertical(&mut top, VerticalDirection::Up, 80));
        assert_eq!(top.cursor(), 0);
        assert!(top.preferred_column().is_some());
        top.move_cursor(MoveIntent::new(MoveKind::CharacterLeft));
        assert_eq!(top.preferred_column(), None);
        top.move_cursor(MoveIntent::new(MoveKind::DraftStart));
        assert_eq!(top.preferred_column(), None);
        let down = top
            .visual_layout(80)
            .scan_adjacent_row(VerticalDirection::Down, top.preferred_column());
        assert_eq!(down.preferred_column, 0);

        let mut end = composer_with("abc\n", 0);
        assert!(move_vertical(&mut end, VerticalDirection::Down, 80));
        assert_eq!(end.cursor(), 4);
        assert!(end.preferred_column().is_some());
        end.move_cursor(MoveIntent::new(MoveKind::CharacterRight));
        assert_eq!(end.preferred_column(), None);
        end.move_cursor(MoveIntent::new(MoveKind::DraftEnd));
        assert_eq!(end.preferred_column(), None);
        let up = end
            .visual_layout(80)
            .scan_adjacent_row(VerticalDirection::Up, end.preferred_column());
        assert_eq!(up.preferred_column, 0);
    }

    #[test]
    fn vertical_moves_fall_back_to_draft_edges_then_report_the_boundary() {
        let mut composer = composer_with("abc", 1);
        assert_eq!(
            composer.move_vertical(VerticalDirection::Up, false, None, 80),
            VerticalOutcome::Moved
        );
        assert_eq!(composer.cursor(), 0);
        assert_eq!(
            composer.move_vertical(VerticalDirection::Up, false, None, 80),
            VerticalOutcome::ReachedTop
        );
        assert_eq!(
            composer.move_vertical(VerticalDirection::Down, false, None, 80),
            VerticalOutcome::Moved
        );
        assert_eq!(composer.cursor(), 3);
        assert_eq!(
            composer.move_vertical(VerticalDirection::Down, false, None, 80),
            VerticalOutcome::ReachedBottom
        );
        assert_eq!(
            composer.move_vertical(VerticalDirection::Down, true, None, 80),
            VerticalOutcome::Unchanged
        );
    }

    #[test]
    fn page_moves_jump_by_the_visible_row_count() {
        let mut composer = composer_with("one\ntwo\nthree\nfour", 1);
        assert_eq!(
            composer.move_vertical(VerticalDirection::Down, false, Some(2), 80),
            VerticalOutcome::Moved
        );
        assert_eq!(composer.cursor(), "one\ntwo\n".len() + 1);
        assert_eq!(
            composer.visual_layout(80).point_at(composer.cursor()),
            CursorPoint {
                raw_offset: composer.cursor(),
                row_index: 2,
                content_column: 1
            }
        );
    }
}
