use super::Composer;
use super::entity_spans::Span;
use super::pasted_blocks::{PastedBlock, expand_range};
use super::text_boundaries::{logical_line_end, logical_line_start};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum YankResult {
    Inactive,
    LimitExceeded(usize),
    Inserted,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct KillRing {
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineBounds {
    pub(crate) start: usize,
    pub(crate) cursor: usize,
    pub(crate) end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeleteRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl DeleteRange {
    fn span(self) -> Span {
        Span::new(self.start, self.end)
    }
}

pub(crate) fn line_bounds(input: &str, cursor: usize) -> LineBounds {
    let cursor = cursor.min(input.len());
    LineBounds {
        start: logical_line_start(input, cursor),
        cursor,
        end: logical_line_end(input, cursor),
    }
}

fn actual_delete_range(
    pasted: &[PastedBlock],
    bounds: LineBounds,
    requested: DeleteRange,
) -> Option<DeleteRange> {
    if requested.start >= requested.end {
        return Some(requested);
    }
    let mut actual = requested;
    loop {
        let mut changed = false;
        for block in pasted {
            if block.span.overlaps(actual.span()) && !actual.span().contains_span(block.span) {
                actual.start = actual.start.min(block.span.raw_start);
                actual.end = actual.end.max(block.span.raw_end);
                if actual.start < bounds.start || actual.end > bounds.end {
                    return None;
                }
                changed = true;
            }
        }
        if !changed {
            return Some(actual);
        }
    }
}

impl Composer {
    pub(crate) fn kill_range(&mut self, bounds: LineBounds, requested: DeleteRange) -> bool {
        if requested.start >= requested.end {
            return false;
        }
        let Some(actual) = actual_delete_range(&self.entities.pasted_blocks, bounds, requested)
        else {
            return false;
        };
        let Some(payload) = expand_range(
            &self.edit.input,
            &self.entities.pasted_blocks,
            actual.start,
            actual.end,
        )
        .map(std::borrow::Cow::into_owned) else {
            return false;
        };
        self.entities.pasted_blocks.retain(|block| {
            block.span.raw_start >= block.span.raw_end || !actual.span().contains_span(block.span)
        });
        self.delete_input_range(actual.start, actual.end);
        self.limit_rejection.clear();
        self.edit.set_cursor(actual.start);
        self.kill_ring.text = payload;
        true
    }

    pub(crate) fn yank_ring(&mut self, max_len: usize) -> YankResult {
        if self.kill_ring.text.is_empty() {
            return YankResult::Inactive;
        }
        let replacement_len = self.kill_ring.text.len();
        let selection = self.edit.selection_range();
        let admitted = match selection {
            Some(range) => self.can_replace_range(range.start, range.end, replacement_len, max_len),
            None => self.can_insert(replacement_len, max_len),
        };
        if !admitted {
            return YankResult::LimitExceeded(replacement_len);
        }
        self.vertical.reset();
        if let Some(range) = selection {
            self.remove_entities_overlapping(range.start, range.end);
            self.delete_input_range(range.start, range.end);
            self.limit_rejection.clear();
            self.edit.set_cursor(range.start);
            self.edit.clear_selection();
        }
        let text = self.kill_ring.text.clone();
        self.insert_slice_without_history(&text);
        self.limit_rejection.clear();
        YankResult::Inserted
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixture::replace_text;
    use super::*;
    use crate::input::TextOwner;

    #[test]
    fn state_captures_and_yanks_a_logical_line_prefix_through_the_active_composer() {
        let mut composer = Composer::new();
        replace_text(&mut composer, "alpha\nleftMIDright");
        composer.edit.set_cursor("alpha\nleftMID".len());
        composer.vertical.preferred_column = Some(4);
        composer.note_limit_rejection(TextOwner::Composer);

        let bounds = line_bounds(composer.text(), composer.cursor());
        assert!(composer.kill_range(
            bounds,
            DeleteRange {
                start: bounds.start,
                end: bounds.cursor
            }
        ));
        assert_eq!(composer.text(), "alpha\nright");
        assert_eq!(composer.kill_ring.text, "leftMID");
        assert!(composer.note_limit_rejection(TextOwner::Composer));

        assert_eq!(composer.yank_ring(4096), YankResult::Inserted);
        assert_eq!(composer.text(), "alpha\nleftMIDright");
    }

    #[test]
    fn limit_rejection_leaves_active_composer_and_kill_state_unchanged() {
        let mut composer = Composer::new();
        composer.kill_ring.text = "XY".to_owned();
        replace_text(&mut composer, "abc");
        composer.vertical.preferred_column = Some(2);
        composer.note_limit_rejection(TextOwner::Composer);

        assert_eq!(composer.yank_ring(4), YankResult::LimitExceeded(2));
        assert_eq!(composer.text(), "abc");
        assert_eq!(composer.kill_ring.text, "XY");
        assert_eq!(composer.preferred_column(), Some(2));
        assert!(!composer.note_limit_rejection(TextOwner::Composer));
    }

    #[test]
    fn line_bounds_assign_the_cursor_to_logical_lines() {
        let cases = [
            ("", 0, (0, 0, 0)),
            ("alpha", 99, (0, 5, 5)),
            ("alpha\nbeta\ngamma", 5, (0, 5, 5)),
            ("alpha\nbeta\ngamma", 6, (6, 6, 10)),
            ("alpha\n\nbeta", 6, (6, 6, 6)),
            ("alpha\n", 6, (6, 6, 6)),
            ("\nalpha", 0, (0, 0, 0)),
            ("\nalpha", 1, (1, 1, 6)),
        ];
        for (input, cursor, (start, clamped, end)) in cases {
            assert_eq!(
                line_bounds(input, cursor),
                LineBounds {
                    start,
                    cursor: clamped,
                    end
                }
            );
        }
    }

    #[test]
    fn delete_ranges_expand_registered_entities_and_reject_cross_line_spans() {
        let blocks = [
            PastedBlock {
                id: 7,
                text: "outer".to_owned(),
                line_count: 1,
                span: Span::new(3, 12),
            },
            PastedBlock {
                id: 8,
                text: "inner".to_owned(),
                line_count: 1,
                span: Span::new(12, 20),
            },
        ];
        let expanded = actual_delete_range(
            &blocks,
            LineBounds {
                start: 0,
                cursor: 11,
                end: 20,
            },
            DeleteRange { start: 11, end: 13 },
        );
        assert_eq!(expanded, Some(DeleteRange { start: 3, end: 20 }));

        assert_eq!(
            actual_delete_range(
                &blocks[..1],
                LineBounds {
                    start: 10,
                    cursor: 11,
                    end: 20
                },
                DeleteRange { start: 11, end: 12 },
            ),
            None
        );
    }
}
