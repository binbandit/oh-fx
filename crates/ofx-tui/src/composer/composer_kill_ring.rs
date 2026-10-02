use super::Composer;
use super::edit_history::{EditHistory, Prepared};
use super::editor_state::EditorState;
use super::kill_ring::{DeleteRange, LineBounds, YankResult, line_bounds};
use super::registered_entities::Entities;
use super::text_boundaries::{is_whitespace_character_at, previous_character_start};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillKind {
    WhitespaceWordLeft,
    LineStart,
    LineEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeleteRequest {
    bounds: LineBounds,
    range: DeleteRange,
}

impl Composer {
    pub(crate) fn kill(&mut self, kind: KillKind) -> bool {
        self.vertical.reset();
        let Some(request) = delete_request(&self.edit, &self.entities, kind) else {
            return false;
        };
        let range = request.range;
        let prepared = if self
            .entities
            .entity_overlapping(range.start, range.end)
            .is_some()
        {
            Prepared::Boundary
        } else {
            EditHistory::prepare(
                range.start,
                &self.edit.input[range.start..range.end],
                "",
                self.edit.cursor,
                range.start,
            )
        };
        let changed = self.kill_range(request.bounds, range);
        if changed {
            self.edit_history.commit(prepared);
        }
        changed
    }

    pub(crate) fn yank(&mut self, max_len: usize) -> YankResult {
        let selection = self.edit.selection_range();
        let start = selection.map_or(self.edit.cursor, |range| range.start);
        let removed = selection.map_or("", |range| &self.edit.input[range.start..range.end]);
        let structured = selection.is_some_and(|range| {
            self.entities
                .entity_overlapping(range.start, range.end)
                .is_some()
        });
        let prepared = if structured {
            Prepared::Boundary
        } else {
            EditHistory::prepare(
                start,
                removed,
                &self.kill_ring.text,
                self.edit.cursor,
                start + self.kill_ring.text.len(),
            )
        };
        let result = self.yank_ring(max_len);
        if result == YankResult::Inserted {
            self.edit_history.commit(prepared);
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn kill_ring_text(&self) -> &str {
        &self.kill_ring.text
    }
}

fn delete_request(
    edit: &EditorState,
    entities: &Entities,
    kind: KillKind,
) -> Option<DeleteRequest> {
    if let Some(selection) = edit.selection_range() {
        return Some(DeleteRequest {
            bounds: LineBounds {
                start: selection.start,
                cursor: selection.end,
                end: selection.end,
            },
            range: DeleteRange {
                start: selection.start,
                end: selection.end,
            },
        });
    }
    let bounds = line_bounds(&edit.input, edit.cursor);
    let range = match kind {
        KillKind::WhitespaceWordLeft => whitespace_word_left_range(&edit.input, bounds, entities),
        KillKind::LineStart => DeleteRange {
            start: bounds.start,
            end: bounds.cursor,
        },
        KillKind::LineEnd => DeleteRange {
            start: bounds.cursor,
            end: if bounds.cursor == bounds.end && bounds.end < edit.input.len() {
                bounds.end + 1
            } else {
                bounds.end
            },
        },
    };
    (range.start < range.end).then_some(DeleteRequest { bounds, range })
}

fn whitespace_word_left_range(input: &str, bounds: LineBounds, entities: &Entities) -> DeleteRange {
    let mut start = bounds.cursor;
    while start > bounds.start {
        let previous = previous_character_start(input, start);
        if !is_whitespace_character_at(input, previous) {
            break;
        }
        start = previous;
    }
    while start > bounds.start {
        if let Some(entity) = entities.entity_ending_at(input, start) {
            start = entity.span.raw_start;
            break;
        }
        let previous = previous_character_start(input, start);
        if is_whitespace_character_at(input, previous) {
            break;
        }
        start = previous;
    }
    DeleteRange {
        start,
        end: bounds.cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::super::entity_spans::Span;
    use super::super::pasted_blocks::{PastedBlock, format_placeholder};
    use super::super::test_fixture::{replace_text, select, select_in};
    use super::super::visual_layout::VerticalDirection;
    use super::super::{DeletionKind, HistoryNavigation, SelectionRange};
    use super::*;

    fn composer_with(text: &str, cursor: usize) -> Composer {
        let mut composer = Composer::new();
        replace_text(&mut composer, text);
        composer.edit.set_cursor(cursor);
        composer
    }

    fn register_paste(composer: &mut Composer, id: usize, backing: &str) {
        let placeholder = format_placeholder(id, 1);
        let span = composer
            .text()
            .find(&placeholder)
            .map_or_else(Span::default, |start| {
                Span::new(start, start + placeholder.len())
            });
        composer.entities.register_pasted_block(PastedBlock {
            id,
            text: backing.to_owned(),
            line_count: 1,
            span,
        });
    }

    fn prime_history_draft(composer: &mut Composer, draft: &str) {
        composer.prompt_history.record(1, "history entry", &[]);
        replace_text(composer, draft);
        assert_eq!(
            composer.navigate_history(-1, usize::MAX),
            HistoryNavigation::Moved
        );
    }

    #[test]
    fn delete_requests_keep_selection_precedence_and_logical_line_ownership_pure() {
        let mut composer = composer_with("alpha\nleftMIDright\ngamma", "alpha\nleftMID".len());
        let bounds = LineBounds {
            start: "alpha\n".len(),
            cursor: "alpha\nleftMID".len(),
            end: "alpha\nleftMIDright".len(),
        };
        assert_eq!(
            delete_request(&composer.edit, &composer.entities, KillKind::LineStart),
            Some(DeleteRequest {
                bounds,
                range: DeleteRange {
                    start: "alpha\n".len(),
                    end: "alpha\nleftMID".len()
                }
            })
        );
        assert_eq!(
            delete_request(&composer.edit, &composer.entities, KillKind::LineEnd),
            Some(DeleteRequest {
                bounds,
                range: DeleteRange {
                    start: "alpha\nleftMID".len(),
                    end: "alpha\nleftMIDright".len()
                }
            })
        );

        select_in(&mut composer.edit, 14, 8);
        for kind in [
            KillKind::WhitespaceWordLeft,
            KillKind::LineStart,
            KillKind::LineEnd,
        ] {
            assert_eq!(
                delete_request(&composer.edit, &composer.entities, kind),
                Some(DeleteRequest {
                    bounds: LineBounds {
                        start: 8,
                        cursor: 14,
                        end: 14
                    },
                    range: DeleteRange { start: 8, end: 14 }
                })
            );
        }
    }

    #[test]
    fn plain_kill_and_yank_each_record_one_edit_history_delta() {
        let mut composer = composer_with("foo-bar  ", "foo-bar  ".len());
        assert!(composer.kill(KillKind::WhitespaceWordLeft));
        assert_eq!(composer.text(), "");
        assert_eq!(composer.kill_ring_text(), "foo-bar  ");
        assert_eq!(
            composer.edit_history.peek_undo().unwrap().removed,
            "foo-bar  "
        );

        composer.edit_history.reset();
        assert_eq!(composer.yank(4096), YankResult::Inserted);
        assert_eq!(composer.text(), "foo-bar  ");
        assert_eq!(
            composer.edit_history.peek_undo().unwrap().inserted,
            "foo-bar  "
        );
    }

    #[test]
    fn structured_kill_and_yank_remain_edit_history_boundaries() {
        let placeholder = format_placeholder(1, 1);
        let mut composer = composer_with(&placeholder, placeholder.len());
        register_paste(&mut composer, 1, "pasted");

        composer
            .edit_history
            .commit(EditHistory::prepare(0, "", "x", 0, 1));
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.edit_history.peek_undo(), None);

        select(&mut composer, 0, 0);
        composer.insert_slice(&placeholder);
        register_paste(&mut composer, 1, "pasted");
        select(&mut composer, 0, placeholder.len());
        composer
            .edit_history
            .commit(EditHistory::prepare(0, "", "x", 0, 1));
        assert_eq!(composer.yank(4096), YankResult::Inserted);
        assert_eq!(composer.edit_history.peek_undo(), None);
    }

    #[test]
    fn word_and_line_deletion_families_remove_forward_and_reverse_selections_first() {
        enum Family {
            Delete(DeletionKind),
            Kill(KillKind),
        }
        for reverse in [false, true] {
            for family in [
                Family::Delete(DeletionKind::WordLeft),
                Family::Kill(KillKind::WhitespaceWordLeft),
                Family::Delete(DeletionKind::WordRight),
                Family::Kill(KillKind::LineStart),
                Family::Kill(KillKind::LineEnd),
            ] {
                let mut composer = Composer::new();
                replace_text(&mut composer, "alpha beta");
                if reverse {
                    select(&mut composer, 8, 2);
                } else {
                    select(&mut composer, 2, 8);
                }
                let changed = match family {
                    Family::Delete(kind) => composer.delete(kind),
                    Family::Kill(kind) => composer.kill(kind),
                };
                assert!(changed);
                assert_eq!(composer.text(), "alta");
                assert_eq!(composer.cursor(), 2);
                assert_eq!(composer.selection(), None);
                assert!(composer.undo());
                assert_eq!(composer.text(), "alpha beta");
            }
        }
    }

    #[test]
    fn delete_whitespace_delimited_word_left_kills_a_token_and_preserves_history_draft() {
        let mut composer = Composer::new();
        prime_history_draft(&mut composer, "draft");
        replace_text(&mut composer, "foo-bar  ");
        composer.vertical.preferred_column = Some(7);

        assert!(composer.kill(KillKind::WhitespaceWordLeft));
        assert_eq!(composer.text(), "");
        assert_eq!(composer.kill_ring_text(), "foo-bar  ");
        assert_eq!(composer.prompt_history.active_index(), Some(0));
        assert_eq!(composer.prompt_history.draft_text(), Some("draft"));
        assert_eq!(composer.preferred_column(), None);
        assert_eq!(composer.yank(4096), YankResult::Inserted);
        assert_eq!(composer.text(), "foo-bar  ");
        assert_eq!(composer.prompt_history.active_index(), Some(0));
        assert_eq!(composer.prompt_history.draft_text(), Some("draft"));
    }

    #[test]
    fn bounded_composer_inserts_and_yank_reject_without_changing_the_composer() {
        let mut composer = composer_with("abcd", 2);
        composer.kill_ring.text = "XY".to_owned();
        composer.vertical.preferred_column = Some(1);
        assert_eq!(
            composer.insert_slice_bounded("!", 4),
            super::super::InsertResult::LimitExceeded
        );
        assert_eq!(
            composer.insert_slice_bounded("XY", 5),
            super::super::InsertResult::LimitExceeded
        );
        assert_eq!(composer.yank(5), YankResult::LimitExceeded(2));
        assert_eq!(composer.text(), "abcd");
        assert_eq!(composer.cursor(), 2);
        assert_eq!(composer.kill_ring_text(), "XY");
        assert_eq!(composer.preferred_column(), Some(1));
        assert_eq!(composer.edit_history.peek_undo(), None);
    }

    #[test]
    fn kills_clear_vertical_intent_on_a_no_op() {
        for kind in [
            KillKind::WhitespaceWordLeft,
            KillKind::LineStart,
            KillKind::LineEnd,
        ] {
            let mut composer = Composer::new();
            composer.kill_ring.text = "existing kill".to_owned();
            composer.vertical.preferred_column = Some(7);
            assert!(!composer.kill(kind));
            assert_eq!(composer.text(), "");
            assert_eq!(composer.kill_ring_text(), "existing kill");
            assert_eq!(composer.preferred_column(), None);
        }
    }

    #[test]
    fn line_kills_end_the_vertical_motion() {
        let mut composer = composer_with(
            "abcdefghij\nab\nabcdefghij\nabcdefghij",
            "abcdefghij\nab\nabcdefgh".len(),
        );
        composer.move_vertical(VerticalDirection::Up, true, None, 80);
        assert_eq!(
            composer.selection(),
            Some(SelectionRange {
                start: "abcdefghij\nab".len(),
                end: "abcdefghij\nab\nabcdefgh".len()
            })
        );
        assert!(composer.kill(KillKind::LineEnd));
        assert_eq!(composer.text(), "abcdefghij\nabij\nabcdefghij");
        assert_eq!(composer.preferred_column(), None);
        composer.move_vertical(VerticalDirection::Down, false, None, 80);
        assert_eq!(composer.cursor(), "abcdefghij\nabij\nab".len());

        let mut composer = composer_with("abcdefghij\nab\nabcdefghij", "abcdefgh".len());
        composer.move_vertical(VerticalDirection::Down, false, None, 80);
        assert_eq!(composer.cursor(), "abcdefghij\nab".len());
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), "abcdefghij\n\nabcdefghij");
        assert_eq!(composer.preferred_column(), None);
        composer.move_vertical(VerticalDirection::Down, false, None, 80);
        assert_eq!(composer.cursor(), "abcdefghij\n\n".len());
    }

    #[test]
    fn delete_to_line_start_removes_only_the_current_logical_line_prefix() {
        let mut composer = composer_with("alpha\nleftMIDright\ngamma", "alpha\nleftMID".len());
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), "alpha\nright\ngamma");
        assert_eq!(composer.cursor(), "alpha\n".len());
        assert_eq!(composer.kill_ring_text(), "leftMID");
    }

    #[test]
    fn delete_to_line_end_removes_only_the_current_logical_line_suffix() {
        let mut composer = composer_with("alpha\nleftMIDright\ngamma", "alpha\nleftMID".len());
        assert!(composer.kill(KillKind::LineEnd));
        assert_eq!(composer.text(), "alpha\nleftMID\ngamma");
        assert_eq!(composer.cursor(), "alpha\nleftMID".len());
        assert_eq!(composer.kill_ring_text(), "right");
    }

    #[test]
    fn line_deletion_boundary_no_ops_preserve_the_kill_ring() {
        let mut composer = composer_with("alpha\nbeta", "alpha\n".len());
        composer.kill_ring.text = "existing kill".to_owned();
        assert!(!composer.kill(KillKind::LineStart));
        assert_eq!(composer.kill_ring_text(), "existing kill");
        composer.edit.set_cursor(composer.text().len());
        assert!(!composer.kill(KillKind::LineEnd));
        assert_eq!(composer.kill_ring_text(), "existing kill");
    }

    #[test]
    fn delete_to_line_end_at_non_final_eol_joins_the_next_logical_line() {
        let mut composer = composer_with("alpha\nbeta\ngamma", "alpha\nbeta".len());
        assert!(composer.kill(KillKind::LineEnd));
        assert_eq!(composer.text(), "alpha\nbetagamma");
        assert_eq!(composer.cursor(), "alpha\nbeta".len());
        assert_eq!(composer.kill_ring_text(), "\n");
    }

    #[test]
    fn line_deletion_preserves_newline_ownership_and_utf8_outside_the_range() {
        let mut composer = composer_with("alpha\nbeta\ngamma", "alpha\nbeta".len());
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), "alpha\n\ngamma");
        assert_eq!(composer.cursor(), "alpha\n".len());

        let mut composer = composer_with("alpha\nbeta\ngamma", "alpha\n".len());
        assert!(composer.kill(KillKind::LineEnd));
        assert_eq!(composer.text(), "alpha\n\ngamma");
        assert_eq!(composer.cursor(), "alpha\n".len());

        let mut composer =
            composer_with("\u{e9}cho\nna\u{ef}ve😀\n尾", "\u{e9}cho\nna\u{ef}ve".len());
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), "\u{e9}cho\n😀\n尾");
    }

    #[test]
    fn line_deletion_no_op_preserves_input_state_and_metadata() {
        let mut composer = Composer::new();
        prime_history_draft(&mut composer, "draft");
        let placeholder = format_placeholder(7, 1);
        replace_text(&mut composer, &format!("alpha\n{placeholder}"));
        composer.edit.set_cursor("alpha\n".len());
        composer.kill_ring.text = "existing kill".to_owned();
        register_paste(&mut composer, 7, "pasted");
        assert!(!composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), format!("alpha\n{placeholder}"));
        assert_eq!(composer.kill_ring_text(), "existing kill");
        assert_eq!(composer.entities.pasted_blocks.len(), 1);
        assert_eq!(composer.prompt_history.active_index(), Some(0));
        assert_eq!(composer.prompt_history.draft_text(), Some("draft"));
    }

    #[test]
    fn line_deletion_no_ops_on_empty_logical_lines_and_clamps_an_oversized_cursor() {
        for (input, cursor) in [("", 0), ("alpha\n", "alpha\n".len())] {
            let mut composer = composer_with(input, cursor);
            composer.kill_ring.text = "existing kill".to_owned();
            assert!(!composer.kill(KillKind::LineStart));
            assert!(!composer.kill(KillKind::LineEnd));
            assert_eq!(composer.text(), input);
            assert_eq!(composer.kill_ring_text(), "existing kill");
        }
        let mut composer = composer_with("alpha", 99);
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), "");
        assert_eq!(composer.cursor(), 0);
    }

    #[test]
    fn successful_line_deletion_preserves_history_draft() {
        let mut composer = Composer::new();
        prime_history_draft(&mut composer, "draft");
        replace_text(&mut composer, "alpha\nleftMIDright\ngamma");
        composer.edit.set_cursor("alpha\nleftMID".len());
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.prompt_history.active_index(), Some(0));
        assert_eq!(composer.prompt_history.draft_text(), Some("draft"));
    }

    #[test]
    fn registered_placeholders_expand_line_deletion_atomically_and_drop_matching_metadata() {
        let placeholder = format_placeholder(7, 1);
        let mut composer = composer_with(&format!("alpha\npre{placeholder}post\ngamma"), 0);
        register_paste(&mut composer, 7, "pasted");
        composer.edit.set_cursor("alpha\npre[Pasted text #7".len());
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), "alpha\npost\ngamma");
        assert_eq!(composer.kill_ring_text(), "prepasted");
        assert!(composer.entities.pasted_blocks.is_empty());
        assert_eq!(composer.yank(4096), YankResult::Inserted);
        assert_eq!(composer.text(), "alpha\nprepastedpost\ngamma");
    }

    #[test]
    fn same_id_surviving_placeholder_preserves_backing_metadata() {
        let placeholder = format_placeholder(7, 1);
        let text = format!("{placeholder}\n{placeholder}");
        let mut composer = composer_with(&text, placeholder.len());
        register_paste(&mut composer, 7, "pasted");
        composer.entities.pasted_blocks[0].span = Span::new(placeholder.len() + 1, text.len());
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), format!("\n{placeholder}"));
        assert_eq!(composer.entities.pasted_blocks.len(), 1);
    }

    #[test]
    fn unregistered_and_truncated_placeholder_lookalikes_remain_ordinary_text() {
        let mut composer = composer_with(
            "pre[Pasted text #99, 1 line]post",
            "pre[Pasted text #99".len(),
        );
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), ", 1 line]post");

        let mut composer = composer_with(
            "pre[Pasted text #7, 2 lin",
            "pre[Pasted text #7, 2 lin".len(),
        );
        register_paste(&mut composer, 7, "pasted");
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), "");
        assert_eq!(composer.entities.pasted_blocks.len(), 1);
    }

    #[test]
    fn placeholder_boundary_touching_does_not_expand_the_deletion_range() {
        let token = format_placeholder(7, 1);
        let mut composer = composer_with(&format!("pre{token}post"), 3);
        register_paste(&mut composer, 7, "pasted");
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), format!("{token}post"));
        assert_eq!(composer.entities.pasted_blocks.len(), 1);

        let mut composer = composer_with(&format!("pre{token}post"), 3 + token.len());
        register_paste(&mut composer, 7, "pasted");
        assert!(composer.kill(KillKind::LineEnd));
        assert_eq!(composer.text(), format!("pre{token}"));
        assert_eq!(composer.entities.pasted_blocks.len(), 1);
    }

    #[test]
    fn registered_pasted_placeholder_crossing_a_newline_makes_line_deletion_a_no_op() {
        let text = "pre[Pasted text #7,\n1 line]post";
        let mut composer = composer_with(text, 3);
        composer.entities.register_pasted_block(PastedBlock {
            id: 7,
            text: "pasted".to_owned(),
            line_count: 1,
            span: Span::new(3, "pre[Pasted text #7,\n1 line]".len()),
        });
        composer.kill_ring.text = "existing kill".to_owned();
        assert!(!composer.kill(KillKind::LineEnd));
        assert_eq!(composer.text(), text);
        assert_eq!(composer.kill_ring_text(), "existing kill");
        assert_eq!(composer.entities.pasted_blocks.len(), 1);
    }

    #[test]
    fn ordinary_line_deletion_keeps_orphan_backing_records() {
        let mut composer = composer_with("alpha\nleftMIDright\ngamma", "alpha\nleftMID".len());
        for id in 1..=1024 {
            register_paste(&mut composer, id, "x");
        }
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.entities.pasted_blocks.len(), 1024);
    }

    #[test]
    fn line_deletion_expands_and_removes_only_the_registered_same_id_placeholder() {
        let placeholder = format_placeholder(7, 1);
        let text = format!("{placeholder}{placeholder}");
        let mut composer = composer_with(&text, text.len());
        register_paste(&mut composer, 7, "pasted");
        assert!(composer.kill(KillKind::LineStart));
        assert_eq!(composer.text(), "");
        assert_eq!(composer.kill_ring_text(), format!("pasted{placeholder}"));
        assert!(composer.entities.pasted_blocks.is_empty());
    }

    #[test]
    fn known_id_pasted_scans_reject_dense_different_id_prefixes() {
        let mut text = format_placeholder(7, 1);
        text.extend((0..128).map(|suffix| format!("[Pasted text #7{suffix}, nested ")));
        text.push(']');
        let mut composer = composer_with(&text, text.len());
        register_paste(&mut composer, 7, "pasted");
        assert!(composer.kill(KillKind::LineStart));
        let lookalikes = &text[format_placeholder(7, 1).len()..];
        assert_eq!(composer.kill_ring_text(), format!("pasted{lookalikes}"));
        assert!(composer.entities.pasted_blocks.is_empty());
    }
}
